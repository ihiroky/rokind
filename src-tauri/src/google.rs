use std::{collections::HashMap, net::SocketAddr};

use anyhow::{anyhow, Context, Result};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use rand::{distributions::Alphanumeric, Rng};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener as TokioTcpListener,
};
use url::Url;

use crate::state::{
    AppStateStore, AuthCallback, CalendarEventSummary, GoogleOAuthConfig, TokenState,
};

static URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"https?://[^\s<>()"]+"#).expect("valid URL regex"));

const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
const EVENTS_LOOKAHEAD_HOURS: i64 = 12;
const EVENTS_MAX_RESULTS: &str = "250";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsResponse {
    items: Vec<GoogleCalendarEvent>,
}

#[derive(Debug, Deserialize)]
struct GoogleCalendarEvent {
    id: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    #[serde(rename = "hangoutLink")]
    hangout_link: Option<String>,
    #[serde(rename = "conferenceData")]
    conference_data: Option<ConferenceData>,
    start: Option<EventDateTime>,
    end: Option<EventDateTime>,
}

#[derive(Debug, Deserialize)]
struct ConferenceData {
    #[serde(rename = "entryPoints")]
    entry_points: Option<Vec<EntryPoint>>,
}

#[derive(Debug, Deserialize)]
struct EntryPoint {
    uri: Option<String>,
    #[serde(rename = "entryPointType")]
    entry_point_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventDateTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
}

pub(crate) async fn oauth_config_diagnostics(app: &AppHandle) -> String {
    let stored = stored_google_oauth_config(app).await;
    let initial = initial_google_oauth_config();

    let id_state = if let Some(stored) = stored.as_ref() {
        if trimmed_config_value(stored.client_id.as_deref().unwrap_or_default()).is_some() {
            "GOOGLE_CLIENT_ID: saved"
        } else {
            "GOOGLE_CLIENT_ID: saved empty"
        }
    } else if initial.client_id.is_some() {
        "GOOGLE_CLIENT_ID: initial from build"
    } else {
        "GOOGLE_CLIENT_ID: missing"
    };

    let secret_state = if let Some(stored) = stored.as_ref() {
        if trimmed_config_value(stored.client_secret.as_deref().unwrap_or_default()).is_some() {
            "GOOGLE_CLIENT_SECRET: saved"
        } else {
            "GOOGLE_CLIENT_SECRET: saved empty"
        }
    } else if initial.client_secret.is_some() {
        "GOOGLE_CLIENT_SECRET: initial from build"
    } else {
        "GOOGLE_CLIENT_SECRET: missing"
    };

    let source = option_env!("ROKIND_OAUTH_CONFIG_SOURCE").unwrap_or("build-time source: unknown");
    format!("{source} / {id_state} / {secret_state}")
}

fn env_config_value(name: &str) -> Option<String> {
    let value = match name {
        "GOOGLE_CLIENT_ID" => option_env!("GOOGLE_CLIENT_ID"),
        "GOOGLE_CLIENT_SECRET" => option_env!("GOOGLE_CLIENT_SECRET"),
        _ => None,
    }?;

    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn initial_google_oauth_config() -> GoogleOAuthConfig {
    GoogleOAuthConfig {
        client_id: env_config_value("GOOGLE_CLIENT_ID"),
        client_secret: env_config_value("GOOGLE_CLIENT_SECRET"),
    }
}

pub(crate) fn effective_google_oauth_config(
    stored: Option<&GoogleOAuthConfig>,
) -> GoogleOAuthConfig {
    if let Some(stored) = stored {
        return GoogleOAuthConfig {
            client_id: trimmed_config_value(stored.client_id.as_deref().unwrap_or_default()),
            client_secret: trimmed_config_value(
                stored.client_secret.as_deref().unwrap_or_default(),
            ),
        };
    }

    initial_google_oauth_config()
}

pub(crate) async fn stored_google_oauth_config(app: &AppHandle) -> Option<GoogleOAuthConfig> {
    let state = app.state::<AppStateStore>();
    let persistent = state.persistent.lock().await;
    persistent.google_oauth_config.clone()
}

pub(crate) async fn google_oauth_config(app: &AppHandle) -> GoogleOAuthConfig {
    let stored = stored_google_oauth_config(app).await;
    effective_google_oauth_config(stored.as_ref())
}

pub(crate) fn trimmed_config_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) async fn ensure_access_token(
    client_id: &str,
    client_secret: Option<&str>,
    token: TokenState,
) -> Result<TokenState> {
    if token.access_token_is_fresh() {
        return Ok(token);
    }

    let Some(refresh_token) = token.refresh_token.clone() else {
        return Err(anyhow!(
            "refresh token is missing; please reconnect Google Calendar"
        ));
    };

    let client = Client::new();
    let mut form = vec![
        ("client_id", client_id.to_string()),
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.clone()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.to_string()));
    }

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&form)
        .send()
        .await
        .context("failed to refresh Google OAuth token")?;

    if !response.status().is_success() {
        let message = parse_google_error(response).await;
        return Err(anyhow!("failed to refresh Google token: {message}"));
    }

    let refreshed: TokenResponse = response
        .json()
        .await
        .context("failed to decode Google token refresh response")?;

    Ok(TokenState {
        access_token: refreshed.access_token,
        refresh_token: Some(refresh_token),
        expires_at: Utc::now() + ChronoDuration::seconds(refreshed.expires_in),
    })
}

pub(crate) fn error_requires_reconnect(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("refresh token is missing")
        || normalized.contains("invalid_grant")
        || normalized.contains("expired or revoked")
        || normalized.contains("invalid authentication credentials")
}

