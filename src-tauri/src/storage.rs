use std::collections::HashMap;
use std::path::PathBuf;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::types::{AppSettings, WorkspaceEntry};

#[cfg(all(not(test), not(target_os = "android")))]
const REMOTE_BACKEND_TOKEN_SERVICE: &str = "com.jmcdev.opencodemonitor.remote-backend";

fn remote_backend_token_account(path: &PathBuf) -> String {
    format!(
        "settings:{}",
        URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes())
    )
}

#[cfg(test)]
fn test_remote_backend_token_store() -> &'static std::sync::Mutex<HashMap<String, String>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, String>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn read_remote_backend_token(path: &PathBuf) -> Result<Option<String>, String> {
    let store = test_remote_backend_token_store()
        .lock()
        .map_err(|_| "Failed to access test remote backend token store".to_string())?;
    Ok(store.get(&remote_backend_token_account(path)).cloned())
}

#[cfg(test)]
fn write_remote_backend_token(path: &PathBuf, token: Option<&str>) -> Result<bool, String> {
    let mut store = test_remote_backend_token_store()
        .lock()
        .map_err(|_| "Failed to access test remote backend token store".to_string())?;
    let key = remote_backend_token_account(path);
    if let Some(token) = token.map(str::trim).filter(|value| !value.is_empty()) {
        store.insert(key, token.to_string());
    } else {
        store.remove(&key);
    }
    Ok(true)
}

#[cfg(test)]
fn clear_test_remote_backend_tokens() {
    if let Ok(mut store) = test_remote_backend_token_store().lock() {
        store.clear();
    }
}

#[cfg(all(not(test), not(target_os = "android")))]
fn remote_backend_token_entry(path: &PathBuf) -> Result<keyring::Entry, String> {
    keyring::Entry::new(
        REMOTE_BACKEND_TOKEN_SERVICE,
        &remote_backend_token_account(path),
    )
    .map_err(|err| format!("Failed to open system keychain entry: {err}"))
}

#[cfg(all(not(test), not(target_os = "android")))]
fn read_remote_backend_token(path: &PathBuf) -> Result<Option<String>, String> {
    let entry = remote_backend_token_entry(path)?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(format!(
            "Failed to read remote backend token from system keychain: {err}"
        )),
    }
}

#[cfg(all(not(test), not(target_os = "android")))]
fn write_remote_backend_token(path: &PathBuf, token: Option<&str>) -> Result<bool, String> {
    let entry = remote_backend_token_entry(path)?;
    if let Some(token) = token.map(str::trim).filter(|value| !value.is_empty()) {
        entry.set_password(token).map_err(|err| {
            format!("Failed to store remote backend token in system keychain: {err}")
        })?;
        return Ok(true);
    }

    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(true),
        Err(err) => Err(format!(
            "Failed to remove remote backend token from system keychain: {err}"
        )),
    }
}

#[cfg(all(not(test), target_os = "android"))]
fn read_remote_backend_token(_path: &PathBuf) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(all(not(test), target_os = "android"))]
fn write_remote_backend_token(_path: &PathBuf, _token: Option<&str>) -> Result<bool, String> {
    Ok(false)
}

pub(crate) fn read_workspaces(path: &PathBuf) -> Result<HashMap<String, WorkspaceEntry>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let data = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let list: Vec<WorkspaceEntry> = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect())
}

pub(crate) fn write_workspaces(path: &PathBuf, entries: &[WorkspaceEntry]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    std::fs::write(path, data).map_err(|e| e.to_string())
}

pub(crate) fn read_settings(path: &PathBuf) -> Result<AppSettings, String> {
    if !path.exists() {
        return Ok(AppSettings::default());
    }
    let data = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut settings: AppSettings = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    let legacy_token = settings.remote_backend_token.clone();

    match read_remote_backend_token(path) {
        Ok(Some(token)) => {
            settings.remote_backend_token = Some(token);
            if legacy_token.is_some() {
                let _ = write_settings(path, &settings);
            }
        }
        Ok(None) => {
            if let Some(token) = legacy_token {
                match write_remote_backend_token(path, Some(&token)) {
                    Ok(true) => {
                        settings.remote_backend_token = Some(token);
                        let _ = write_settings(path, &settings);
                    }
                    Ok(false) | Err(_) => {
                        settings.remote_backend_token = Some(token);
                    }
                }
            } else {
                settings.remote_backend_token = None;
            }
        }
        Err(_) => {
            settings.remote_backend_token = legacy_token;
        }
    }

    Ok(settings)
}

