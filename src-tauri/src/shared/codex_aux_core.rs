use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

use crate::backend::app_server::{
    build_codex_command_with_bin, build_codex_path_env, check_codex_installation, WorkspaceSession,
};
use crate::shared::process_core::tokio_command;
use crate::storage::write_workspaces;
use crate::types::{AppSettings, WorkspaceEntry};

const DEFAULT_COMMIT_MESSAGE_PROMPT: &str =
    "Generate a concise git commit message for the following changes. \
Follow conventional commit format (e.g., feat:, fix:, refactor:, docs:, etc.). \
Keep the summary line under 72 characters. \
Only output the commit message, nothing else.\n\n\
Changes:\n{diff}";

pub(crate) fn build_commit_message_prompt(diff: &str, template: &str) -> String {
    let base = if template.trim().is_empty() {
        DEFAULT_COMMIT_MESSAGE_PROMPT
    } else {
        template
    };
    if base.contains("{diff}") {
        base.replace("{diff}", diff)
    } else {
        format!("{base}\n\nChanges:\n{diff}")
    }
}

pub(crate) fn build_commit_message_prompt_for_diff(
    diff: &str,
    template: &str,
) -> Result<String, String> {
    if diff.trim().is_empty() {
        return Err("No changes to generate commit message for".to_string());
    }
    Ok(build_commit_message_prompt(diff, template))
}

pub(crate) fn build_run_metadata_prompt(cleaned_prompt: &str) -> String {
    format!(
        "You create concise run metadata for a coding task.\n\
Return ONLY a JSON object with keys:\n\
- title: short, clear, 3-7 words, Title Case\n\
- worktreeName: lower-case, kebab-case slug prefixed with one of: \
feat/, fix/, chore/, test/, docs/, refactor/, perf/, build/, ci/, style/.\n\n\
Choose fix/ when the task is a bug fix, error, regression, crash, or cleanup. \
Use the closest match for chores/tests/docs/refactors/perf/build/ci/style. \
Otherwise use feat/.\n\n\
Examples:\n\
{{\"title\":\"Fix Login Redirect Loop\",\"worktreeName\":\"fix/login-redirect-loop\"}}\n\
{{\"title\":\"Add Workspace Home View\",\"worktreeName\":\"feat/workspace-home\"}}\n\
{{\"title\":\"Update Lint Config\",\"worktreeName\":\"chore/update-lint-config\"}}\n\
{{\"title\":\"Add Coverage Tests\",\"worktreeName\":\"test/add-coverage-tests\"}}\n\n\
Task:\n{cleaned_prompt}"
    )
}

pub(crate) fn parse_run_metadata_value(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("No metadata was generated".to_string());
    }
    let json_value =
        extract_json_value(trimmed).ok_or_else(|| "Failed to parse metadata JSON".to_string())?;
    let title = json_value
        .get("title")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Missing title in metadata".to_string())?;
    let worktree_name = json_value
        .get("worktreeName")
        .or_else(|| json_value.get("worktree_name"))
        .and_then(|v| v.as_str())
        .map(sanitize_run_worktree_name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Missing worktree name in metadata".to_string())?;

    Ok(json!({
        "title": title,
        "worktreeName": worktree_name
    }))
}

pub(crate) fn extract_json_value(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&raw[start..=end]).ok()
}

pub(crate) fn sanitize_run_worktree_name(value: &str) -> String {
    let trimmed = value.trim().to_lowercase();
    let mut cleaned = String::new();
    let mut last_dash = false;
    for ch in trimmed.chars() {
        let next = if ch.is_ascii_alphanumeric() || ch == '/' {
            last_dash = false;
            Some(ch)
        } else if ch == '-' || ch.is_whitespace() || ch == '_' {
            if last_dash {
                None
            } else {
                last_dash = true;
                Some('-')
            }
        } else {
            None
        };
        if let Some(ch) = next {
            cleaned.push(ch);
        }
    }
    while cleaned.ends_with('-') || cleaned.ends_with('/') {
        cleaned.pop();
    }
    let allowed_prefixes = [
        "feat/",
        "fix/",
        "chore/",
        "test/",
        "docs/",
        "refactor/",
        "perf/",
        "build/",
        "ci/",
        "style/",
    ];
    if allowed_prefixes
        .iter()
        .any(|prefix| cleaned.starts_with(prefix))
    {
        return cleaned;
    }
    for prefix in &allowed_prefixes {
        let dash_prefix = prefix.replace('/', "-");
        if cleaned.starts_with(&dash_prefix) {
            return cleaned.replacen(&dash_prefix, prefix, 1);
        }
    }
    format!("feat/{}", cleaned.trim_start_matches('/'))
}

