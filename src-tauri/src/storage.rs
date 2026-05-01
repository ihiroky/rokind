use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use tauri::{AppHandle, Manager};

use crate::state::{AppStateStore, PersistentState};

pub(crate) async fn persist_state(app: &AppHandle) -> Result<()> {
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

pub(crate) fn load_persistent_state(app: &AppHandle) -> Result<()> {
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

pub(crate) async fn record_error(app: &AppHandle, message: &str) {
    let state = app.state::<AppStateStore>();
    {
        let mut persistent = state.persistent.lock().await;
        persistent.last_error = Some(message.to_string());
    }
    let _ = persist_state(app).await;
}
