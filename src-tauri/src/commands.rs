use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener as TokioTcpListener;

use crate::{
    google,
    reminders::{
        AuthReconnectNoticePayload, AuthReconnectNoticeState, EventReminderPayload,
        EventWindowDisplayState,
    },
    state::{
        AppStateStore, AppStatus, AuthCallback, CommandResult, GoogleOAuthConfig,
        OAuthStartResponse, PendingAuth, TokenState,
    },
    storage::{persist_state, record_error},
    windows::{
        clear_auth_reconnect_notice, close_all_auth_reconnect_notice_windows,
        close_event_reminder_windows, show_main_window,
    },
};

pub(crate) async fn emit_app_status_updated(app: &AppHandle) {
    if let Ok(status) = get_app_status(app.clone()).await {
        let _ = app.emit("app-status-updated", status);
    }
}

async fn fail_auth(app: &AppHandle, message: &str) -> String {
    record_error(app, message).await;
    let _ = app.emit("auth-flow-failed", message.to_string());
    message.to_string()
}

async fn clear_pending_auth(app: &AppHandle, expected_state: &str) {
    let state = app.state::<AppStateStore>();
    let mut pending_auth = state.pending_auth.lock().await;
    if pending_auth
        .as_ref()
        .is_some_and(|pending| pending.state == expected_state)
    {
        *pending_auth = None;
    }
}

async fn pending_auth_matches(app: &AppHandle, expected_state: &str) -> bool {
    let state = app.state::<AppStateStore>();
    let pending_auth = state.pending_auth.lock().await;
    pending_auth
        .as_ref()
        .is_some_and(|pending| pending.state == expected_state)
}

