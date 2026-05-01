use std::{collections::HashMap, sync::atomic::AtomicBool};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::reminders::{AuthReconnectNoticeState, EventReminderRecord};

pub(crate) type CommandResult<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct PersistentState {
    pub(crate) google_oauth_config: Option<GoogleOAuthConfig>,
    pub(crate) token: Option<TokenState>,
    pub(crate) cached_events: Vec<CalendarEventSummary>,
    pub(crate) last_sync_at: Option<DateTime<Utc>>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct GoogleOAuthConfig {
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TokenState {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_at: DateTime<Utc>,
}

impl TokenState {
    pub(crate) fn has_access_token(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    pub(crate) fn has_refresh_token(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    pub(crate) fn access_token_is_fresh(&self) -> bool {
        self.expires_at > Utc::now() + ChronoDuration::seconds(60)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CalendarEventSummary {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) start_at: String,
    pub(crate) end_at: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) meeting_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AppStatus {
    pub(crate) client_id_configured: bool,
    pub(crate) client_secret_configured: bool,
    pub(crate) google_client_id: Option<String>,
    pub(crate) google_client_secret: Option<String>,
    pub(crate) google_client_id_using_initial: bool,
    pub(crate) google_client_secret_using_initial: bool,
    pub(crate) signed_in: bool,
    pub(crate) polling_enabled: bool,
    pub(crate) auth_in_progress: bool,
    pub(crate) can_start_google_auth: bool,
    pub(crate) auto_reconnect_ready: bool,
    pub(crate) oauth_config_diagnostics: String,
    pub(crate) upcoming_events: Vec<CalendarEventSummary>,
    pub(crate) last_sync_at: Option<String>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OAuthStartResponse {
    pub(crate) auth_url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingAuth {
    pub(crate) state: String,
    pub(crate) code_verifier: String,
    pub(crate) redirect_uri: String,
    pub(crate) auth_url: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: Option<String>,
}

#[derive(Debug)]
pub(crate) struct AuthCallback {
    pub(crate) code: String,
    pub(crate) state: String,
}

#[derive(Default)]
pub(crate) struct AppStateStore {
    pub(crate) persistent: Mutex<PersistentState>,
    pub(crate) pending_auth: Mutex<Option<PendingAuth>>,
    pub(crate) event_reminder_states: Mutex<HashMap<String, EventReminderRecord>>,
    pub(crate) auth_reconnect_notice: Mutex<Option<AuthReconnectNoticeState>>,
    pub(crate) sync_flow: Mutex<()>,
    pub(crate) is_quitting: AtomicBool,
}
