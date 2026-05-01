use std::{collections::HashMap, sync::atomic::Ordering, time::Duration};

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalPosition, Manager, Position, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

use crate::{
    reminders::{EventReminderRecord, EventWindowDisplayState},
    state::{AppStateStore, CommandResult},
    storage::record_error,
};

const REMINDER_WINDOW_PREFIX: &str = "reminder-display-";
const AUTH_RECONNECT_NOTICE_WINDOW_PREFIX: &str = "auth-reconnect-notice-";
const POLL_INTERVAL_SECS: u64 = 30;
const AUTH_RECONNECT_NOTICE_INTERVAL_SECS: i64 = 600;
const TRAY_OPEN_ID: &str = "tray-open";
const TRAY_QUIT_ID: &str = "tray-quit";
const DEVTOOLS_MENU_ID_PREFIX: &str = "window-devtools:";
const REMINDER_WINDOW_MARGIN: f64 = 0.0;
const REMINDER_WINDOW_WIDTH: f64 = 540.0;
const REMINDER_WINDOW_HEIGHT: f64 = 160.0;

pub(crate) fn prepare_main_window(app: &AppHandle) -> Result<()> {
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

pub(crate) fn setup_tray_icon(app: &AppHandle) -> Result<()> {
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

pub(crate) fn show_main_window(app: &AppHandle) -> Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };

    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(())
}

pub(crate) fn handle_window_menu_event(app: &AppHandle, menu_id: &str) {
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
pub(crate) fn show_devtools_context_menu(
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

pub(crate) fn spawn_polling_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            if let Err(error) = crate::reminders::sync_calendar_and_maybe_notify(&app).await {
                let message = error.to_string();
                record_error(&app, &message).await;
                let _ = app.emit("calendar-sync-failed", message);
            }

            if let Err(error) = maybe_open_auth_reconnect_notice(&app).await {
                crate::debug_log!("failed to open auth reconnect notice: {error}");
            }

            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    });
}

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

fn auth_reconnect_notice_window_label(monitor_index: usize) -> String {
    format!("{AUTH_RECONNECT_NOTICE_WINDOW_PREFIX}{monitor_index}")
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

pub(crate) async fn open_event_reminder_windows(
    app: &AppHandle,
    record: &EventReminderRecord,
) -> Result<()> {
    let state = app.state::<AppStateStore>();
    let reminder_states = state.event_reminder_states.lock().await;
    let active = sorted_showing_records(&reminder_states);
    let slot = active
        .iter()
        .position(|r| r.event_id == record.event_id)
        .unwrap_or(0);
    drop(reminder_states);

    crate::debug_log!(
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

pub(crate) async fn close_event_reminder_windows(app: &AppHandle, event_id: &str) -> Result<()> {
    crate::debug_log!("close_event_reminder_windows: event_id={event_id}");
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

pub(crate) async fn upgrade_event_reminder_phase(app: &AppHandle, event_id: &str) -> Result<()> {
    crate::debug_log!("upgrade_event_reminder_phase: event_id={event_id}");
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

pub(crate) async fn close_all_event_reminder_windows(app: &AppHandle) {
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

pub(crate) async fn open_auth_reconnect_notice_windows(app: &AppHandle) -> Result<()> {
    let state = app.state::<AppStateStore>();
    let slot = {
        let notice = state.auth_reconnect_notice.lock().await;
        if notice.is_none() {
            return Ok(());
        }

        let reminder_states = state.event_reminder_states.lock().await;
        sorted_showing_records(&reminder_states).len()
    };

    let monitors = app
        .available_monitors()
        .context("failed to enumerate monitors")?;

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

        let label = auth_reconnect_notice_window_label(monitor_idx);

        if let Some(existing) = app.get_webview_window(&label) {
            let _ = existing.close();
        }

        WebviewWindowBuilder::new(
            app,
            &label,
            WebviewUrl::App("index.html?view=auth_reconnect".into()),
        )
        .title("Google Calendar Reconnect Required")
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
        .context("failed to build auth reconnect notice window")?;
    }

    Ok(())
}

pub(crate) async fn close_all_auth_reconnect_notice_windows(app: &AppHandle) {
    let labels: Vec<String> = app
        .webview_windows()
        .keys()
        .filter(|label| label.starts_with(AUTH_RECONNECT_NOTICE_WINDOW_PREFIX))
        .cloned()
        .collect();

    for label in labels {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
}

pub(crate) async fn clear_auth_reconnect_notice(app: &AppHandle) {
    {
        let state = app.state::<AppStateStore>();
        let mut notice = state.auth_reconnect_notice.lock().await;
        *notice = None;
    }

    close_all_auth_reconnect_notice_windows(app).await;
}

pub(crate) async fn maybe_open_auth_reconnect_notice(app: &AppHandle) -> Result<()> {
    let state = app.state::<AppStateStore>();
    let window_is_open = app
        .webview_windows()
        .keys()
        .any(|label| label.starts_with(AUTH_RECONNECT_NOTICE_WINDOW_PREFIX));

    if window_is_open {
        return Ok(());
    }

    let should_open = {
        let notice = state.auth_reconnect_notice.lock().await;
        let Some(notice) = notice.as_ref() else {
            return Ok(());
        };

        let now = Utc::now();
        notice.last_shown_at.is_none_or(|last| {
            now - last >= ChronoDuration::seconds(AUTH_RECONNECT_NOTICE_INTERVAL_SECS)
        })
    };

    if should_open {
        open_auth_reconnect_notice_windows(app).await?;
        let mut notice = state.auth_reconnect_notice.lock().await;
        if let Some(notice) = notice.as_mut() {
            notice.last_shown_at = Some(Utc::now());
        }
    }

    Ok(())
}
