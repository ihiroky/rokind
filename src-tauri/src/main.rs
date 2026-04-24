#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use rand::{distributions::Alphanumeric, Rng};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalPosition, Manager, Position, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener as TokioTcpListener,
    sync::Mutex,
};
use url::Url;

static URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"https?://[^\s<>()"]+"#).expect("valid URL regex"));

const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
const REMINDER_WINDOW_PREFIX: &str = "reminder-display-";
const POLL_INTERVAL_SECS: u64 = 30;
const EVENTS_LOOKAHEAD_HOURS: i64 = 12;
const EVENTS_MAX_RESULTS: &str = "250";
const TRAY_OPEN_ID: &str = "tray-open";
const TRAY_QUIT_ID: &str = "tray-quit";
const DEVTOOLS_MENU_ID_PREFIX: &str = "window-devtools:";
const REMINDER_WINDOW_MARGIN: f64 = 0.0;
const REMINDER_WINDOW_WIDTH: f64 = 540.0;
const REMINDER_WINDOW_HEIGHT: f64 = 160.0;
const REMINDER_UPCOMING_MINUTES: i64 = 5;

type CommandResult<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistentState {
    google_oauth_config: Option<GoogleOAuthConfig>,
    token: Option<TokenState>,
    cached_events: Vec<CalendarEventSummary>,
    last_sync_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
struct GoogleOAuthConfig {
    client_id: Option<String>,
    client_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenState {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: DateTime<Utc>,
}

impl TokenState {
    fn has_access_token(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    fn has_refresh_token(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    fn access_token_is_fresh(&self) -> bool {
        self.expires_at > Utc::now() + ChronoDuration::seconds(60)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CalendarEventSummary {
    id: String,
    title: String,
    start_at: String,
    end_at: Option<String>,
    location: Option<String>,
    meeting_url: Option<String>,
}

// --- Reminder state machine ---

#[derive(Debug, Clone, Copy, PartialEq)]
enum EventWindowDisplayState {
    ShowingUpcoming,
    DismissedBeforeStart,
    ShowingCurrent,
    DismissedAfterStart,
}

#[derive(Debug, Clone)]
struct EventReminderRecord {
    event_id: String,
    title: String,
    start_at: DateTime<Utc>,
    end_at: Option<DateTime<Utc>>,
    meeting_url: Option<String>,
    location: Option<String>,
    display_state: EventWindowDisplayState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventReminderPayload {
    event_id: String,
    title: String,
    start_at: String,
    end_at: Option<String>,
    phase: ReminderPhase,
    meeting_url: Option<String>,
    location: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReminderPhase {
    Upcoming,
    StartingNow,
}

impl EventReminderRecord {
    fn to_payload(&self) -> EventReminderPayload {
        EventReminderPayload {
            event_id: self.event_id.clone(),
            title: self.title.clone(),
            start_at: self.start_at.to_rfc3339(),
            end_at: self.end_at.map(|t| t.to_rfc3339()),
            phase: match self.display_state {
                EventWindowDisplayState::ShowingUpcoming
                | EventWindowDisplayState::DismissedBeforeStart => ReminderPhase::Upcoming,
                EventWindowDisplayState::ShowingCurrent
                | EventWindowDisplayState::DismissedAfterStart => ReminderPhase::StartingNow,
            },
            meeting_url: self.meeting_url.clone(),
            location: self.location.clone(),
        }
    }
}

// --- App state ---

#[derive(Debug, Clone, Serialize)]
struct AppStatus {
    client_id_configured: bool,
    client_secret_configured: bool,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
    google_client_id_using_initial: bool,
    google_client_secret_using_initial: bool,
    signed_in: bool,
    polling_enabled: bool,
    auth_in_progress: bool,
    can_start_google_auth: bool,
    auto_reconnect_ready: bool,
    oauth_config_diagnostics: String,
    upcoming_events: Vec<CalendarEventSummary>,
    last_sync_at: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct OAuthStartResponse {
    auth_url: String,
}

#[derive(Debug, Clone)]
struct PendingAuth {
    state: String,
    code_verifier: String,
    redirect_uri: String,
    auth_url: String,
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Debug)]
struct AuthCallback {
    code: String,
    state: String,
}

#[derive(Default)]
struct AppStateStore {
    persistent: Mutex<PersistentState>,
    pending_auth: Mutex<Option<PendingAuth>>,
    event_reminder_states: Mutex<HashMap<String, EventReminderRecord>>,
    sync_flow: Mutex<()>,
    is_quitting: AtomicBool,
}

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

fn main() {
    run();
}

fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .manage(AppStateStore::default())
        .invoke_handler(tauri::generate_handler![
            get_app_status,
            save_google_oauth_settings,
            start_google_auth,
            disconnect_google,
            refresh_events,
            dismiss_event_reminder,
            get_event_reminder,
            show_devtools_context_menu
        ])
        .on_menu_event(|app, event| {
            handle_window_menu_event(app, event.id().as_ref());
        })
        .setup(|app| {
            load_persistent_state(app.handle())?;
            setup_tray_icon(app.handle())?;
            prepare_main_window(app.handle())?;
            spawn_polling_loop(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn oauth_config_diagnostics(app: &AppHandle) -> String {
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

fn prepare_main_window(app: &AppHandle) -> Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };

    let app_handle = app.clone();
    let window_clone = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            if is_quitting(&app_handle) {
                return;
            }

            api.prevent_close();
            let _ = window_clone.hide();
        }
    });

    Ok(())
}

fn setup_tray_icon(app: &AppHandle) -> Result<()> {
    let open_item = MenuItemBuilder::with_id(TRAY_OPEN_ID, "設定画面を開く").build(app)?;
    let quit_item = MenuItemBuilder::with_id(TRAY_QUIT_ID, "終了").build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&open_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let mut tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("Rokind Reminder")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            TRAY_OPEN_ID => {
                let _ = show_main_window(app);
            }
            TRAY_QUIT_ID => {
                request_exit(app);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }

    tray.build(app).context("failed to create tray icon")?;
    Ok(())
}

fn show_main_window(app: &AppHandle) -> Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };

    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(())
}

fn handle_window_menu_event(app: &AppHandle, menu_id: &str) {
    if let Some(window_label) = menu_id.strip_prefix(DEVTOOLS_MENU_ID_PREFIX) {
        open_window_devtools(app, window_label);
    }
}

fn open_window_devtools(app: &AppHandle, window_label: &str) {
    if let Some(window) = app.get_webview_window(window_label) {
        window.open_devtools();
    }
}

#[tauri::command]
fn show_devtools_context_menu(
    app: AppHandle,
    window: WebviewWindow,
    x: f64,
    y: f64,
) -> CommandResult<()> {
    let item = MenuItemBuilder::with_id(
        format!("{DEVTOOLS_MENU_ID_PREFIX}{}", window.label()),
        "DevTools を開く",
    )
    .build(&app)
    .map_err(|error| error.to_string())?;

    let menu = MenuBuilder::new(&app)
        .cut_with_text("切り取り")
        .copy_with_text("コピー")
        .paste_with_text("貼り付け")
        .separator()
        .item(&item)
        .build()
        .map_err(|error| error.to_string())?;

    window
        .popup_menu_at(&menu, Position::Logical(LogicalPosition::new(x, y)))
        .map_err(|error| error.to_string())
}

fn request_exit(app: &AppHandle) {
    let state = app.state::<AppStateStore>();
    state.is_quitting.store(true, Ordering::SeqCst);
    app.exit(0);
}

fn is_quitting(app: &AppHandle) -> bool {
    let state = app.state::<AppStateStore>();
    state.is_quitting.load(Ordering::SeqCst)
}

fn spawn_polling_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            if let Err(error) = sync_calendar_and_maybe_notify(&app).await {
                let message = error.to_string();
                record_error(&app, &message).await;
                let _ = app.emit("calendar-sync-failed", message);
            }

            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    });
}

async fn emit_app_status_updated(app: &AppHandle) {
    if let Ok(status) = get_app_status(app.clone()).await {
        let _ = app.emit("app-status-updated", status);
    }
}

#[cfg(debug_assertions)]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        eprintln!("[rokind-debug] {}", format_args!($($arg)*));
    };
}