pub(crate) async fn codex_doctor_core(
    app_settings: &Mutex<AppSettings>,
    codex_bin: Option<String>,
    codex_args: Option<String>,
) -> Result<Value, String> {
    let (default_bin, default_args) = {
        let settings = app_settings.lock().await;
        (settings.codex_bin.clone(), settings.codex_args.clone())
    };
    let resolved = codex_bin
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or(default_bin);
    let resolved_args = codex_args
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or(default_args);
    let path_env = build_codex_path_env(resolved.as_deref());
    let version = check_codex_installation(resolved.clone()).await?;
    let mut command = build_codex_command_with_bin(
        resolved.clone(),
        resolved_args.as_deref(),
        vec!["app-server".to_string(), "--help".to_string()],
    )?;
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let app_server_ok = match timeout(Duration::from_secs(5), command.output()).await {
        Ok(result) => result
            .map(|output| output.status.success())
            .unwrap_or(false),
        Err(_) => false,
    };
    let (node_ok, node_version, node_details) = {
        let mut node_command = tokio_command("node");
        if let Some(ref path_env) = path_env {
            node_command.env("PATH", path_env);
        }
        node_command.arg("--version");
        node_command.stdout(std::process::Stdio::piped());
        node_command.stderr(std::process::Stdio::piped());
        match timeout(Duration::from_secs(5), node_command.output()).await {
            Ok(result) => match result {
                Ok(output) => {
                    if output.status.success() {
                        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        (
                            !version.is_empty(),
                            if version.is_empty() {
                                None
                            } else {
                                Some(version)
                            },
                            None,
                        )
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let detail = if stderr.trim().is_empty() {
                            stdout.trim()
                        } else {
                            stderr.trim()
                        };
                        (
                            false,
                            None,
                            Some(if detail.is_empty() {
                                "Node failed to start.".to_string()
                            } else {
                                detail.to_string()
                            }),
                        )
                    }
                }
                Err(err) => {
                    if err.kind() == ErrorKind::NotFound {
                        (false, None, Some("Node not found on PATH.".to_string()))
                    } else {
                        (false, None, Some(err.to_string()))
                    }
                }
            },
            Err(_) => (
                false,
                None,
                Some("Timed out while checking Node.".to_string()),
            ),
        }
    };
    let details = if app_server_ok {
        None
    } else {
        Some("Failed to run `codex app-server --help`.".to_string())
    };
    Ok(json!({
        "ok": version.is_some() && app_server_ok,
        "codexBin": resolved,
        "version": version,
        "appServerOk": app_server_ok,
        "details": details,
        "path": path_env,
        "nodeOk": node_ok,
        "nodeVersion": node_version,
        "nodeDetails": node_details,
    }))
}

pub(crate) async fn run_background_prompt_core<F>(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    storage_path: &PathBuf,
    workspace_id: String,
    prompt: String,
    model: Option<&str>,
    on_hide_thread: F,
) -> Result<String, String>
where
    F: Fn(&str, &str),
{
    let session = {
        let sessions = sessions.lock().await;
        sessions
            .get(&workspace_id)
            .ok_or("workspace not connected")?
            .clone()
    };

    let thread_result = session.rest_post("/session", json!({})).await?;

    let thread_id = thread_result
        .get("id")
        .and_then(|t| t.as_str())
        .ok_or_else(|| {
            format!(
                "Failed to get session id from POST /session response: {:?}",
                thread_result
            )
        })?
        .to_string();

    on_hide_thread(&workspace_id, &thread_id);
    if let Err(error) =
        remember_hidden_session_id(workspaces, storage_path, &workspace_id, &thread_id).await
    {
        eprintln!(
            "[codex_aux_core] failed to persist hidden helper session {} for workspace {}: {}",
            thread_id, workspace_id, error
        );
    }

    let prompt_path = format!("/session/{}/message", &thread_id);
    let mut prompt_body = json!({
        "parts": [{ "type": "text", "text": prompt }],
    });
    if let Some(model_override) = resolve_background_prompt_model(session.as_ref(), model).await {
        prompt_body["model"] = model_override;
    }
    let _prompt_guard = session.prompt_lock.lock().await;
    let async_path = format!("/session/{}/prompt_async", &thread_id);
    session.rest_post(&async_path, prompt_body).await?;
    poll_background_prompt_result(session.as_ref(), &prompt_path, &thread_id).await
}

