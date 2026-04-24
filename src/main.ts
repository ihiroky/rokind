import "./styles.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  disable as disableAutostart,
  enable as enableAutostart,
  isEnabled as isAutostartEnabled
} from "@tauri-apps/plugin-autostart";
import { openUrl } from "@tauri-apps/plugin-opener";

import type { AppStatus, EventReminderPayload, OAuthStartResponse } from "./types";

type LaunchOnLoginState = {
  busy: boolean;
  enabled: boolean | null;
  errorMessage: string;
};

type OAuthFormState = {
  busy: boolean;
  clientId: string;
  clientSecret: string;
  dirty: boolean;
  errorMessage: string;
  feedbackMessage: string;
};

const app = document.querySelector<HTMLDivElement>("#app");

if (!app) {
  throw new Error("App root was not found.");
}

const params = new URLSearchParams(window.location.search);
const isReminderView = params.get("view") === "reminder";
const CLOCK_TICK_MS = 30_000;

document.documentElement.classList.toggle("reminder-view", isReminderView);
document.body.classList.toggle("reminder-view", isReminderView);

const formatDateTime = (value: string) =>
  new Intl.DateTimeFormat("ja-JP", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit"
  }).format(new Date(value));

const formatTime = (value: string) =>
  new Intl.DateTimeFormat("ja-JP", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false
  }).format(new Date(value));

const isPastDateTime = (value: string) => {
  const timestamp = new Date(value).getTime();
  return Number.isFinite(timestamp) && timestamp <= Date.now();
};

const formatReminderStartDelta = (value: string) => {
  const timestamp = new Date(value).getTime();

  if (!Number.isFinite(timestamp)) {
    return "";
  }

  const diffMs = timestamp - Date.now();
  const minutes = diffMs > 0 ? Math.ceil(diffMs / 60_000) : Math.floor(Math.abs(diffMs) / 60_000);

  return diffMs > 0 ? `（${minutes}分前）` : `（${minutes}分経過）`;
};

const escapeHtml = (value: string) =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

const describeError = (error: unknown) => {
  if (typeof error === "string") {
    return error;
  }

  if (error && typeof error === "object" && "message" in error) {
    const message = Reflect.get(error, "message");
    if (typeof message === "string") {
      return message;
    }
  }

  return "不明なエラーが発生しました。";
};

const debugLog = (message: string, payload?: unknown) => {
  const debugLogsEnabled = Boolean(
    (import.meta as ImportMeta & { env?: { DEV?: boolean } }).env?.DEV
  );

  if (!debugLogsEnabled) {
    return;
  }

  if (payload === undefined) {
    console.info(`[rokind-debug] ${message}`);
    return;
  }

  console.info(`[rokind-debug] ${message}`, payload);
};

const enableDevtoolsContextMenu = () => {
  document.addEventListener("contextmenu", (event) => {
    event.preventDefault();

    void invoke("show_devtools_context_menu", {
      x: event.clientX,
      y: event.clientY
    }).catch((error) => {
      console.warn("Failed to show devtools context menu.", error);
    });
  });
};

const enableReminderWindowDrag = () => {
  document.querySelector<HTMLElement>("[data-reminder-drag-region]")?.addEventListener("mousedown", (event) => {
    if (event.button !== 0) {
      return;
    }

    const target = event.target;
    if (!(target instanceof HTMLElement)) {
      return;
    }

    if (target.closest("button, a, input, textarea, select, [data-no-window-drag]")) {
      return;
    }

    event.preventDefault();
    getCurrentWindow().startDragging().catch((error) => {
      console.warn("Failed to start dragging reminder window.", error);
    });
  });
};

const sleep = (ms: number) =>
  new Promise<void>((resolve) => {
    window.setTimeout(resolve, ms);
  });

const copyTextToClipboard = async (value: string) => {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value);
    return;
  }

  const textarea = document.createElement("textarea");
  textarea.value = value;
  textarea.setAttribute("readonly", "true");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  textarea.style.pointerEvents = "none";
  document.body.append(textarea);
  textarea.select();

  const copied = document.execCommand("copy");
  textarea.remove();

  if (!copied) {
    throw new Error("クリップボードへのコピーに失敗しました。");
  }
};