pub(crate) async fn fetch_calendar_events(access_token: &str) -> Result<Vec<CalendarEventSummary>> {
    let now = Utc::now();
    let time_min = now;
    let time_max = now + ChronoDuration::hours(EVENTS_LOOKAHEAD_HOURS);

    let url = Url::parse_with_params(
        "https://www.googleapis.com/calendar/v3/calendars/primary/events",
        &[
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", EVENTS_MAX_RESULTS),
            ("conferenceDataVersion", "1"),
            (
                "timeMin",
                &time_min.to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
            (
                "timeMax",
                &time_max.to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
        ],
    )?;

    let client = Client::new();
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .context("failed to request Google Calendar events")?;

    if !response.status().is_success() {
        let message = parse_google_error(response).await;
        return Err(anyhow!("failed to load Google Calendar events: {message}"));
    }

    let payload: EventsResponse = response
        .json()
        .await
        .context("failed to decode Google Calendar events response")?;

    Ok(payload
        .items
        .into_iter()
        .filter_map(convert_event)
        .collect())
}

fn convert_event(event: GoogleCalendarEvent) -> Option<CalendarEventSummary> {
    let meeting_url = extract_meeting_url(&event);
    let id = event.id?;
    let start_at = event.start?.date_time?;
    let end_at = event.end.and_then(|end| end.date_time);

    Some(CalendarEventSummary {
        id,
        title: event.summary.unwrap_or_else(|| "No title".to_string()),
        start_at,
        end_at,
        location: event.location.clone(),
        meeting_url,
    })
}

fn extract_meeting_url(event: &GoogleCalendarEvent) -> Option<String> {
    if let Some(url) = event.hangout_link.clone() {
        return Some(url);
    }

    if let Some(conference) = &event.conference_data {
        if let Some(entry_points) = &conference.entry_points {
            for entry in entry_points {
                let prefers = matches!(
                    entry.entry_point_type.as_deref(),
                    Some("video") | Some("more")
                );
                if prefers {
                    if let Some(url) = entry.uri.clone() {
                        return Some(url);
                    }
                }
            }
        }
    }

    for text in [&event.location, &event.description] {
        if let Some(text) = text {
            if let Some(found) = URL_REGEX.find(text) {
                return Some(
                    found
                        .as_str()
                        .trim_end_matches(|ch: char| [')', ']', '.', ','].contains(&ch))
                        .to_string(),
                );
            }
        }
    }

    None
}

pub(crate) async fn parse_google_error(response: reqwest::Response) -> String {
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(_) => return format!("HTTP {status} (response body could not be read)"),
    };

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("HTTP {status} (empty response body)");
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(message) = json
            .get("error")
            .and_then(|value| value.get("message"))
            .and_then(|value| value.as_str())
        {
            return format!("HTTP {status}: {message}");
        }

        let error_code = json.get("error").and_then(|value| value.as_str());
        let error_description = json
            .get("error_description")
            .and_then(|value| value.as_str());

        match (error_code, error_description) {
            (Some(code), Some(description)) => {
                return format!("HTTP {status}: {code} ({description})");
            }
            (Some(code), None) => {
                return format!("HTTP {status}: {code}");
            }
            (None, Some(description)) => {
                return format!("HTTP {status}: {description}");
            }
            (None, None) => {}
        }
    }

    format!("HTTP {status}: {trimmed}")
}

pub(crate) fn random_token(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

pub(crate) fn build_auth_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String> {
    let url = Url::parse_with_params(
        "https://accounts.google.com/o/oauth2/v2/auth",
        &[
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", GOOGLE_SCOPE),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("include_granted_scopes", "true"),
            ("state", state),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
        ],
    )?;

    Ok(url.to_string())
}

pub(crate) async fn wait_for_auth_callback(listener: TokioTcpListener) -> Result<AuthCallback> {
    let (mut stream, socket_addr) = listener
        .accept()
        .await
        .context("failed to receive OAuth callback")?;
    validate_loopback(socket_addr)?;

    let mut buffer = vec![0_u8; 4096];
    let count = stream
        .read(&mut buffer)
        .await
        .context("failed to read OAuth callback request")?;
    let request = String::from_utf8_lossy(&buffer[..count]);
    let request_target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("failed to parse OAuth callback request line"))?;

    let callback_url = Url::parse(&format!("http://127.0.0.1{request_target}"))?;
    let query = callback_url.query_pairs().collect::<HashMap<_, _>>();
    let code = query
        .get("code")
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("OAuth callback did not include a code"))?;
    let state = query
        .get("state")
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("OAuth callback did not include a state"))?;

    let body = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>Connected</title><body style="font-family:sans-serif;padding:24px">Google Calendar の接続が完了しました。アプリへ戻ってください。</body></html>"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write OAuth callback response")?;

    Ok(AuthCallback { code, state })
}

fn validate_loopback(addr: SocketAddr) -> Result<()> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(anyhow!(
            "received OAuth callback from a non-loopback address"
        ))
    }
}

pub(crate) async fn exchange_authorization_code(
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenState> {
    let client = Client::new();
    let mut form = vec![
        ("client_id", client_id.to_string()),
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_verifier", code_verifier.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.to_string()));
    }

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&form)
        .send()
        .await
        .context("failed to exchange authorization code")?;

    if !response.status().is_success() {
        let message = parse_google_error(response).await;
        return Err(anyhow!("failed to exchange authorization code: {message}"));
    }

    let token_response: TokenResponse = response
        .json()
        .await
        .context("failed to decode Google token response")?;

    Ok(TokenState {
        access_token: token_response.access_token,
        refresh_token: token_response.refresh_token,
        expires_at: Utc::now() + ChronoDuration::seconds(token_response.expires_in),
    })
}