fn extract_text_from_message_response(response: &Value) -> Result<String, String> {
    let Some(parts) = response.get("parts").and_then(|value| value.as_array()) else {
        return Err("Failed to parse helper response parts".to_string());
    };

    let mut text = String::new();
    for part in parts {
        let part_type = part.get("type").and_then(|value| value.as_str()).unwrap_or("");
        if part_type != "text" {
            continue;
        }
        if let Some(value) = part.get("text").and_then(|value| value.as_str()) {
            text.push_str(value);
        }
    }

    Ok(text.trim().to_string())
}

async fn resolve_background_prompt_model(
    session: &WorkspaceSession,
    requested_model: Option<&str>,
) -> Option<Value> {
    let requested = requested_model?.trim();
    if requested.is_empty() {
        return None;
    }

    if let Some((provider_id, model_id)) = requested.split_once('/') {
        let provider_id = provider_id.trim();
        let model_id = model_id.trim();
        if !provider_id.is_empty() && !model_id.is_empty() {
            return Some(json!({
                "providerID": provider_id,
                "modelID": model_id
            }));
        }
    }

    let providers = if let Some(cached) = session.models_cache.lock().await.clone() {
        cached
    } else {
        let fresh = session.rest_get("/config/providers").await.ok()?;
        *session.models_cache.lock().await = Some(fresh.clone());
        fresh
    };

    let providers = providers
        .get("providers")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    let mut matches = Vec::new();
    for provider in providers {
        let provider_id = provider
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .unwrap_or_default();
        if provider_id.is_empty() {
            continue;
        }

        let Some(models) = provider.get("models").and_then(|value| value.as_object()) else {
            continue;
        };

        let found = models.contains_key(requested)
            || models.values().any(|model| {
                model
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .unwrap_or_default()
                    == requested
            });
        if found {
            matches.push(provider_id.to_string());
        }
    }

    if matches.len() == 1 {
        Some(json!({
            "providerID": matches[0],
            "modelID": requested
        }))
    } else {
        None
    }
}