const renderMainView = (
  status: AppStatus,
  busyMessage = "",
  copiedEventId: string | null = null,
  copyFeedbackMessage = "",
  launchOnLoginState: LaunchOnLoginState = { busy: false, enabled: null, errorMessage: "" },
  oauthFormState: OAuthFormState = {
    busy: false,
    clientId: status.google_client_id ?? "",
    clientSecret: status.google_client_secret ?? "",
    dirty: false,
    errorMessage: "",
    feedbackMessage: ""
  }
) => {
  const launchOnLoginStatus =
    launchOnLoginState.enabled === null
      ? "確認中"
      : launchOnLoginState.enabled
        ? "有効"
        : "無効";

  const eventCards =
    status.upcoming_events.length === 0
      ? `<div class="empty-card">現在時刻から 12 時間以内の予定はまだ取得されていません。</div>`
      : status.upcoming_events
          .map(
            (event) => {
              const isPast = isPastDateTime(event.start_at);

              return `
              <article class="event-card ${isPast ? "event-card--past" : ""}">
                <div class="event-time ${isPast ? "event-time--past" : ""}">
                  ${formatDateTime(event.start_at)}
                  ${isPast ? '<span class="event-time-status">開始時刻を過ぎました</span>' : ""}
                </div>
                <h3>${escapeHtml(event.title)}</h3>
                <p>${event.location ? escapeHtml(event.location) : "場所未設定"}</p>
                <div class="event-meta">
                  ${
                    event.meeting_url
                      ? `
                        <button
                          type="button"
                          class="link-pill copy-pill ${copiedEventId === event.id ? "link-pill--copied" : ""}"
                          data-copy-meeting-url="${escapeHtml(event.meeting_url)}"
                          data-event-id="${escapeHtml(event.id)}"
                        >
                          ${copiedEventId === event.id ? "コピーしました" : "Meeting URL をコピー"}
                        </button>
                        ${
                          copiedEventId === event.id
                            ? `
                              <span class="copy-hint copy-hint--copied" role="status">
                                クリップボードにコピー済みです
                              </span>
                            `
                            : ""
                        }
                      `
                      : `<span class="muted-pill">Meeting URL なし</span>`
                  }
                </div>
              </article>
            `
            }
          )
          .join("");

  app.innerHTML = `
    <main class="shell">
      <section class="hero">
        <div>
          <p class="eyebrow">Rokind Reminder</p>
          <h1>Google Calendar の予定を常駐監視して、5 分前と取得時点で進行中の予定を全ディスプレイへ表示します。</h1>
          <p class="lead">
            設定画面は閉じるとタスクトレイへ隠れ、アプリ本体は残ったまま該当タイミングで always-on-top のリマインドウィンドウが出ます。
          </p>
        </div>
        <div class="status-panel">
          <div class="status-line">
            <span>Google 接続</span>
            <strong>${status.signed_in ? "Connected" : "Disconnected"}</strong>
          </div>
          <div class="status-line">
            <span>Client ID</span>
            <strong>${status.client_id_configured ? "Configured" : "Not set"}</strong>
          </div>
          <div class="status-line">
            <span>Client Secret</span>
            <strong>${status.client_secret_configured ? "Configured" : "Not set"}</strong>
          </div>
          <div class="status-line">
            <span>監視状態</span>
            <strong>${status.polling_enabled ? "Polling" : "Idle"}</strong>
          </div>
          <div class="status-line">
            <span>自動再接続</span>
            <strong>${status.auto_reconnect_ready ? "Ready" : "Needs consent"}</strong>
          </div>
          <div class="status-line">
            <span>最終同期</span>
            <strong>${status.last_sync_at ? formatDateTime(status.last_sync_at) : "未同期"}</strong>
          </div>
        </div>
      </section>

      <section class="panel">
        <div class="panel-heading">
          <div>
            <p class="eyebrow">Setup</p>
            <h2>設定</h2>
            <p class="helper subtle">
              Google Calendar 連携とアプリ起動方法をここでまとめて管理できます。
            </p>
          </div>
        </div>

        <div class="settings-card">
          <div>
            <p class="eyebrow">Google OAuth</p>
            <h3>Google OAuth 設定</h3>
            <p class="helper subtle">
              Google Calendar の予定取得と再接続に使う認証設定です。
            </p>
          </div>
          <label class="toggle-row">
            <span>
              <strong>OAuth Client設定</strong>
            </span>
            <form id="oauth-settings-form" class="oauth-form">
              <label class="field" for="google-client-id-input">
                <span>Google Client ID</span>
                <input
                  id="google-client-id-input"
                  type="text"
                  size="80"
                  autocomplete="off"
                  spellcheck="false"
                  value="${escapeHtml(oauthFormState.clientId)}"
                  ${oauthFormState.busy ? "disabled" : ""}
                />
              </label>
              <label class="field" for="google-client-secret-input">
                <span>Google Client Secret</span>
                <input
                  id="google-client-secret-input"
                  type="password"
                  size="80"
                  autocomplete="off"
                  spellcheck="false"
                  value="${escapeHtml(oauthFormState.clientSecret)}"
                  ${oauthFormState.busy ? "disabled" : ""}
                />
              </label>
              <div class="action-row">
                <button type="submit" class="secondary-button" ${oauthFormState.busy ? "disabled" : ""}>OAuth 設定を保存</button>
              </div>
            </form>
          </label>
          ${
            oauthFormState.errorMessage
              ? `<div class="error-banner settings-banner">${escapeHtml(oauthFormState.errorMessage)}</div>`
              : ""
          }
          ${
            oauthFormState.feedbackMessage
              ? `<div class="busy-banner settings-banner">${escapeHtml(oauthFormState.feedbackMessage)}</div>`
              : ""
          }
          <div class="action-row">
            <button id="connect-button" type="button" class="primary-button" ${status.can_start_google_auth ? "" : "disabled"}>
              ${status.auth_in_progress ? "認証ページを開く" : status.signed_in ? "再接続" : "Google で接続"}
            </button>
            <button id="refresh-button" type="button" class="secondary-button">予定を再取得</button>
            <button id="disconnect-button" type="button" class="ghost-button" ${status.signed_in || status.auth_in_progress ? "" : "disabled"}>切断</button>
          </div>
        </div>

        <div class="settings-card">
          <div>
            <p class="eyebrow">Launch</p>
            <h3>起動設定</h3>
            <p class="helper subtle">
              OS にログインした直後に Rokind Reminder を自動で起動します。設定画面を閉じてもタスクトレイ常駐の動きはそのままです。
            </p>
          </div>
          <label class="toggle-row" for="launch-on-login-toggle">
            <span>
              <strong>ログイン直後に起動する</strong>
              <small>現在: ${launchOnLoginStatus}</small>
            </span>
            <input
              id="launch-on-login-toggle"
              class="toggle-input"
              type="checkbox"
              ${launchOnLoginState.enabled ? "checked" : ""}
              ${launchOnLoginState.busy || launchOnLoginState.enabled === null ? "disabled" : ""}
            />
          </label>
          ${
            launchOnLoginState.errorMessage
              ? `<div class="error-banner settings-banner">${escapeHtml(launchOnLoginState.errorMessage)}</div>`
              : ""
          }
          ${
            launchOnLoginState.busy
              ? `<div class="busy-banner settings-banner">起動設定を更新しています...</div>`
              : ""
          }
        </div>

        ${
          status.last_error
            ? `<div class="error-banner">${escapeHtml(status.last_error)}</div>`
            : ""
        }
        ${busyMessage ? `<div class="busy-banner">${escapeHtml(busyMessage)}</div>` : ""}
      </section>

      <section class="panel">
        <div class="panel-heading">
          <div>
            <p class="eyebrow">Upcoming</p>
            <h2>直近の予定</h2>
          </div>
          <div class="helper subtle">
            取得対象は現在時刻から 12 時間以内のイベントです。Meeting URL は Google Meet / conferenceData / 本文 URL を順に見ます。
          </div>
        </div>
        ${copyFeedbackMessage ? `<div class="busy-banner">${escapeHtml(copyFeedbackMessage)}</div>` : ""}
        <div class="event-grid">${eventCards}</div>
      </section>
    </main>
  `;
};