#[cfg(not(debug_assertions))]
macro_rules! debug_log {
    ($($arg:tt)*) => {};
}

// --- Window label helpers ---

fn sanitize_for_window_label(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn event_window_label(event_id: &str, monitor_index: usize) -> String {
    format!(
        "{}{}_{monitor_index}",
        REMINDER_WINDOW_PREFIX,
        sanitize_for_window_label(event_id)
    )
}

fn sorted_showing_records(
    states: &HashMap<String, EventReminderRecord>,
) -> Vec<EventReminderRecord> {
    let mut active: Vec<EventReminderRecord> = states
        .values()
        .filter(|r| {
            matches!(
                r.display_state,
                EventWindowDisplayState::ShowingUpcoming | EventWindowDisplayState::ShowingCurrent
            )
        })
        .cloned()
        .collect();
    active.sort_by_key(|r| r.start_at);
    active
}

// --- Window management ---

async fn open_event_reminder_windows(app: &AppHandle, record: &EventReminderRecord) -> Result<()> {
    let state = app.state::<AppStateStore>();
    let reminder_states = state.event_reminder_states.lock().await;
    let active = sorted_showing_records(&reminder_states);
    let slot = active
        .iter()
        .position(|r| r.event_id == record.event_id)
        .unwrap_or(0);
    drop(reminder_states);

    debug_log!(
        "open_event_reminder_windows: event_id={} slot={} phase={:?}",
        record.event_id,
        slot,
        record.display_state
    );

    let monitors = app
        .available_monitors()
        .context("failed to enumerate monitors")?;
    let payload = record.to_payload();

    for (monitor_idx, monitor) in monitors.iter().enumerate() {
        let work_area = monitor.work_area();
        let scale = monitor.scale_factor();
        let waw = work_area.size.width as f64 / scale;
        let wah = work_area.size.height as f64 / scale;
        let wax = work_area.position.x as f64 / scale;
        let way = work_area.position.y as f64 / scale;

        let max_width = (waw - REMINDER_WINDOW_MARGIN * 2.0).max(1.0);
        let max_height = (wah - REMINDER_WINDOW_MARGIN * 2.0).max(1.0);
        let width = REMINDER_WINDOW_WIDTH.min(max_width);
        let height = REMINDER_WINDOW_HEIGHT.min(max_height);
        let x = wax + (waw - width) / 2.0;
        let base_y = way
            + (wah * 0.05).clamp(
                REMINDER_WINDOW_MARGIN,
                (wah - height - REMINDER_WINDOW_MARGIN).max(REMINDER_WINDOW_MARGIN),
            );
        let y = base_y + slot as f64 * (height + REMINDER_WINDOW_MARGIN);

        let label = event_window_label(&record.event_id, monitor_idx);

        if let Some(existing) = app.get_webview_window(&label) {
            let _ = existing.close();
        }

        WebviewWindowBuilder::new(
            app,
            &label,
            WebviewUrl::App(
                format!("index.html?view=reminder&event_id={}", record.event_id).into(),
            ),
        )
        .title("Meeting Reminder")
        .decorations(false)
        .resizable(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .devtools(true)
        .focused(true)
        .position(x, y)
        .inner_size(width, height)
        .build()
        .context("failed to build reminder window")?;

        let _ = app.emit_to(label.as_str(), "event-reminder-update", &payload);
    }

    Ok(())
}

async fn close_event_reminder_windows(app: &AppHandle, event_id: &str) -> Result<()> {
    debug_log!("close_event_reminder_windows: event_id={event_id}");
    let monitors = app
        .available_monitors()
        .context("failed to enumerate monitors")?;
    for (monitor_idx, _) in monitors.iter().enumerate() {
        let label = event_window_label(event_id, monitor_idx);
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
    Ok(())
}

async fn upgrade_event_reminder_phase(app: &AppHandle, event_id: &str) -> Result<()> {
    debug_log!("upgrade_event_reminder_phase: event_id={event_id}");
    let state = app.state::<AppStateStore>();
    let reminder_states = state.event_reminder_states.lock().await;
    let Some(record) = reminder_states.get(event_id) else {
        return Ok(());
    };
    let payload = record.to_payload();
    drop(reminder_states);

    let monitors = app
        .available_monitors()
        .context("failed to enumerate monitors")?;
    for (monitor_idx, _) in monitors.iter().enumerate() {
        let label = event_window_label(event_id, monitor_idx);
        if app.get_webview_window(&label).is_some() {
            let _ = app.emit_to(label.as_str(), "event-reminder-update", &payload);
        }
    }

    Ok(())
}

async fn close_all_event_reminder_windows(app: &AppHandle) {
    let labels: Vec<String> = app
        .webview_windows()
        .keys()
        .filter(|label| label.starts_with(REMINDER_WINDOW_PREFIX))
        .cloned()
        .collect();

    for label in labels {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
}

// --- Calendar sync + reminder state machine ---

async fn sync_calendar_and_maybe_notify(app: &AppHandle) -> Result<()> {
    let state = app.state::<AppStateStore>();
    let _sync_flow = state.sync_flow.lock().await;

    let events = match sync_calendar_once(app).await {
        Ok(events) => events,
        Err(error) => {
            record_error(app, &error.to_string()).await;
            return Err(error);
        }
    };

    let now = Utc::now();
    let fetched_ids: std::collections::HashSet<String> =
        events.iter().map(|e| e.id.clone()).collect();

    let mut to_open: Vec<EventReminderRecord> = Vec::new();
    let mut to_upgrade: Vec<String> = Vec::new();
    let mut to_close: Vec<String> = Vec::new();

    {
        let mut reminder_states = state.event_reminder_states.lock().await;

        // Auto-close windows for events that ended (no longer in fetched list)
        let expired: Vec<String> = reminder_states
            .iter()
            .filter(|(id, record)| {
                !fetched_ids.contains(id.as_str())
                    && matches!(
                        record.display_state,
                        EventWindowDisplayState::ShowingUpcoming
                            | EventWindowDisplayState::ShowingCurrent
                    )
            })
            .map(|(id, _)| id.clone())
            .collect();

        for event_id in &expired {
            reminder_states.remove(event_id);
            to_close.push(event_id.clone());
        }

        // Clean up dismissed states for events no longer fetched
        reminder_states.retain(|id, _| fetched_ids.contains(id));

        for event in &events {
            let start_at = match DateTime::parse_from_rfc3339(&event.start_at) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => continue,
            };
            let end_at = event
                .end_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let remaining = start_at - now;
            let is_past_start = remaining <= ChronoDuration::zero();
            let is_upcoming =
                !is_past_start && remaining <= ChronoDuration::minutes(REMINDER_UPCOMING_MINUTES);

            match reminder_states.get(&event.id).map(|r| r.display_state) {
                None => {
                    if is_upcoming || is_past_start {
                        let record = EventReminderRecord {
                            event_id: event.id.clone(),
                            title: event.title.clone(),
                            start_at,
                            end_at,
                            meeting_url: event.meeting_url.clone(),
                            location: event.location.clone(),
                            display_state: if is_past_start {
                                EventWindowDisplayState::ShowingCurrent
                            } else {
                                EventWindowDisplayState::ShowingUpcoming
                            },
                        };
                        reminder_states.insert(event.id.clone(), record.clone());
                        to_open.push(record);
                    }
                }
                Some(EventWindowDisplayState::ShowingUpcoming) => {
                    if is_past_start {
                        if let Some(rec) = reminder_states.get_mut(&event.id) {
                            rec.display_state = EventWindowDisplayState::ShowingCurrent;
                            rec.end_at = end_at;
                        }
                        to_upgrade.push(event.id.clone());
                    }
                }
                Some(EventWindowDisplayState::DismissedBeforeStart) => {
                    if is_past_start {
                        let record = EventReminderRecord {
                            event_id: event.id.clone(),
                            title: event.title.clone(),
                            start_at,
                            end_at,
                            meeting_url: event.meeting_url.clone(),
                            location: event.location.clone(),
                            display_state: EventWindowDisplayState::ShowingCurrent,
                        };
                        reminder_states.insert(event.id.clone(), record.clone());
                        to_open.push(record);
                    }
                }
                Some(
                    EventWindowDisplayState::ShowingCurrent
                    | EventWindowDisplayState::DismissedAfterStart,
                ) => {}
            }
        }
    }

    debug_log!(
        "sync: to_open={} to_upgrade={} to_close={}",
        to_open.len(),
        to_upgrade.len(),
        to_close.len()
    );

    for event_id in &to_close {
        close_event_reminder_windows(app, event_id).await?;
    }

    to_open.sort_by_key(|r| r.start_at);
    for record in &to_open {
        open_event_reminder_windows(app, record).await?;
    }

    for event_id in &to_upgrade {
        upgrade_event_reminder_phase(app, event_id).await?;
    }

    if !to_close.is_empty() || !to_open.is_empty() || !to_upgrade.is_empty() {
        emit_app_status_updated(app).await;
    }

    Ok(())
}

async fn sync_calendar_once(app: &AppHandle) -> Result<Vec<CalendarEventSummary>> {
    let existing_token;
    {
        let state = app.state::<AppStateStore>();
        let persistent = state.persistent.lock().await;
        existing_token = persistent.token.clone();
    }

    let oauth_config = google_oauth_config(app).await;

    let Some(client_id) = oauth_config.client_id else {
        return Ok(vec![]);
    };

    let Some(token) = existing_token else {
        return Ok(vec![]);
    };

    let token =
        match ensure_access_token(&client_id, oauth_config.client_secret.as_deref(), token).await {
            Ok(token) => token,
            Err(error) => {
                let message = error.to_string();
                if error_requires_reconnect(&message) {
                    prompt_reconnect(app, &message).await?;
                }
                return Err(error);
            }
        };
    let events = match fetch_calendar_events(&token.access_token).await {
        Ok(events) => events,
        Err(error) => {
            let message = error.to_string();
            if error_requires_reconnect(&message) {
                prompt_reconnect(app, &message).await?;
            }
            return Err(error);
        }
    };

    {
        let state = app.state::<AppStateStore>();
        let mut persistent = state.persistent.lock().await;
        persistent.token = Some(token);
        persistent.cached_events = events.clone();
        persistent.last_sync_at = Some(Utc::now());
        persistent.last_error = None;
    }

    persist_state(app).await?;
    emit_app_status_updated(app).await;

    Ok(events)
}

async fn ensure_access_token(
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

fn error_requires_reconnect(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("refresh token is missing")
        || normalized.contains("invalid_grant")
        || normalized.contains("expired or revoked")
        || normalized.contains("invalid authentication credentials")
}

async fn fetch_calendar_events(access_token: &str) -> Result<Vec<CalendarEventSummary>> {
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

fn initial_google_oauth_config() -> GoogleOAuthConfig {
    GoogleOAuthConfig {
        client_id: env_config_value("GOOGLE_CLIENT_ID"),
        client_secret: env_config_value("GOOGLE_CLIENT_SECRET"),
    }
}

fn effective_google_oauth_config(stored: Option<&GoogleOAuthConfig>) -> GoogleOAuthConfig {
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

async fn stored_google_oauth_config(app: &AppHandle) -> Option<GoogleOAuthConfig> {
    let state = app.state::<AppStateStore>();
    let persistent = state.persistent.lock().await;
    persistent.google_oauth_config.clone()
}

async fn google_oauth_config(app: &AppHandle) -> GoogleOAuthConfig {
    let stored = stored_google_oauth_config(app).await;
    effective_google_oauth_config(stored.as_ref())
}

fn trimmed_config_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn parse_google_error(response: reqwest::Response) -> String {
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

async fn record_error(app: &AppHandle, message: &str) {
    let state = app.state::<AppStateStore>();
    {
        let mut persistent = state.persistent.lock().await;
        persistent.last_error = Some(message.to_string());
    }
    let _ = persist_state(app).await;
}

async fn fail_auth(app: &AppHandle, message: &str) -> String {
    record_error(app, message).await;
    let _ = app.emit("auth-flow-failed", message.to_string());
    message.to_string()
}

async fn reset_google_auth_state(app: &AppHandle, last_error: Option<String>) -> Result<()> {
    {
        let state = app.state::<AppStateStore>();
        let mut persistent = state.persistent.lock().await;
        persistent.token = None;
        persistent.cached_events.clear();
        persistent.last_sync_at = None;
        persistent.last_error = last_error;
    }
    {
        let state = app.state::<AppStateStore>();
        let mut pending_auth = state.pending_auth.lock().await;
        *pending_auth = None;
    }
    {
        let state = app.state::<AppStateStore>();
        let mut reminder_states = state.event_reminder_states.lock().await;
        reminder_states.clear();
    }

    close_all_event_reminder_windows(app).await;
    persist_state(app).await?;
    emit_app_status_updated(app).await;
    Ok(())
}

async fn prompt_reconnect(app: &AppHandle, message: &str) -> Result<()> {
    reset_google_auth_state(app, Some(message.to_string())).await?;
    Ok(())
}

async fn persist_state(app: &AppHandle) -> Result<()> {
    let persistent = {
        let state = app.state::<AppStateStore>();
        let persistent = state.persistent.lock().await.clone();
        persistent
    };

    let path = state_file_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create app data directory")?;
    }

    let serialized =
        serde_json::to_string_pretty(&persistent).context("failed to serialize state")?;
    fs::write(path, serialized).context("failed to write state file")?;
    Ok(())
}

fn load_persistent_state(app: &AppHandle) -> Result<()> {
    let path = state_file_path(app)?;
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path).context("failed to read saved state file")?;
    let persistent: PersistentState =
        serde_json::from_str(&content).context("failed to parse saved state file")?;

    let state = app.state::<AppStateStore>();
    tauri::async_runtime::block_on(async move {
        let mut guard = state.persistent.lock().await;
        *guard = persistent;
    });

    Ok(())
}

fn state_file_path(app: &AppHandle) -> Result<PathBuf> {
    let mut dir = app
        .path()
        .app_data_dir()
        .context("failed to resolve app data directory")?;
    dir.push("state.json");
    Ok(dir)
}

fn random_token(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn build_auth_url(
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

async fn wait_for_auth_callback(listener: TokioTcpListener) -> Result<AuthCallback> {
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

async fn exchange_authorization_code(
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

    let token = exchange_authorization_code(
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
    sync_calendar_and_maybe_notify(app).await?;
    Ok(())
}

// --- Commands ---

#[tauri::command]
async fn get_app_status(app: AppHandle) -> CommandResult<AppStatus> {
    let state = app.state::<AppStateStore>();
    let persistent = state.persistent.lock().await.clone();
    let auth_in_progress = state.pending_auth.lock().await.is_some();
    let initial = initial_google_oauth_config();
    let oauth_config = effective_google_oauth_config(persistent.google_oauth_config.as_ref());
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
        oauth_config_diagnostics: oauth_config_diagnostics(&app).await,
        upcoming_events: persistent.cached_events,
        last_sync_at: persistent.last_sync_at.map(|value| value.to_rfc3339()),
        last_error: persistent.last_error,
    })
}

#[tauri::command]
async fn save_google_oauth_settings(
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
    let previous_effective = google_oauth_config(&app).await;
    let next_effective = effective_google_oauth_config(Some(&next_stored));
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
        {
            let state = app.state::<AppStateStore>();
            let mut pending_auth = state.pending_auth.lock().await;
            *pending_auth = None;
        }
        {
            let state = app.state::<AppStateStore>();
            let mut reminder_states = state.event_reminder_states.lock().await;
            reminder_states.clear();
        }
        close_all_event_reminder_windows(&app).await;
    }

    persist_state(&app)
        .await
        .map_err(|error| error.to_string())?;
    let status = get_app_status(app.clone()).await?;
    let _ = app.emit("app-status-updated", status.clone());
    Ok(status)
}

#[tauri::command]
async fn start_google_auth(app: AppHandle) -> CommandResult<OAuthStartResponse> {
    let oauth_config = google_oauth_config(&app).await;
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

    let code_verifier = random_token(64);
    let state_token = random_token(32);
    let challenge_digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(challenge_digest);
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
    let auth_url = build_auth_url(&client_id, &redirect_uri, &state_token, &code_challenge)
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
            let callback = wait_for_auth_callback(listener).await?;
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
async fn disconnect_google(app: AppHandle) -> CommandResult<AppStatus> {
    reset_google_auth_state(&app, None)
        .await
        .map_err(|error| error.to_string())?;
    let status = get_app_status(app.clone()).await?;
    let _ = app.emit("app-status-updated", status.clone());
    Ok(status)
}

#[tauri::command]
async fn refresh_events(app: AppHandle) -> CommandResult<AppStatus> {
    sync_calendar_and_maybe_notify(&app)
        .await
        .map_err(|error| error.to_string())?;
    get_app_status(app).await
}

#[tauri::command]
async fn dismiss_event_reminder(app: AppHandle, event_id: String) -> CommandResult<()> {
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
async fn get_event_reminder(
    app: AppHandle,
    event_id: String,
) -> CommandResult<Option<EventReminderPayload>> {
    let state = app.state::<AppStateStore>();
    let reminder_states = state.event_reminder_states.lock().await;
    Ok(reminder_states.get(&event_id).map(|r| r.to_payload()))
}