async fn poll_background_prompt_result(
    session: &WorkspaceSession,
    messages_path: &str,
    thread_id: &str,
) -> Result<String, String> {
    timeout(Duration::from_secs(90), async {
        let mut idle_polls = 0usize;

        loop {
            let statuses = session.rest_get("/session/status").await?;
            let status_type = statuses
                .get(thread_id)
                .and_then(|status| status.get("type"))
                .and_then(|value| value.as_str());

            let messages = session.rest_get(messages_path).await?;
            if let Some(result) = extract_background_prompt_result_from_messages(&messages) {
                return result;
            }

            match status_type {
                Some("busy") | Some("retry") => idle_polls = 0,
                _ => idle_polls += 1,
            }

            if idle_polls >= 4 {
                return Err("No response was generated".to_string());
            }

            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| "Timeout waiting for helper response".to_string())?
}

fn extract_background_prompt_result_from_messages(messages: &Value) -> Option<Result<String, String>> {
    let entries = messages.as_array()?;

    for entry in entries.iter().rev() {
        let info = entry.get("info")?;
        if info.get("role").and_then(|value| value.as_str()) != Some("assistant") {
            continue;
        }

        if let Some(error) = info.get("error") {
            let message = error
                .get("data")
                .and_then(|data| data.get("message"))
                .or_else(|| error.get("message"))
                .or_else(|| error.get("error"))
                .or_else(|| error.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or("Unknown error during helper prompt");
            return Some(Err(message.to_string()));
        }

        let is_completed = info
            .get("time")
            .and_then(|time| time.get("completed"))
            .is_some();
        if !is_completed {
            return None;
        }

        let text = extract_text_from_message_response(entry).unwrap_or_default();
        if text.is_empty() {
            return Some(Err("No response was generated".to_string()));
        }
        return Some(Ok(text));
    }

    None
}

async fn remember_hidden_session_id(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    storage_path: &PathBuf,
    workspace_id: &str,
    thread_id: &str,
) -> Result<(), String> {
    let entries = {
        let mut workspaces = workspaces.lock().await;
        let entry = workspaces
            .get_mut(workspace_id)
            .ok_or_else(|| "workspace not found".to_string())?;
        if !entry
            .settings
            .hidden_session_ids
            .iter()
            .any(|session_id| session_id == thread_id)
        {
            entry
                .settings
                .hidden_session_ids
                .push(thread_id.to_string());
        }
        workspaces.values().cloned().collect::<Vec<_>>()
    };

    write_workspaces(storage_path, &entries)
}

pub(crate) async fn generate_commit_message_core<F>(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    storage_path: &PathBuf,
    workspace_id: String,
    diff: &str,
    template: &str,
    model: Option<&str>,
    on_hide_thread: F,
) -> Result<String, String>
where
    F: Fn(&str, &str),
{
    let prompt = build_commit_message_prompt_for_diff(diff, template)?;
    run_background_prompt_core(
        sessions,
        workspaces,
        storage_path,
        workspace_id,
        prompt,
        model,
        on_hide_thread,
    )
    .await
}

pub(crate) async fn generate_run_metadata_core<F>(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    storage_path: &PathBuf,
    workspace_id: String,
    prompt: &str,
    on_hide_thread: F,
) -> Result<Value, String>
where
    F: Fn(&str, &str),
{
    let cleaned_prompt = prompt.trim();
    if cleaned_prompt.is_empty() {
        return Err("Prompt is required.".to_string());
    }

    let metadata_prompt = build_run_metadata_prompt(cleaned_prompt);
    let response = run_background_prompt_core(
        sessions,
        workspaces,
        storage_path,
        workspace_id,
        metadata_prompt,
        None,
        on_hide_thread,
    )
    .await?;

    parse_run_metadata_value(&response)
}

#[cfg(test)]
mod tests {
    use super::{
        build_commit_message_prompt_for_diff, extract_text_from_message_response,
        parse_run_metadata_value,
    };
    use serde_json::json;

    #[test]
    fn build_commit_message_prompt_for_diff_requires_changes() {
        let result = build_commit_message_prompt_for_diff("   ", "{diff}");
        assert_eq!(
            result.expect_err("should fail"),
            "No changes to generate commit message for"
        );
    }

    #[test]
    fn parse_run_metadata_value_normalizes_worktree_name_alias() {
        let raw =
            r#"{"title":"Fix Login Redirect Loop","worktree_name":"fix-login-redirect-loop"}"#;
        let parsed = parse_run_metadata_value(raw).expect("parse metadata");
        assert_eq!(parsed["title"], "Fix Login Redirect Loop");
        assert_eq!(parsed["worktreeName"], "fix/login-redirect-loop");
    }

    #[test]
    fn parse_run_metadata_value_requires_title() {
        let raw = r#"{"worktreeName":"feat/example"}"#;
        let result = parse_run_metadata_value(raw);
        assert_eq!(
            result.expect_err("should fail"),
            "Missing title in metadata"
        );
    }

    #[test]
    fn extract_text_from_message_response_concatenates_text_parts() {
        let response = json!({
            "info": { "id": "msg_1" },
            "parts": [
                { "type": "reasoning", "text": "thinking" },
                { "type": "text", "text": "fix: " },
                { "type": "text", "text": "update parser" }
            ]
        });
        let parsed = extract_text_from_message_response(&response).expect("helper text");
        assert_eq!(parsed, "fix: update parser");
    }
}