const renderReminderView = (payload: EventReminderPayload | null) => {
  debugLog("renderReminderView", { eventId: payload?.event_id, phase: payload?.phase });

  if (!payload) {
    app.innerHTML = "";
    return;
  }

  const isStartingNow = payload.phase === "starting_now";
  const eyebrowLabel = isStartingNow ? "Current Event" : "Upcoming Event";
  const reminderTimeLabel = formatTime(payload.start_at);
  const reminderStartDeltaLabel = formatReminderStartDelta(payload.start_at);
  const locationLabel = payload.location ?? "場所未設定";
  const meetingAvailabilityLabel = payload.meeting_url ? "Meetあり" : "Meetなし";
  const isPast = isPastDateTime(payload.start_at);
  const reminderStartDeltaClass = `reminder-time-delta ${isPast ? "reminder-time-delta--elapsed" : ""}`.trim();

  app.innerHTML = `
    <main class="reminder-shell" data-reminder-drag-region>
      <section class="reminder-card ${isPast ? "reminder-card--past" : ""}">
        <div class="reminder-current">
          <p class="eyebrow">${eyebrowLabel}</p>
          <div class="reminder-current-row">
            <time class="reminder-time ${isPast ? "reminder-time--past" : ""}" datetime="${escapeHtml(payload.start_at)}">
              <span>${reminderTimeLabel}</span>
              ${reminderStartDeltaLabel ? `<span class="${reminderStartDeltaClass}">${reminderStartDeltaLabel}</span>` : ""}
            </time>
            <h1 class="reminder-title" title="${escapeHtml(payload.title)}">${escapeHtml(payload.title)}</h1>
          </div>
          <p class="reminder-location">${escapeHtml(locationLabel)} / ${meetingAvailabilityLabel}</p>
        </div>

        <div class="reminder-actions" data-no-window-drag>
          <button id="close-reminder-button" type="button" class="ghost-button">閉じる</button>
          <button id="join-button" type="button" class="primary-button" ${payload.meeting_url ? "" : "disabled"}>Meeting URL を開く</button>
        </div>
      </section>
    </main>
  `;

  enableReminderWindowDrag();
};

