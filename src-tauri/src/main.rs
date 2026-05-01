#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(debug_assertions)]
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        eprintln!("[rokind-debug] {}", format_args!($($arg)*));
    };
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {};
}

mod commands;
mod google;
mod reminders;
mod state;
mod storage;
mod windows;

use state::AppStateStore;
use storage::load_persistent_state;
use windows::{
    handle_window_menu_event, prepare_main_window, setup_tray_icon, show_main_window,
    spawn_polling_loop,
};

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
            commands::get_app_status,
            commands::save_google_oauth_settings,
            commands::start_google_auth,
            commands::disconnect_google,
            commands::refresh_events,
            commands::dismiss_event_reminder,
            commands::get_event_reminder,
            commands::get_auth_reconnect_notice,
            commands::dismiss_auth_reconnect_notice,
            commands::open_main_window_for_reconnect,
            windows::show_devtools_context_menu
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