pub(crate) fn write_settings(path: &PathBuf, settings: &AppSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut persisted = settings.clone();
    let normalized_token = persisted
        .remote_backend_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    // Prefer the system keychain, but keep settings writes resilient if the
    // platform secret store is unavailable in the current environment.
    let stored_securely =
        write_remote_backend_token(path, normalized_token.as_deref()).unwrap_or(false);
    persisted.remote_backend_token = if stored_securely {
        None
    } else {
        normalized_token
    };

    let data = serde_json::to_string_pretty(&persisted).map_err(|e| e.to_string())?;
    std::fs::write(path, data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        clear_test_remote_backend_tokens, read_settings, read_workspaces, write_settings,
        write_workspaces,
    };
    use crate::types::{AppSettings, WorkspaceEntry, WorkspaceKind, WorkspaceSettings};
    use uuid::Uuid;

    #[test]
    fn write_read_workspaces_persists_sort_and_group() {
        let temp_dir = std::env::temp_dir().join(format!("codex-monitor-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");
        let path = temp_dir.join("workspaces.json");

        let mut settings = WorkspaceSettings::default();
        settings.sort_order = Some(5);
        settings.group_id = Some("group-42".to_string());
        settings.sidebar_collapsed = true;
        settings.git_root = Some("/tmp".to_string());
        settings.codex_args = Some("--profile personal".to_string());

        let entry = WorkspaceEntry {
            id: "w1".to_string(),
            name: "Workspace".to_string(),
            path: "/tmp".to_string(),
            codex_bin: None,
            kind: WorkspaceKind::Main,
            parent_id: None,
            worktree: None,
            settings: settings.clone(),
        };

        write_workspaces(&path, &[entry]).expect("write workspaces");
        let read = read_workspaces(&path).expect("read workspaces");
        let stored = read.get("w1").expect("stored workspace");
        assert_eq!(stored.settings.sort_order, Some(5));
        assert_eq!(stored.settings.group_id.as_deref(), Some("group-42"));
        assert!(stored.settings.sidebar_collapsed);
        assert_eq!(stored.settings.git_root.as_deref(), Some("/tmp"));
        assert_eq!(
            stored.settings.codex_args.as_deref(),
            Some("--profile personal")
        );
    }

    #[test]
    fn write_settings_moves_remote_token_out_of_plaintext_file() {
        clear_test_remote_backend_tokens();
        let temp_dir =
            std::env::temp_dir().join(format!("opencode-monitor-settings-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");
        let path = temp_dir.join("settings.json");

        let mut settings = AppSettings::default();
        settings.remote_backend_token = Some("token-123".to_string());

        write_settings(&path, &settings).expect("write settings");
        let on_disk = std::fs::read_to_string(&path).expect("read settings file");

        assert!(!on_disk.contains("token-123"));

        let loaded = read_settings(&path).expect("read settings");
        assert_eq!(loaded.remote_backend_token.as_deref(), Some("token-123"));
    }

    #[test]
    fn read_settings_migrates_legacy_plaintext_remote_token() {
        clear_test_remote_backend_tokens();
        let temp_dir =
            std::env::temp_dir().join(format!("opencode-monitor-legacy-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");
        let path = temp_dir.join("settings.json");

        let mut settings = AppSettings::default();
        settings.remote_backend_token = Some("legacy-token".to_string());
        let data = serde_json::to_string_pretty(&settings).expect("serialize settings");
        std::fs::write(&path, data).expect("write legacy settings");

        let loaded = read_settings(&path).expect("read settings");
        assert_eq!(loaded.remote_backend_token.as_deref(), Some("legacy-token"));

        let on_disk = std::fs::read_to_string(&path).expect("read sanitized settings");
        assert!(!on_disk.contains("legacy-token"));
    }
}