const bootReminderView = async () => {
  const eventId = params.get("event_id");

  if (!eventId) {
    getCurrentWindow().close().catch(() => undefined);
    return;
  }

  let currentPayload: EventReminderPayload | null = await invoke<EventReminderPayload | null>(
    "get_event_reminder",
    { eventId }
  );

  debugLog("bootReminderView.initial", {
    eventId,
    phase: currentPayload?.phase ?? null
  });

  const redraw = () => {
    renderReminderView(currentPayload);

    if (!currentPayload) {
      return;
    }

    const payload = currentPayload;

    document.querySelector<HTMLButtonElement>("#close-reminder-button")?.addEventListener("click", async () => {
      debugLog("close_reminder.click", { eventId: payload.event_id });
      await invoke("dismiss_event_reminder", { eventId: payload.event_id });
    });

    document.querySelector<HTMLButtonElement>("#join-button")?.addEventListener("click", async () => {
      if (!payload.meeting_url) {
        return;
      }

      debugLog("join_button.click", { eventId: payload.event_id });
      await openUrl(payload.meeting_url);
      await invoke("dismiss_event_reminder", { eventId: payload.event_id });
    });
  };

  redraw();

  window.setInterval(() => {
    redraw();
  }, CLOCK_TICK_MS);

  await listen<EventReminderPayload | null>("event-reminder-update", (event) => {
    debugLog("event.event-reminder-update", { payload: event.payload });

    if (event.payload && event.payload.event_id !== eventId) {
      return;
    }

    currentPayload = event.payload;

    if (!currentPayload) {
      getCurrentWindow().close().catch(() => undefined);
      return;
    }

    redraw();
  });

  getCurrentWindow().setFocus().catch(() => undefined);
};

