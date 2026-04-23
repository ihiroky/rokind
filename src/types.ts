export type ReminderPhase = "upcoming" | "starting_now";

export interface CalendarEventSummary {
  id: string;
  title: string;
  start_at: string;
  end_at: string | null;
  location: string | null;
  meeting_url: string | null;
}

export interface AppStatus {
  client_id_configured: boolean;
  client_secret_configured: boolean;
  signed_in: boolean;
  polling_enabled: boolean;
  auth_in_progress: boolean;
  can_start_google_auth: boolean;
  auto_reconnect_ready: boolean;
  oauth_config_diagnostics: string;
  active_reminder: ActiveReminder | null;
  queued_reminder_count: number;
  upcoming_events: CalendarEventSummary[];
  last_sync_at: string | null;
  last_error: string | null;
}

export interface ActiveReminder {
  reminder_id: string;
  event_id: string;
  title: string;
  start_at: string;
  phase: ReminderPhase;
  offset_minutes: 5 | null;
  meeting_url: string | null;
  location: string | null;
}

export interface OAuthStartResponse {
  auth_url: string;
}
