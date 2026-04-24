import "./styles.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { LogicalPosition, LogicalSize, currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";
import {
  disable as disableAutostart,
  enable as enableAutostart,
  isEnabled as isAutostartEnabled
} from "@tauri-apps/plugin-autostart";
import { openUrl } from "@tauri-apps/plugin-opener";

import type { ActiveReminder, AppStatus, OAuthStartResponse, ReminderPanelState } from "./types";

type LaunchOnLoginState = {
  busy: boolean;
  enabled: boolean | null;
  errorMessage: string;
};

const app = document.querySelector<HTMLDivElement>("#app");

if (!app) {
  throw new Error("App root was not found.");
}

const params = new URLSearchParams(window.location.search);
const isReminderView = params.get("view") === "reminder";
const REMINDER_WINDOW_MARGIN = 10;
const CLOCK_TICK_MS = 30_000;

document.documentElement.classList.toggle("reminder-view", isReminderView);
document.body.classList.toggle("reminder-view", isReminderView);

const clamp = (value: number, min: number, max: number) => Math.min(Math.max(value, min), max);

const formatDateTime = (value: string) =>
  new Intl.DateTimeFormat("ja-JP", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit"
  }).format(new Date(value));

const isPastDateTime = (value: string) => {
  const timestamp = new Date(value).getTime();
  return Number.isFinite(timestamp) && timestamp <= Date.now();
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

const fitReminderWindowToContent = () => {
  if (!isReminderView) {
    return;
  }

  const reminderCard = document.querySelector<HTMLElement>(".reminder-card");
  if (!reminderCard) {
    return;
  }

  const syncWindowBounds = () => {
    window.requestAnimationFrame(() => {
      void (async () => {
        const reminderWindow = getCurrentWindow();
        const monitor = await currentMonitor();
        const cardRect = reminderCard.getBoundingClientRect();
        let width = Math.ceil(cardRect.width + REMINDER_WINDOW_MARGIN * 2);
        let height = Math.ceil(cardRect.height + REMINDER_WINDOW_MARGIN * 2);

        if (monitor) {
          const { scaleFactor, workArea } = monitor;
          const workAreaWidth = workArea.size.width / scaleFactor;
          const workAreaHeight = workArea.size.height / scaleFactor;
          const workAreaX = workArea.position.x / scaleFactor;
          const workAreaY = workArea.position.y / scaleFactor;

          width = Math.min(width, Math.floor(workAreaWidth - REMINDER_WINDOW_MARGIN * 2));
          height = Math.min(height, Math.floor(workAreaHeight - REMINDER_WINDOW_MARGIN * 2));

          const x = Math.round(workAreaX + (workAreaWidth - width) / 2);
          const y = Math.round(
            clamp(
              workAreaY + workAreaHeight * 0.05,
              workAreaY + REMINDER_WINDOW_MARGIN,
              workAreaY + workAreaHeight - height - REMINDER_WINDOW_MARGIN
            )
          );

          await reminderWindow.setSize(new LogicalSize(width, height));
          await reminderWindow.setPosition(new LogicalPosition(x, y));
          return;
        }

        await reminderWindow.setSize(new LogicalSize(width, height));
      })().catch((error) => {
        console.warn("Failed to fit reminder window to its content.", error);
      });
    });
  };

  syncWindowBounds();

  document.fonts.ready
    .then(() => {
      syncWindowBounds();
    })
    .catch(() => undefined);
};

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
  launchOnLoginState: LaunchOnLoginState = { busy: false, enabled: null, errorMessage: "" }
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
              Google Calendar の予定取得と再接続に使う認証設定です。接続状態の確認や認証フローの開始もここから行えます。
            </p>
          </div>
          <div class="action-row">
            <button id="connect-button" type="button" class="primary-button" ${status.can_start_google_auth ? "" : "disabled"}>
              ${status.auth_in_progress ? "認証ページを開く" : status.signed_in ? "再接続" : "Google で接続"}
            </button>
            <button id="refresh-button" type="button" class="secondary-button">予定を再取得</button>
            <button id="disconnect-button" type="button" class="ghost-button" ${status.signed_in || status.auth_in_progress ? "" : "disabled"}>切断</button>
          </div>

          ${
            !status.client_id_configured
              ? `<div class="helper">OAuth 読み込み状況: ${escapeHtml(status.oauth_config_diagnostics)}</div>`
              : ""
          }
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

type ReminderRenderState = {
  selectedReminder: ActiveReminder | null;
  selectedReminderId: string | null;
};

const renderReminderView = (
  panel: ReminderPanelState | null,
  selectedReminderId: string | null,
  expandedReminderList: boolean
): ReminderRenderState => {
  if (!panel || panel.reminders.length === 0) {
    app.innerHTML = `
      <main class="reminder-shell reminder-shell--empty" data-reminder-drag-region>
        <div class="reminder-card">
          <p class="eyebrow">Waiting</p>
          <h1>次のリマインドを待機しています</h1>
        </div>
      </main>
    `;
    fitReminderWindowToContent();
    enableReminderWindowDrag();
    return {
      selectedReminder: null,
      selectedReminderId: null
    };
  }

  const reminders = panel.reminders;
  const collapsedVisibleLimit = 3;
  const collapsedReminders = reminders.slice(0, collapsedVisibleLimit);
  const selectedReminder =
    reminders.find((reminder) => reminder.reminder_id === selectedReminderId) ??
    collapsedReminders[0] ??
    reminders[0];
  const visibleReminders = expandedReminderList ? reminders : collapsedReminders;
  const hiddenReminderCount = Math.max(reminders.length - collapsedVisibleLimit, 0);
  const isStartingNow = selectedReminder.phase === "starting_now";
  const eyebrowLabel = isStartingNow ? "current schedule" : "upcomming schedule";
  const reminderTimeLabel = formatDateTime(selectedReminder.start_at);
  const isPast = isPastDateTime(selectedReminder.start_at);
  const reminderList = visibleReminders
    .map((reminder) => {
      const selected = reminder.reminder_id === selectedReminder.reminder_id;

      return `
        <button
          type="button"
          class="reminder-list-item ${selected ? "reminder-list-item--selected" : ""}"
          data-reminder-select="${escapeHtml(reminder.reminder_id)}"
          data-no-window-drag
          aria-pressed="${selected ? "true" : "false"}"
        >
          <span class="reminder-list-time">${formatDateTime(reminder.start_at)}</span>
          <span class="reminder-list-body">
            <strong>${escapeHtml(reminder.title)}</strong>
            <small>${escapeHtml(reminder.location ?? "場所未設定")}</small>
          </span>
          <span class="reminder-list-pill ${reminder.meeting_url ? "reminder-list-pill--active" : ""}">
            ${reminder.meeting_url ? "Meet" : "No URL"}
          </span>
        </button>
      `;
    })
    .join("");

  app.innerHTML = `
    <main class="reminder-shell" data-reminder-drag-region>
      <section class="reminder-card ${isPast ? "reminder-card--past" : ""}">
        <div class="reminder-hero">
          <div class="reminder-hero-header">
            <p class="eyebrow">${eyebrowLabel}</p>
            ${
              reminders.length > 1
                ? `<span class="reminder-count-pill">${reminders.length}件の通知</span>`
                : ""
            }
          </div>
          <h1>${escapeHtml(selectedReminder.title)}</h1>
          <p class="reminder-time ${isPast ? "reminder-time--past" : ""}">
            ${reminderTimeLabel}
            ${isPast ? '<span class="reminder-time-status">開始時刻を過ぎています</span>' : ""}
          </p>
          <p class="reminder-location">${escapeHtml(selectedReminder.location ?? "場所の記載はありません")}</p>
          <div class="reminder-actions" data-no-window-drag>
            <button id="close-reminder-window-button" type="button" class="ghost-button">閉じる</button>
            <button id="join-button" type="button" class="primary-button" ${selectedReminder.meeting_url ? "" : "disabled"}>Meeting URL を開く</button>
          </div>
        </div>

        ${
          reminders.length > 1
            ? `
              <section class="reminder-list-section" data-no-window-drag>
                <div class="reminder-list-header">
                  <div>
                    <p class="eyebrow">Queue</p>
                    <h2>通知中の予定</h2>
                  </div>
                  <span class="reminder-list-summary">${reminders.length}件</span>
                </div>
                <div class="reminder-list">
                  ${reminderList}
                </div>
                ${
                  hiddenReminderCount > 0
                    ? `
                      <button id="toggle-reminder-list-button" type="button" class="secondary-button reminder-toggle-button">
                        ${expandedReminderList ? "折りたたむ" : `+${hiddenReminderCount}件を表示`}
                      </button>
                    `
                    : reminders.length > collapsedVisibleLimit
                      ? `
                        <button id="toggle-reminder-list-button" type="button" class="secondary-button reminder-toggle-button">
                          折りたたむ
                        </button>
                      `
                      : ""
                }
              </section>
            `
            : ""
        }
      </section>
    </main>
  `;

  fitReminderWindowToContent();
  enableReminderWindowDrag();
  return {
    selectedReminder,
    selectedReminderId: selectedReminder.reminder_id
  };
};

const bootReminderView = async () => {
  let selectedReminderId: string | null = null;
  let expandedReminderList = false;

  const redraw = () => {
    const panel = status.reminder_panel;
    if (!panel || panel.reminders.length <= 3) {
      expandedReminderList = false;
    }

    const renderState = renderReminderView(panel, selectedReminderId, expandedReminderList);
    selectedReminderId = renderState.selectedReminderId;

    if (!renderState.selectedReminder) {
      return;
    }

    const selectedReminder = renderState.selectedReminder;

    document.querySelector<HTMLButtonElement>("#close-reminder-window-button")?.addEventListener("click", async () => {
      await invoke("close_reminder_window");
    });

    document.querySelector<HTMLButtonElement>("#join-button")?.addEventListener("click", async () => {
      if (!selectedReminder.meeting_url) {
        return;
      }

      await openUrl(selectedReminder.meeting_url);
      await invoke("dismiss_reminder", { reminderId: selectedReminder.reminder_id });
    });

    document.querySelector<HTMLButtonElement>("#toggle-reminder-list-button")?.addEventListener("click", () => {
      expandedReminderList = !expandedReminderList;
      redraw();
    });

    document.querySelectorAll<HTMLButtonElement>("[data-reminder-select]").forEach((button) => {
      button.addEventListener("click", () => {
        const nextReminderId = button.dataset.reminderSelect;
        if (!nextReminderId) {
          return;
        }

        selectedReminderId = nextReminderId;
        redraw();
      });
    });
  };

  renderReminderView(null, null, false);

  let status = await invoke<AppStatus>("get_app_status");
  redraw();
  window.setInterval(() => {
    redraw();
  }, CLOCK_TICK_MS);

  await listen<ReminderPanelState | null>("reminder-updated", (event) => {
    status = { ...status, reminder_panel: event.payload };
    redraw();
  });

  await listen<AppStatus>("app-status-updated", (event) => {
    status = event.payload;
    redraw();
  });

  const currentWindow = getCurrentWindow();
  currentWindow.setFocus().catch(() => undefined);
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

  const resetCopyFeedback = () => {
    copiedEventId = null;
    copyFeedbackMessage = "";
    redraw();
  };

  const redraw = () => {
    renderMainView(status, busyMessage, copiedEventId, copyFeedbackMessage, launchOnLoginState);

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

  await listen<ReminderPanelState | null>("reminder-updated", (event) => {
    status = { ...status, reminder_panel: event.payload };
    redraw();
  });

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

if (isReminderView) {
  void bootReminderView();
} else {
  void bootMainView();
}
