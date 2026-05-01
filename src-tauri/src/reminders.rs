use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::{
    commands::emit_app_status_updated,
    google,
    state::{AppStateStore, CalendarEventSummary},
    storage::{persist_state, record_error},
    windows::{
        clear_auth_reconnect_notice, close_all_auth_reconnect_notice_windows,
        close_all_event_reminder_windows, close_event_reminder_windows,
        maybe_open_auth_reconnect_notice, open_event_reminder_windows,
        upgrade_event_reminder_phase,
    },
};

const REMINDER_UPCOMING_MINUTES: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum EventWindowDisplayState {
    ShowingUpcoming,
    DismissedBeforeStart,
    ShowingCurrent,
    DismissedAfterStart,
}

#[derive(Debug, Clone)]
pub(crate) struct EventReminderRecord {
    pub(crate) event_id: String,
    pub(crate) title: String,
    pub(crate) start_at: DateTime<Utc>,
    pub(crate) end_at: Option<DateTime<Utc>>,
    pub(crate) meeting_url: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) display_state: EventWindowDisplayState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EventReminderPayload {
    pub(crate) event_id: String,
    pub(crate) title: String,
    pub(crate) start_at: String,
    pub(crate) end_at: Option<String>,
    pub(crate) phase: ReminderPhase,
    pub(crate) meeting_url: Option<String>,
    pub(crate) location: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthReconnectNoticeState {
    pub(crate) detail: String,
    pub(crate) first_detected_at: DateTime<Utc>,
    pub(crate) last_shown_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthReconnectNoticePayload {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    pub(crate) first_detected_at: String,
    pub(crate) last_shown_at: Option<String>,
}

impl AuthReconnectNoticeState {
    pub(crate) fn to_payload(&self) -> AuthReconnectNoticePayload {
        AuthReconnectNoticePayload {
            message: "Google Calendar の接続が無効になりました".to_string(),
            detail: Some(self.detail.clone()),
            first_detected_at: self.first_detected_at.to_rfc3339(),
            last_shown_at: self.last_shown_at.map(|t| t.to_rfc3339()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReminderPhase {
    Upcoming,
    StartingNow,
}

impl EventReminderRecord {
    pub(crate) fn to_payload(&self) -> EventReminderPayload {
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

pub(crate) async fn sync_calendar_and_maybe_notify(app: &AppHandle) -> Result<()> {
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

    crate::debug_log!(
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

    let oauth_config = google::google_oauth_config(app).await;

    let Some(client_id) = oauth_config.client_id else {
        return Ok(vec![]);
    };

    let Some(token) = existing_token else {
        return Ok(vec![]);
    };

    let token =
        match google::ensure_access_token(&client_id, oauth_config.client_secret.as_deref(), token)
            .await
        {
            Ok(token) => token,
            Err(error) => {
                let message = error.to_string();
                if google::error_requires_reconnect(&message) {
                    prompt_reconnect(app, &message).await?;
                }
                return Err(error);
            }
        };
    let events = match google::fetch_calendar_events(&token.access_token).await {
        Ok(events) => events,
        Err(error) => {
            let message = error.to_string();
            if google::error_requires_reconnect(&message) {
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

pub(crate) async fn reset_google_auth_state(
    app: &AppHandle,
    last_error: Option<String>,
) -> Result<()> {
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
    {
        let state = app.state::<AppStateStore>();
        let mut notice = state.auth_reconnect_notice.lock().await;
        *notice = None;
    }

    close_all_event_reminder_windows(app).await;
    close_all_auth_reconnect_notice_windows(app).await;
    persist_state(app).await?;
    emit_app_status_updated(app).await;
    Ok(())
}

async fn prompt_reconnect(app: &AppHandle, message: &str) -> Result<()> {
    reset_google_auth_state(app, Some(message.to_string())).await?;
    {
        let state = app.state::<AppStateStore>();
        let mut notice = state.auth_reconnect_notice.lock().await;
        let now = Utc::now();
        *notice = Some(AuthReconnectNoticeState {
            detail: message.to_string(),
            first_detected_at: now,
            last_shown_at: None,
        });
    }
    maybe_open_auth_reconnect_notice(app).await?;
    Ok(())
}

pub(crate) async fn clear_google_auth_runtime_state(app: &AppHandle) {
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
    clear_auth_reconnect_notice(app).await;
}