async fn complete_google_auth(
    app: &AppHandle,
    pending: &PendingAuth,
    callback: AuthCallback,
) -> Result<()> {
    if callback.state != pending.state {
        return Err(anyhow!("OAuth state が一致しませんでした"));
    }

    let token = google::exchange_authorization_code(
        &pending.client_id,
        pending.client_secret.as_deref(),
        &callback.code,
        &pending.redirect_uri,
        &pending.code_verifier,
    )
    .await?;

    {
        let state = app.state::<AppStateStore>();
        let mut persistent = state.persistent.lock().await;
        persistent.token = Some(token);
        persistent.last_error = None;
    }

    persist_state(app).await?;
    clear_auth_reconnect_notice(app).await;
    crate::reminders::sync_calendar_and_maybe_notify(app).await?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn get_app_status(app: AppHandle) -> CommandResult<AppStatus> {
    let state = app.state::<AppStateStore>();
    let persistent = state.persistent.lock().await.clone();
    let auth_in_progress = state.pending_auth.lock().await.is_some();
    let initial = google::initial_google_oauth_config();
    let oauth_config =
        google::effective_google_oauth_config(persistent.google_oauth_config.as_ref());
    let client_id_configured = oauth_config.client_id.is_some();
    let client_secret_configured = oauth_config.client_secret.is_some();
    let can_start_google_auth = client_id_configured && !auth_in_progress;
    let signed_in = persistent
        .token
        .as_ref()
        .is_some_and(|token| token.has_access_token() && token.has_refresh_token());
    let auto_reconnect_ready = persistent
        .token
        .as_ref()
        .is_some_and(TokenState::has_refresh_token);

    Ok(AppStatus {
        client_id_configured,
        client_secret_configured,
        google_client_id: oauth_config.client_id,
        google_client_secret: oauth_config.client_secret,
        google_client_id_using_initial: persistent.google_oauth_config.is_none()
            && initial.client_id.is_some(),
        google_client_secret_using_initial: persistent.google_oauth_config.is_none()
            && initial.client_secret.is_some(),
        signed_in,
        polling_enabled: client_id_configured && signed_in,
        auth_in_progress,
        can_start_google_auth,
        auto_reconnect_ready,
        oauth_config_diagnostics: google::oauth_config_diagnostics(&app).await,
        upcoming_events: persistent.cached_events,
        last_sync_at: persistent.last_sync_at.map(|value| value.to_rfc3339()),
        last_error: persistent.last_error,
    })
}

#[tauri::command]
pub(crate) async fn save_google_oauth_settings(
    app: AppHandle,
    client_id: String,
    client_secret: String,
) -> CommandResult<AppStatus> {
    let next_stored = GoogleOAuthConfig {
        client_id: Some(client_id.trim().to_string()),
        client_secret: Some(client_secret.trim().to_string()),
    };
    apply_google_oauth_settings(app, next_stored).await
}

async fn apply_google_oauth_settings(
    app: AppHandle,
    next_stored: GoogleOAuthConfig,
) -> CommandResult<AppStatus> {
    let previous_effective = google::google_oauth_config(&app).await;
    let next_effective = google::effective_google_oauth_config(Some(&next_stored));
    let oauth_changed = previous_effective != next_effective;

    {
        let state = app.state::<AppStateStore>();
        let mut persistent = state.persistent.lock().await;
        persistent.google_oauth_config = Some(next_stored);
        persistent.last_error = None;

        if oauth_changed {
            persistent.token = None;
            persistent.cached_events.clear();
            persistent.last_sync_at = None;
        }
    }

    if oauth_changed {
        crate::reminders::clear_google_auth_runtime_state(&app).await;
    }

    persist_state(&app)
        .await
        .map_err(|error| error.to_string())?;
    let status = get_app_status(app.clone()).await?;
    let _ = app.emit("app-status-updated", status.clone());
    Ok(status)
}

#[tauri::command]
pub(crate) async fn start_google_auth(app: AppHandle) -> CommandResult<OAuthStartResponse> {
    let oauth_config = google::google_oauth_config(&app).await;
    let client_id = oauth_config
        .client_id
        .ok_or_else(|| "GOOGLE_CLIENT_ID を .env に設定してください".to_string())?;

    {
        let shared = app.state::<AppStateStore>();
        let pending_auth = shared.pending_auth.lock().await;
        if let Some(existing) = pending_auth.as_ref() {
            return Ok(OAuthStartResponse {
                auth_url: existing.auth_url.clone(),
            });
        }
    }

    let listener = TokioTcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();

    let code_verifier = google::random_token(64);
    let state_token = google::random_token(32);
    let challenge_digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(challenge_digest);
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
    let auth_url = google::build_auth_url(&client_id, &redirect_uri, &state_token, &code_challenge)
        .map_err(|error| error.to_string())?;
    let pending = PendingAuth {
        state: state_token,
        code_verifier,
        redirect_uri,
        auth_url: auth_url.clone(),
        client_id,
        client_secret: oauth_config.client_secret,
    };

    {
        let shared = app.state::<AppStateStore>();
        let mut pending_auth = shared.pending_auth.lock().await;
        *pending_auth = Some(pending.clone());
    }

    {
        let shared = app.state::<AppStateStore>();
        let mut persistent = shared.persistent.lock().await;
        persistent.last_error = None;
    }
    if let Err(error) = persist_state(&app).await {
        clear_pending_auth(&app, &pending.state).await;
        return Err(error.to_string());
    }

    let app_handle = app.clone();
    let pending_for_task = pending.clone();
    tauri::async_runtime::spawn(async move {
        let result = async {
            let callback = google::wait_for_auth_callback(listener).await?;
            if !pending_auth_matches(&app_handle, &pending_for_task.state).await {
                return Ok(());
            }
            complete_google_auth(&app_handle, &pending_for_task, callback).await
        }
        .await;

        match result {
            Ok(()) => {
                clear_pending_auth(&app_handle, &pending_for_task.state).await;
                let _ = app_handle.emit("auth-flow-completed", ());
            }
            Err(error) => {
                clear_pending_auth(&app_handle, &pending_for_task.state).await;
                let _ = fail_auth(&app_handle, &error.to_string()).await;
            }
        }
    });

    Ok(OAuthStartResponse { auth_url })
}

#[tauri::command]
pub(crate) async fn disconnect_google(app: AppHandle) -> CommandResult<AppStatus> {
    crate::reminders::reset_google_auth_state(&app, None)
        .await
        .map_err(|error| error.to_string())?;
    let status = get_app_status(app.clone()).await?;
    let _ = app.emit("app-status-updated", status.clone());
    Ok(status)
}

#[tauri::command]
pub(crate) async fn refresh_events(app: AppHandle) -> CommandResult<AppStatus> {
    crate::reminders::sync_calendar_and_maybe_notify(&app)
        .await
        .map_err(|error| error.to_string())?;
    get_app_status(app).await
}

#[tauri::command]
pub(crate) async fn dismiss_event_reminder(app: AppHandle, event_id: String) -> CommandResult<()> {
    let state = app.state::<AppStateStore>();
    let now = Utc::now();

    {
        let mut reminder_states = state.event_reminder_states.lock().await;
        if let Some(record) = reminder_states.get_mut(&event_id) {
            record.display_state = if record.start_at <= now {
                EventWindowDisplayState::DismissedAfterStart
            } else {
                EventWindowDisplayState::DismissedBeforeStart
            };
        }
    }

    close_event_reminder_windows(&app, &event_id)
        .await
        .map_err(|e| e.to_string())?;

    emit_app_status_updated(&app).await;
    Ok(())
}

#[tauri::command]
pub(crate) async fn get_event_reminder(
    app: AppHandle,
    event_id: String,
) -> CommandResult<Option<EventReminderPayload>> {
    let state = app.state::<AppStateStore>();
    let reminder_states = state.event_reminder_states.lock().await;
    Ok(reminder_states.get(&event_id).map(|r| r.to_payload()))
}

#[tauri::command]
pub(crate) async fn get_auth_reconnect_notice(
    app: AppHandle,
) -> CommandResult<Option<AuthReconnectNoticePayload>> {
    let state = app.state::<AppStateStore>();
    let notice = state.auth_reconnect_notice.lock().await;
    Ok(notice.as_ref().map(AuthReconnectNoticeState::to_payload))
}

#[tauri::command]
pub(crate) async fn dismiss_auth_reconnect_notice(app: AppHandle) -> CommandResult<()> {
    close_all_auth_reconnect_notice_windows(&app).await;
    Ok(())
}

#[tauri::command]
pub(crate) async fn open_main_window_for_reconnect(app: AppHandle) -> CommandResult<()> {
    show_main_window(&app).map_err(|error| error.to_string())?;
    close_all_auth_reconnect_notice_windows(&app).await;
    Ok(())
}