const bootMainView = async () => {
  let status = await invoke<AppStatus>("get_app_status");
  let busyMessage = "";
  let authPolling = false;
  let copiedEventId: string | null = null;
  let copyFeedbackMessage = "";
  let copyFeedbackTimer: number | null = null;
  let launchOnLoginState: LaunchOnLoginState = {
    busy: false,
    enabled: null,
    errorMessage: ""
  };
  let oauthFormState: OAuthFormState = {
    busy: false,
    clientId: status.google_client_id ?? "",
    clientSecret: status.google_client_secret ?? "",
    dirty: false,
    errorMessage: "",
    feedbackMessage: ""
  };

  const resetCopyFeedback = () => {
    copiedEventId = null;
    copyFeedbackMessage = "";
    redraw();
  };

  const syncOAuthFormWithStatus = () => {
    if (oauthFormState.busy || oauthFormState.dirty) {
      return;
    }

    oauthFormState = {
      ...oauthFormState,
      clientId: status.google_client_id ?? "",
      clientSecret: status.google_client_secret ?? ""
    };
  };

  const redraw = () => {
    syncOAuthFormWithStatus();
    renderMainView(
      status,
      busyMessage,
      copiedEventId,
      copyFeedbackMessage,
      launchOnLoginState,
      oauthFormState
    );

    const clientIdInput = document.querySelector<HTMLInputElement>("#google-client-id-input");
    const clientSecretInput = document.querySelector<HTMLInputElement>("#google-client-secret-input");

    clientIdInput?.addEventListener("input", () => {
      oauthFormState = {
        ...oauthFormState,
        clientId: clientIdInput.value,
        dirty: true,
        errorMessage: "",
        feedbackMessage: ""
      };
    });

    clientSecretInput?.addEventListener("input", () => {
      oauthFormState = {
        ...oauthFormState,
        clientSecret: clientSecretInput.value,
        dirty: true,
        errorMessage: "",
        feedbackMessage: ""
      };
    });

    document.querySelector<HTMLFormElement>("#oauth-settings-form")?.addEventListener("submit", async (event) => {
      event.preventDefault();

      oauthFormState = {
        ...oauthFormState,
        clientId: clientIdInput?.value ?? oauthFormState.clientId,
        clientSecret: clientSecretInput?.value ?? oauthFormState.clientSecret,
        busy: true,
        errorMessage: "",
        feedbackMessage: "OAuth 設定を保存しています..."
      };
      redraw();

      try {
        status = await invoke<AppStatus>("save_google_oauth_settings", {
          clientId: oauthFormState.clientId,
          clientSecret: oauthFormState.clientSecret
        });
        authPolling = false;
        oauthFormState = {
          busy: false,
          clientId: status.google_client_id ?? "",
          clientSecret: status.google_client_secret ?? "",
          dirty: false,
          errorMessage: "",
          feedbackMessage: "OAuth 設定を保存しました。必要に応じて Google へ再接続してください。"
        };
      } catch (error) {
        oauthFormState = {
          ...oauthFormState,
          busy: false,
          errorMessage: `OAuth 設定を保存できませんでした: ${describeError(error)}`,
          feedbackMessage: ""
        };
      }

      redraw();
    });

    document.querySelector<HTMLButtonElement>("#refresh-button")?.addEventListener("click", async () => {
      busyMessage = "Google Calendar を再取得しています...";
      redraw();
      status = await invoke<AppStatus>("refresh_events");
      busyMessage = "";
      redraw();
    });

    document.querySelector<HTMLButtonElement>("#disconnect-button")?.addEventListener("click", async () => {
      busyMessage = "Google 接続を切断しています...";
      redraw();

      try {
        status = await invoke<AppStatus>("disconnect_google");
      } catch (error) {
        status = {
          ...status,
          last_error: describeError(error)
        };
      } finally {
        authPolling = false;
        busyMessage = "";
        redraw();
      }
    });

    document.querySelector<HTMLButtonElement>("#connect-button")?.addEventListener("click", async () => {
      try {
        busyMessage = "ブラウザで Google 認証を開始します...";
        redraw();

        const start = await invoke<OAuthStartResponse>("start_google_auth");
        await openUrl(start.auth_url);

        void pollAuthUntilSettled();
      } catch (error) {
        status = {
          ...status,
          last_error: describeError(error)
        };
        busyMessage = "";
        redraw();
      }
    });

    document.querySelectorAll<HTMLButtonElement>("[data-copy-meeting-url]").forEach((button) => {
      button.addEventListener("click", async () => {
        const meetingUrl = button.dataset.copyMeetingUrl;
        const eventId = button.dataset.eventId;
        if (!meetingUrl || !eventId) {
          return;
        }

        try {
          await copyTextToClipboard(meetingUrl);
          copiedEventId = eventId;
          copyFeedbackMessage = "Meeting URL をクリップボードにコピーしました。";
          redraw();
          if (copyFeedbackTimer !== null) {
            window.clearTimeout(copyFeedbackTimer);
          }
          copyFeedbackTimer = window.setTimeout(() => {
            copyFeedbackTimer = null;
            resetCopyFeedback();
          }, 1800);
        } catch (error) {
          copiedEventId = null;
          copyFeedbackMessage = `Meeting URL をコピーできませんでした: ${describeError(error)}`;
          redraw();
          if (copyFeedbackTimer !== null) {
            window.clearTimeout(copyFeedbackTimer);
          }
          copyFeedbackTimer = window.setTimeout(() => {
            copyFeedbackTimer = null;
            resetCopyFeedback();
          }, 2600);
        }
      });
    });

    document.querySelector<HTMLInputElement>("#launch-on-login-toggle")?.addEventListener("change", async (event) => {
      const target = event.currentTarget;
      if (!(target instanceof HTMLInputElement)) {
        return;
      }

      launchOnLoginState = {
        ...launchOnLoginState,
        busy: true,
        errorMessage: ""
      };
      redraw();

      try {
        if (target.checked) {
          await enableAutostart();
        } else {
          await disableAutostart();
        }

        launchOnLoginState = {
          busy: false,
          enabled: await isAutostartEnabled(),
          errorMessage: ""
        };
      } catch (error) {
        launchOnLoginState = {
          ...launchOnLoginState,
          busy: false,
          errorMessage: `起動設定を更新できませんでした: ${describeError(error)}`
        };
      }

      redraw();
    });
  };

  const refreshLaunchOnLoginState = async () => {
    try {
      launchOnLoginState = {
        busy: false,
        enabled: await isAutostartEnabled(),
        errorMessage: ""
      };
    } catch (error) {
      launchOnLoginState = {
        busy: false,
        enabled: false,
        errorMessage: `起動設定を読み込めませんでした: ${describeError(error)}`
      };
    }

    redraw();
  };

  const pollAuthUntilSettled = async () => {
    if (authPolling) {
      return;
    }

    authPolling = true;
    try {
      while (true) {
        status = await invoke<AppStatus>("get_app_status");
        busyMessage = status.auth_in_progress ? "認証完了を待っています..." : "";
        redraw();

        if (!status.auth_in_progress) {
          break;
        }

        await sleep(1000);
      }
    } finally {
      authPolling = false;
      if (!status.auth_in_progress) {
        busyMessage = "";
        redraw();
      }
    }
  };

  redraw();
  window.setInterval(() => {
    redraw();
  }, CLOCK_TICK_MS);
  void refreshLaunchOnLoginState();

  if (status.auth_in_progress) {
    void pollAuthUntilSettled();
  }

  await listen<AppStatus>("app-status-updated", (event) => {
    status = event.payload;
    redraw();
  });

  await listen<string>("calendar-sync-failed", (event) => {
    status = { ...status, last_error: event.payload };
    redraw();
  });

  await listen<string>("auth-flow-failed", (event) => {
    busyMessage = "";
    status = {
      ...status,
      auth_in_progress: false,
      last_error: event.payload
    };
    redraw();
  });

  await listen("auth-flow-completed", async () => {
    status = await invoke<AppStatus>("get_app_status");
    busyMessage = "";
    redraw();
  });
};

enableDevtoolsContextMenu();

if (isReminderView) {
  void bootReminderView();
} else {
  void bootMainView();
}
