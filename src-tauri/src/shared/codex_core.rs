use base64::Engine;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;

use crate::backend::app_server::WorkspaceSession;
use crate::backend::event_translator;
use crate::backend::events::{AppServerEvent, EventSink};
use crate::codex::config as codex_config;
use crate::codex::home::{resolve_default_codex_home, resolve_workspace_codex_home};
use crate::rules;
use crate::shared::account::{build_account_response, read_auth_account};
use crate::shared::diff_utils::{generate_apply_patch_changes, generate_edit_diff};
use crate::types::WorkspaceEntry;

pub(crate) enum CodexLoginCancelState {
    PendingStart(oneshot::Sender<()>),
    LoginId(String),
}

async fn get_session_clone(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: &str,
) -> Result<Arc<WorkspaceSession>, String> {
    let sessions = sessions.lock().await;
    sessions
        .get(workspace_id)
        .cloned()
        .ok_or_else(|| "workspace not connected".to_string())
}

async fn resolve_workspace_and_parent(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: &str,
) -> Result<(WorkspaceEntry, Option<WorkspaceEntry>), String> {
    let workspaces = workspaces.lock().await;
    let entry = workspaces
        .get(workspace_id)
        .cloned()
        .ok_or_else(|| "workspace not found".to_string())?;
    let parent_entry = entry
        .parent_id
        .as_ref()
        .and_then(|parent_id| workspaces.get(parent_id))
        .cloned();
    Ok((entry, parent_entry))
}

async fn resolve_codex_home_for_workspace_core(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: &str,
) -> Result<PathBuf, String> {
    let (entry, parent_entry) = resolve_workspace_and_parent(workspaces, workspace_id).await?;
    resolve_workspace_codex_home(&entry, parent_entry.as_ref())
        .or_else(resolve_default_codex_home)
        .ok_or_else(|| "Unable to resolve OpenCode config directory".to_string())
}

fn should_include_hidden_sessions(sort_key: &Option<String>) -> bool {
    sort_key
        .as_deref()
        .map(str::trim)
        .map(|value| value.eq_ignore_ascii_case("all"))
        .unwrap_or(false)
}

fn replay_message_order_key(message: &Value) -> Option<String> {
    let timestamp = message
        .get("createdAt")
        .or_else(|| message.get("created_at"))
        .or_else(|| message.get("updatedAt"))
        .or_else(|| message.get("updated_at"))
        .or_else(|| message.get("time").and_then(|time| time.get("created")))
        .or_else(|| message.get("time").and_then(|time| time.get("createdAt")))
        .or_else(|| message.get("time").and_then(|time| time.get("created_at")))
        .or_else(|| message.get("time").and_then(|time| time.get("updated")))
        .or_else(|| message.get("time").and_then(|time| time.get("updatedAt")))
        .or_else(|| message.get("time").and_then(|time| time.get("updated_at")))?;

    if let Some(value) = timestamp.as_u64() {
        return Some(format!("{value:020}"));
    }
    if let Some(value) = timestamp.as_i64() {
        return Some(format!("{value:020}"));
    }
    timestamp
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn sort_replay_messages_chronologically(messages: &mut [Value]) {
    messages.sort_by(
        |a, b| match (replay_message_order_key(a), replay_message_order_key(b)) {
            (Some(a_key), Some(b_key)) => a_key.cmp(&b_key),
            _ => std::cmp::Ordering::Equal,
        },
    );
}

fn last_revertable_message_id(messages: &[Value]) -> Option<String> {
    let mut ordered = messages.to_vec();
    ordered.sort_by(|a, b| {
        let a_key = replay_message_order_key(a.get("info").unwrap_or(a));
        let b_key = replay_message_order_key(b.get("info").unwrap_or(b));
        match (a_key, b_key) {
            (Some(a_key), Some(b_key)) => a_key.cmp(&b_key),
            _ => std::cmp::Ordering::Equal,
        }
    });

    ordered.iter().rev().find_map(|entry| {
        entry.get("info")
            .and_then(|info| info.get("id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn apply_pending_revert_to_replay_messages(messages: &mut Vec<Value>, session_details: Option<&Value>) {
    let Some(revert) = session_details
        .and_then(|details| details.get("revert"))
        .filter(|value| value.is_object())
    else {
        return;
    };

    let Some(revert_message_id) = revert
        .get("messageID")
        .or_else(|| revert.get("messageId"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    let revert_part_id = revert
        .get("partID")
        .or_else(|| revert.get("partId"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let mut reached_target = false;
    let mut filtered: Vec<Value> = Vec::with_capacity(messages.len());

    for mut entry in messages.drain(..) {
        let message_id = entry
            .get("info")
            .and_then(|info| info.get("id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default();

        if reached_target {
            continue;
        }

        if message_id != revert_message_id {
            filtered.push(entry);
            continue;
        }

        reached_target = true;

        if let Some(target_part_id) = revert_part_id.as_deref() {
            if let Some(parts) = entry.get_mut("parts").and_then(|v| v.as_array_mut()) {
                if let Some(remove_start) = parts.iter().position(|part| {
                    part.get("id")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .unwrap_or_default()
                        == target_part_id
                }) {
                    parts.truncate(remove_start);
                }
            }
            filtered.push(entry);
        }
    }

    *messages = filtered;
}

async fn hidden_session_ids_for_workspace(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: &str,
) -> HashSet<String> {
    let workspaces = workspaces.lock().await;
    workspaces
        .get(workspace_id)
        .map(|entry| {
            entry
                .settings
                .hidden_session_ids
                .iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default()
}

fn session_is_archived(session: &Value) -> bool {
    session
        .get("time")
        .and_then(|time| time.get("archived"))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|n| (n > 0).then_some(n as u64)))
                .or_else(|| {
                    value
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .and_then(|s| s.parse::<u64>().ok())
                        .filter(|n| *n > 0)
                })
        })
        .is_some()
}

fn session_to_thread_entry(session: &Value) -> Option<Value> {
    let id = session
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if id.is_empty() {
        return None;
    }

    let title = session
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let updated_at = session
        .get("updatedAt")
        .or_else(|| session.get("updated_at"))
        .or_else(|| session.get("time").and_then(|time| time.get("updated")))
        .or_else(|| session.get("time").and_then(|time| time.get("updatedAt")))
        .or_else(|| session.get("time").and_then(|time| time.get("updated_at")))
        .cloned()
        .unwrap_or(Value::Null);
    let created_at = session
        .get("createdAt")
        .or_else(|| session.get("created_at"))
        .or_else(|| session.get("time").and_then(|time| time.get("created")))
        .or_else(|| session.get("time").and_then(|time| time.get("createdAt")))
        .or_else(|| session.get("time").and_then(|time| time.get("created_at")))
        .cloned()
        .unwrap_or_else(|| updated_at.clone());
    let directory = session
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let parent_id = session
        .get("parentID")
        .or_else(|| session.get("parentId"))
        .or_else(|| session.get("parent_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let mut entry = json!({
        "id": id,
        "cwd": directory,
        "name": title,
        "preview": title,
        "updatedAt": updated_at,
        "createdAt": created_at
    });
    if let Some(pid) = parent_id {
        entry["parentId"] = json!(pid);
    }
    Some(entry)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn review_target_to_opencode_arguments(target: &Value) -> Result<String, String> {
    let kind = target
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default();

    match kind {
        "" | "uncommittedChanges" => Ok(String::new()),
        "baseBranch" => target
            .get("branch")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| "review target missing branch".to_string()),
        "commit" => target
            .get("sha")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| "review target missing sha".to_string()),
        "custom" => target
            .get("instructions")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| "review target missing instructions".to_string()),
        other => Err(format!("unsupported review target type: {other}")),
    }
}

fn collaboration_agent_name_from_payload(collaboration_mode: Option<&Value>) -> Option<String> {
    collaboration_mode
        .and_then(|value| value.as_object())
        .and_then(|obj| obj.get("mode"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn collaboration_mode_entry_from_agent(agent: &Value) -> Option<Value> {
    let name = agent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }

    let agent_mode = agent
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(mode) = agent_mode {
        // Collaboration mode picker should only show primary-capable agents.
        if mode != "primary" && mode != "all" {
            return None;
        }
    }

    let hidden = agent
        .get("hidden")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if hidden {
        return None;
    }

    let description = agent
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let label = {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            None => name.clone(),
        }
    };

    Some(json!({
        "name": name,
        "label": label,
        "mode": name,
        "description": description,
        "settings": {},
    }))
}

/// Converts an agent from the REST API into a format suitable for @ mentions.
/// Filters for subagents (mode != "primary") that can be mentioned inline.
fn agent_mention_entry_from_agent(agent: &Value) -> Option<Value> {
    let name = agent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }

    let agent_mode = agent
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("subagent");

    // For @ mentions, filter out primary-only agents (like TUI does).
    // Include "subagent" and "all" modes.
    if agent_mode == "primary" {
        return None;
    }

    let hidden = agent
        .get("hidden")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if hidden {
        return None;
    }

    let description = agent
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Some(json!({
        "name": name,
        "mode": agent_mode,
        "description": description,
    }))
}

fn resume_thread_result_thread(thread_id: &str, session_details: Option<&Value>) -> Value {
    if let Some(details) = session_details.and_then(session_to_thread_entry) {
        return details;
    }
    json!({ "id": thread_id })
}

fn replay_collab_prompt_from_raw_input(raw_input: Option<&Value>) -> String {
    raw_input
        .and_then(|inp| inp.get("description"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            raw_input
                .and_then(|inp| inp.get("prompt"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn replay_collab_agent_status(item_status: &str, raw_input: Option<&Value>) -> Option<Value> {
    let agent = raw_input
        .and_then(|inp| inp.get("subagent_type"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();

    let agent_status = match item_status {
        "in_progress" => "running",
        "completed" => "completed",
        "failed" => "failed",
        _ => "unknown",
    };

    let mut agent_map = serde_json::Map::new();
    agent_map.insert(agent, json!({ "status": agent_status }));
    Some(Value::Object(agent_map))
}

// ---------------------------------------------------------------------------
// Thread / session lifecycle (REST)
// ---------------------------------------------------------------------------

pub(crate) async fn start_thread_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    // POST /session → { id, projectID, directory }
    let response = session.rest_post("/session", json!({})).await?;
    let session_id = response
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if !session_id.is_empty() {
        let mut ts = session.translation_state.lock().await;
        ts.session_id = session_id.clone();
    }
    Ok(json!({
        "result": {
            "thread": { "id": session_id }
        }
    }))
}

pub(crate) async fn resume_thread_core<E: EventSink>(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    event_sink: &E,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    let session_details = session
        .rest_get(&format!("/session/{thread_id}"))
        .await
        .ok();

    let path = format!("/session/{thread_id}/message");
    let messages = session.rest_get(&path).await?;

    {
        let mut ts = session.translation_state.lock().await;
        ts.prepare_replay(thread_id.clone());
    }

    let mut replay_item_counter = 0u64;
    let mut latest_assistant_info: Option<Value> = None;

    if let Some(msg_list) = messages.as_array() {
        let mut ordered_messages = msg_list.clone();
        sort_replay_messages_chronologically(&mut ordered_messages);
        apply_pending_revert_to_replay_messages(&mut ordered_messages, session_details.as_ref());
        for msg_entry in &ordered_messages {
            let role = msg_entry
                .get("info")
                .and_then(|i| i.get("role"))
                .and_then(|v| v.as_str())
                .unwrap_or("assistant");

            if role == "assistant" {
                if let Some(info) = msg_entry.get("info") {
                    latest_assistant_info = Some(info.clone());
                }
            }

            let parts = msg_entry
                .get("parts")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let mut text_fragments: Vec<String> = Vec::new();
            let mut content_parts: Vec<Value> = Vec::new();
            let mut tool_parts: Vec<Value> = Vec::new();

            for part in &parts {
                let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match part_type {
                    "text" => {
                        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            text_fragments.push(text.to_string());
                            content_parts.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    "tool" => {
                        tool_parts.push(part.clone());
                    }
                    "file" => {
                        if let Some(url) = part.get("url").and_then(|v| v.as_str()) {
                            content_parts.push(json!({
                                "type": "image",
                                "value": frontend_image_value(url)
                            }));
                        }
                    }
                    _ => {}
                }
            }

            if !content_parts.is_empty() {
                if role == "user" {
                    // Reuse the translator's stable per-session ID so the
                    // frontend merges with any live-SSE-emitted user message.
                    let item_id = {
                        let mut ts = session.translation_state.lock().await;
                        ts.user_message_item(&thread_id)
                    };
                    event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id: workspace_id.clone(),
                        message: json!({
                            "method": "item/completed",
                            "params": {
                                "threadId": thread_id,
                                "item": {
                                    "id": item_id,
                                    "type": "userMessage",
                                    "content": content_parts
                                }
                            }
                        }),
                    });
                    {
                        let mut ts = session.translation_state.lock().await;
                        ts.mark_new_replayed_user_message_boundary();
                    }
                } else {
                    replay_item_counter += 1;
                    let item_id = format!("replay_item_{replay_item_counter}");
                    let full_text = text_fragments.join("\n\n");
                    event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id: workspace_id.clone(),
                        message: json!({
                            "method": "item/completed",
                            "params": {
                                "threadId": thread_id,
                                "item": {
                                    "id": item_id,
                                    "type": "agentMessage",
                                    "text": full_text,
                                    "content": content_parts
                                }
                            }
                        }),
                    });
                }
            }

            for tool_part in &tool_parts {
                replay_item_counter += 1;
                let item_id = format!("replay_item_{replay_item_counter}");

                let tool_name = tool_part
                    .get("tool")
                    .or_else(|| tool_part.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                let (status_str, raw_input, output_str) =
                    if let Some(state_obj) = tool_part.get("state").filter(|v| v.is_object()) {
                        let st = state_obj
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("completed");
                        let inp = state_obj.get("input").cloned();
                        let out = state_obj
                            .get("output")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        (st, inp, out.to_string())
                    } else {
                        let st = tool_part
                            .get("state")
                            .and_then(|v| v.as_str())
                            .unwrap_or("completed");
                        let out = tool_part
                            .get("output")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        (st, None, out.to_string())
                    };

                let item_type = replay_tool_kind_to_item_type(tool_name);
                let final_status = match status_str {
                    "error" => "failed",
                    "completed" => "completed",
                    _ => "completed",
                };

                let item = replay_build_tool_item(
                    &item_id,
                    &thread_id,
                    item_type,
                    tool_name,
                    final_status,
                    raw_input.as_ref(),
                    &output_str,
                );

                event_sink.emit_app_server_event(AppServerEvent {
                    workspace_id: workspace_id.clone(),
                    message: json!({
                        "method": "item/completed",
                        "params": {
                            "threadId": thread_id,
                            "item": item
                        }
                    }),
                });
            }
        }
    }

    if let Some(info) = latest_assistant_info {
        let replay_usage_event = json!({
            "type": "message.updated",
            "properties": {
                "info": info
            }
        });

        let translated = {
            let mut ts = session.translation_state.lock().await;
            event_translator::translate_sse_event(&replay_usage_event, &mut ts)
        };

        for message in translated {
            event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: workspace_id.clone(),
                message,
            });
        }
    }

    Ok(json!({
        "result": {
            "thread": resume_thread_result_thread(&thread_id, session_details.as_ref())
        }
    }))
}

fn replay_tool_kind_to_item_type(tool_name: &str) -> &str {
    match tool_name {
        "edit" | "write" | "create" | "apply_patch" => "fileChange",
        "bash" | "command" | "terminal" => "commandExecution",
        "task" => "collabToolCall",
        "todowrite" => "todowrite",
        _ => "commandExecution",
    }
}

fn replay_build_tool_item(
    item_id: &str,
    thread_id: &str,
    item_type: &str,
    tool_name: &str,
    status: &str,
    raw_input: Option<&Value>,
    output: &str,
) -> Value {
    let mut item = json!({
        "id": item_id,
        "type": item_type,
        "status": status
    });

    if item_type == "collabToolCall" {
        item["tool"] = json!(tool_name);
        item["senderThreadId"] = json!(thread_id);
        let prompt = replay_collab_prompt_from_raw_input(raw_input);
        if !prompt.is_empty() {
            item["prompt"] = json!(prompt);
        }
        if let Some(agent_status) = replay_collab_agent_status(status, raw_input) {
            item["agentStatus"] = agent_status;
        }
    } else if item_type == "commandExecution" {
        let command = raw_input
            .and_then(|inp| {
                inp.get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        inp.get("command").and_then(|v| v.as_array()).map(|parts| {
                            parts
                                .iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    })
            })
            .unwrap_or_else(|| tool_name.to_string());
        item["command"] = json!([command]);
        if let Some(inp) = raw_input {
            for key in &["workdir", "cwd", "path"] {
                if let Some(cwd) = inp.get(key).and_then(|v| v.as_str()) {
                    if !cwd.is_empty() {
                        item["cwd"] = json!(cwd);
                        break;
                    }
                }
            }
        }
        if !output.trim().is_empty() {
            item["aggregatedOutput"] = json!(output);
        }
    } else if item_type == "fileChange" {
        let mut changes = Vec::new();
        if let Some(inp) = raw_input {
            if let Some(parsed_changes) = generate_apply_patch_changes(inp) {
                changes = parsed_changes;
            } else {
                for key in &["filePath", "path"] {
                    if let Some(path) = inp.get(key).and_then(|v| v.as_str()) {
                        if !path.is_empty() {
                            let mut change = json!({ "path": path, "kind": "modify" });
                            // Generate diff from oldString/newString if available
                            if let Some(diff) = generate_edit_diff(inp, path) {
                                change["diff"] = json!(diff);
                            }
                            changes.push(change);
                            break;
                        }
                    }
                }
            }
        }
        if !changes.is_empty() {
            item["changes"] = json!(changes);
        }
        if !output.trim().is_empty() {
            item["output"] = json!(output);
        }
    } else if item_type == "todowrite" {
        if let Some(inp) = raw_input {
            if let Some(todos) = inp.get("todos").and_then(|t| t.as_array()) {
                let todo_items: Vec<Value> = todos
                    .iter()
                    .filter_map(|todo| {
                        let content = todo.get("content").and_then(|v| v.as_str())?;
                        let todo_status = todo
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending");
                        let priority = todo
                            .get("priority")
                            .and_then(|v| v.as_str())
                            .unwrap_or("medium");
                        Some(json!({ "content": content, "status": todo_status, "priority": priority }))
                    })
                    .collect();
                item["todos"] = json!(todo_items);
            }
        }
    }

    item
}

pub(crate) async fn fork_thread_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let path = format!("/session/{thread_id}/fork");
    let response = session.rest_post(&path, json!({})).await?;
    let thread = session_to_thread_entry(&response).unwrap_or_else(|| {
        json!({
            "id": response.get("id").and_then(|v| v.as_str()).unwrap_or_default()
        })
    });
    Ok(json!({
        "result": {
            "thread": thread
        }
    }))
}

pub(crate) async fn list_threads_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: String,
    _cursor: Option<String>,
    _limit: Option<u32>,
    sort_key: Option<String>,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    // GET /session → Session[]
    let response = session.rest_get("/session").await?;
    let include_hidden = should_include_hidden_sessions(&sort_key);
    let hidden_session_ids = if include_hidden {
        HashSet::new()
    } else {
        hidden_session_ids_for_workspace(workspaces, &workspace_id).await
    };

    let sessions_arr = response.as_array().cloned().unwrap_or_default();
    let data: Vec<Value> = sessions_arr
        .into_iter()
        .filter_map(|s| {
            let id = s.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if !include_hidden && hidden_session_ids.contains(id) {
                return None;
            }
            if !include_hidden && session_is_archived(&s) {
                return None;
            }
            session_to_thread_entry(&s)
        })
        .collect();
    Ok(json!({
        "result": {
            "data": data,
            "nextCursor": null
        }
    }))
}

pub(crate) async fn list_mcp_server_status_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    _cursor: Option<String>,
    limit: Option<u32>,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let response = session.rest_get("/mcp").await?;

    let mut data = response
        .as_object()
        .map(|map| {
            let mut entries = map
                .iter()
                .map(|(name, status)| {
                    let status_label = status
                        .get("status")
                        .and_then(|v| v.as_str())
                        .or_else(|| status.as_str())
                        .unwrap_or_default()
                        .to_string();
                    json!({
                        "name": name,
                        "status": status_label,
                        "authStatus": status,
                        "auth_status": status,
                        "tools": {},
                        "resources": [],
                        "resourceTemplates": [],
                        "resource_templates": []
                    })
                })
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| {
                a.get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or_default())
            });
            entries
        })
        .unwrap_or_default();

    if let Some(limit) = limit {
        data.truncate(limit as usize);
    }

    Ok(json!({ "result": { "data": data, "nextCursor": null } }))
}

pub(crate) async fn list_slash_commands_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let response = session.rest_get("/command").await?;
    let data = response.as_array().cloned().unwrap_or_default();
    Ok(json!({ "result": { "data": data } }))
}

pub(crate) async fn archive_thread_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let path = format!("/session/{thread_id}");
    session
        .rest_patch(&path, json!({ "time": { "archived": now_unix_ms() } }))
        .await?;
    Ok(json!({ "ok": true }))
}

pub(crate) async fn undo_last_turn_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let messages_path = format!("/session/{thread_id}/message");
    let messages = session.rest_get(&messages_path).await?;
    let message_list = messages
        .as_array()
        .ok_or_else(|| "Invalid message list returned for undo.".to_string())?;
    let message_id = last_revertable_message_id(message_list)
        .ok_or_else(|| "No messages available to undo in this thread.".to_string())?;

    let path = format!("/session/{thread_id}/revert");
    let response = session
        .rest_post(&path, json!({ "messageID": message_id }))
        .await?;

    Ok(json!({
        "result": {
            "thread": resume_thread_result_thread(&thread_id, Some(&response))
        }
    }))
}

pub(crate) async fn redo_last_turn_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let path = format!("/session/{thread_id}/unrevert");
    let response = session.rest_post(&path, json!({})).await?;

    Ok(json!({
        "result": {
            "thread": resume_thread_result_thread(&thread_id, Some(&response))
        }
    }))
}

pub(crate) async fn compact_thread_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    model: Option<String>,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let requested_model = normalize_optional_string(model);
    let model_override = if let Some(ref model_id) = requested_model {
        resolve_prompt_model_override(session.as_ref(), model_id)
            .await
            .ok_or_else(|| {
                format!("Failed to resolve model `{model_id}` for context compaction.")
            })?
    } else {
        return Err(
            "No model selected for context compaction. Select a model (or connect a provider) and try again."
                .to_string(),
        );
    };

    let provider_id = model_override
        .get("providerID")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Compaction model resolution missing providerID.".to_string())?;
    let model_id = model_override
        .get("modelID")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Compaction model resolution missing modelID.".to_string())?;

    let path = format!("/session/{thread_id}/summarize");
    session
        .rest_post(
            &path,
            json!({
                "providerID": provider_id,
                "modelID": model_id,
            }),
        )
        .await
}

pub(crate) async fn execute_slash_command_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    command: String,
    arguments: Option<String>,
) -> Result<Value, String> {
    let command = command.trim().trim_start_matches('/').to_string();
    if command.is_empty() {
        return Err("empty slash command".to_string());
    }
    let session = get_session_clone(sessions, &workspace_id).await?;
    let path = format!("/session/{thread_id}/command");
    let body = json!({
        "command": command,
        "arguments": arguments.unwrap_or_default().trim().to_string()
    });
    session.rest_post(&path, body).await
}

pub(crate) async fn set_thread_name_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    name: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let path = format!("/session/{thread_id}");
    session
        .rest_patch(&path, json!({ "title": name.trim() }))
        .await?;
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Image handling
// ---------------------------------------------------------------------------

const URL_IMAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const URL_IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

fn extension_from_mime(mime: &str) -> &'static str {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "bin",
    }
}

fn persist_data_image_to_temp_file(data_url: &str) -> Option<String> {
    let trimmed = data_url.trim();
    let (metadata, encoded) = trimmed
        .strip_prefix("data:")?
        .split_once(";base64,")?;
    if !metadata.starts_with("image/") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;

    let mut hasher = DefaultHasher::new();
    metadata.hash(&mut hasher);
    bytes.hash(&mut hasher);
    let digest = hasher.finish();

    let cache_dir = std::env::temp_dir().join("opencode-monitor-image-cache");
    std::fs::create_dir_all(&cache_dir).ok()?;

    let extension = extension_from_mime(metadata);
    let path = cache_dir.join(format!("{digest:016x}.{extension}"));
    if !path.exists() {
        std::fs::write(&path, &bytes).ok()?;
    }
    path.to_str().map(|value| value.to_string())
}

fn frontend_image_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("data:image/") {
        return persist_data_image_to_temp_file(trimmed).unwrap_or_else(|| trimmed.to_string());
    }
    trimmed.to_string()
}

/// Build REST prompt parts from frontend input.
///
/// REST uses `{ type: "file", mime, url: "data:...", filename }` for images.
async fn build_rest_prompt_parts(
    text: String,
    images: Option<Vec<String>>,
    app_mentions: Option<Vec<Value>>,
    agent_mentions: Option<Vec<Value>>,
) -> Result<Vec<Value>, String> {
    let trimmed_text = text.trim();
    let mut parts: Vec<Value> = Vec::new();
    if !trimmed_text.is_empty() {
        parts.push(json!({ "type": "text", "text": trimmed_text }));
    }
    if let Some(paths) = images {
        for path in paths {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("data:") {
                // data: URI — use directly as file part.
                let mime = trimmed
                    .strip_prefix("data:")
                    .and_then(|rest| rest.split_once(";base64,"))
                    .map(|(m, _)| m)
                    .unwrap_or("application/octet-stream");
                parts.push(json!({
                    "type": "file",
                    "mime": mime,
                    "url": trimmed,
                    "filename": "image"
                }));
            } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                parts.push(fetch_url_image_as_file_part(trimmed).await?);
            } else {
                // Local file path — read and base64-encode.
                match std::fs::read(trimmed) {
                    Ok(bytes) => {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let mime = mime_from_extension(trimmed);
                        let filename = std::path::Path::new(trimmed)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("image");
                        parts.push(json!({
                            "type": "file",
                            "mime": mime,
                            "url": format!("data:{mime};base64,{encoded}"),
                            "filename": filename
                        }));
                    }
                    Err(_) => {
                        parts
                            .push(json!({ "type": "text", "text": format!("[image: {trimmed}]") }));
                    }
                }
            }
        }
    }
    if let Some(mentions) = app_mentions {
        let mut seen_paths: HashSet<String> = HashSet::new();
        for mention in mentions {
            let object = mention
                .as_object()
                .ok_or_else(|| "invalid app mention payload".to_string())?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "invalid app mention name".to_string())?;
            let path = object
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "invalid app mention path".to_string())?;
            if !path.starts_with("app://") || path.len() <= "app://".len() {
                return Err("invalid app mention path".to_string());
            }
            if !seen_paths.insert(path.to_string()) {
                continue;
            }
            let file_path = &path["app://".len()..];
            // Read file content and include as text context.
            match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    parts.push(json!({
                        "type": "text",
                        "text": format!("--- {name} ({file_path}) ---\n{content}")
                    }));
                }
                Err(_) => {
                    parts.push(json!({
                        "type": "text",
                        "text": format!("[file: {name} at {file_path}]")
                    }));
                }
            }
        }
    }
    if let Some(mentions) = agent_mentions {
        for mention in mentions {
            let object = mention
                .as_object()
                .ok_or_else(|| "invalid agent mention payload".to_string())?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "invalid agent mention name".to_string())?;
            parts.push(json!({
                "type": "agent",
                "name": name
            }));
        }
    }
    if parts.is_empty() {
        return Err("empty user message".to_string());
    }
    Ok(parts)
}

fn mime_from_extension(path: &str) -> &str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

fn ip_is_disallowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 255 && octets[1] == 255 && octets[2] == 255 && octets[3] == 255)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

async fn validate_public_image_url(url: &reqwest::Url) -> Result<(), String> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("Image URL must use http or https.".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "Image URL must include a host.".to_string())?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
    {
        return Err(
            "Blocked image URL host: localhost and local domains are not allowed.".to_string(),
        );
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip_is_disallowed(ip) {
            return Err(
                "Blocked image URL host: private or local network addresses are not allowed."
                    .to_string(),
            );
        }
        return Ok(());
    }

    let port = url
        .port_or_known_default()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let resolved = timeout(
        URL_IMAGE_FETCH_TIMEOUT,
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| "Timed out resolving image URL host.".to_string())
    .and_then(|result| result.map_err(|err| format!("Failed to resolve image URL host: {err}")))?;

    let mut saw_address = false;
    for socket_addr in resolved {
        saw_address = true;
        if ip_is_disallowed(socket_addr.ip()) {
            return Err(
                "Blocked image URL host: private or local network addresses are not allowed."
                    .to_string(),
            );
        }
    }

    if !saw_address {
        return Err("Image URL host did not resolve to any addresses.".to_string());
    }

    Ok(())
}

/// Fetch an image URL and return it as a REST file part.
async fn fetch_url_image_as_file_part(url: &str) -> Result<Value, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid image URL.".to_string())?;
    validate_public_image_url(&parsed).await?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(URL_IMAGE_FETCH_TIMEOUT)
        .build()
        .map_err(|err| format!("Failed to initialize image downloader: {err}"))?;

    let response = client.get(parsed.clone()).send().await.map_err(|err| {
        if err.is_timeout() {
            "Timed out while fetching image URL.".to_string()
        } else {
            format!("Failed to fetch image URL: {err}")
        }
    })?;

    if !response.status().is_success() {
        return Err(format!(
            "Image URL request failed with status {}.",
            response.status()
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Image URL must return an image Content-Type header.".to_string())?;
    let mime_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !mime_type.starts_with("image/") {
        return Err("Image URL must return an image Content-Type header.".to_string());
    }

    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|err| format!("Failed reading image bytes: {err}"))?;
        if bytes.len() + chunk.len() > URL_IMAGE_MAX_BYTES {
            return Err(format!(
                "Image URL exceeds max allowed size of {} bytes.",
                URL_IMAGE_MAX_BYTES
            ));
        }
        bytes.extend_from_slice(&chunk);
    }

    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let filename = parsed
        .path_segments()
        .and_then(|segs| segs.last())
        .filter(|s| !s.is_empty())
        .unwrap_or("image");
    Ok(json!({
        "type": "file",
        "mime": mime_type,
        "url": format!("data:{mime_type};base64,{encoded}"),
        "filename": filename
    }))
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn provider_has_model(config_providers: &Value, provider_id: &str, model_id: &str) -> bool {
    let provider_id = provider_id.trim();
    let model_id = model_id.trim();
    if provider_id.is_empty() || model_id.is_empty() {
        return false;
    }

    let providers = config_providers
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    providers.iter().any(|provider| {
        let pid = provider
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default();
        if pid != provider_id {
            return false;
        }
        let models = provider.get("models").and_then(|v| v.as_object());
        if let Some(models_map) = models {
            if models_map.contains_key(model_id) {
                return true;
            }
            return models_map.values().any(|model| {
                model
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or_default()
                    == model_id
            });
        }
        false
    })
}

fn resolve_model_override_from_providers(
    config_providers: &Value,
    requested_model: &str,
) -> Option<Value> {
    let requested = requested_model.trim();
    if requested.is_empty() {
        return None;
    }

    if let Some((provider_id, model_id)) = requested.split_once('/') {
        let provider_id = provider_id.trim();
        let model_id = model_id.trim();
        if provider_id.is_empty() || model_id.is_empty() {
            return None;
        }
        if provider_has_model(config_providers, provider_id, model_id) {
            return Some(json!({ "providerID": provider_id, "modelID": model_id }));
        }
        return None;
    }

    let providers = config_providers
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut matches: Vec<String> = Vec::new();

    for provider in &providers {
        let provider_id = provider
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default();
        if provider_id.is_empty() {
            continue;
        }
        let Some(models_map) = provider.get("models").and_then(|v| v.as_object()) else {
            continue;
        };
        let found = models_map.contains_key(requested)
            || models_map.values().any(|model| {
                model
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or_default()
                    == requested
            });
        if found {
            matches.push(provider_id.to_string());
        }
    }

    if matches.len() == 1 {
        return Some(json!({ "providerID": matches[0], "modelID": requested }));
    }

    None
}

async fn resolve_prompt_model_override(
    session: &WorkspaceSession,
    requested_model: &str,
) -> Option<Value> {
    let requested = requested_model.trim();
    if requested.is_empty() {
        return None;
    }

    let has_provider = requested.contains('/');

    let cached = session.models_cache.lock().await.clone();
    if let Some(cache) = cached {
        if let Some(model) = resolve_model_override_from_providers(&cache, requested) {
            return Some(model);
        }
        // If the client sent a qualified id but it's no longer present in the
        // current provider list, silently fall back to server default instead
        // of sending an invalid override that yields a scary 400.
        if has_provider {
            return None;
        }
    }

    // Best-effort refresh for legacy unqualified ids when cache is stale/missing.
    if !has_provider {
        if let Ok(fresh) = session.rest_get("/config/providers").await {
            *session.models_cache.lock().await = Some(fresh.clone());
            if let Some(model) = resolve_model_override_from_providers(&fresh, requested) {
                return Some(model);
            }
        }
    }

    // If already qualified and no cache was available to validate it, keep the
    // explicit override rather than dropping a likely-valid user choice.
    if let Some((provider_id, model_id)) = requested.split_once('/') {
        let provider_id = provider_id.trim();
        let model_id = model_id.trim();
        if !provider_id.is_empty() && !model_id.is_empty() {
            return Some(json!({ "providerID": provider_id, "modelID": model_id }));
        }
    }

    None
}

fn emit_turn_error<E: EventSink>(
    event_sink: &E,
    workspace_id: &str,
    thread_id: &str,
    turn_id: &str,
    message: &str,
) {
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: workspace_id.to_string(),
        message: json!({
            "method": "error",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "willRetry": false,
                "error": {
                    "message": message
                }
            }
        }),
    });
}

static TURN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Send user message (REST: fire-and-forget via prompt_async)
// ---------------------------------------------------------------------------

pub(crate) async fn send_user_message_core<E: EventSink>(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    text: String,
    model: Option<String>,
    effort: Option<String>,
    _access_mode: Option<String>,
    images: Option<Vec<String>>,
    app_mentions: Option<Vec<Value>>,
    agent_mentions: Option<Vec<Value>>,
    collaboration_mode: Option<Value>,
    event_sink: &E,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let user_text = text.trim().to_string();
    let user_images = images.clone().unwrap_or_default();
    let parts = build_rest_prompt_parts(text, images, app_mentions, agent_mentions).await?;
    let _prompt_guard = session.prompt_lock.lock().await;

    // Synthesize turn ID and prepare translation state for this session.
    let turn_n = TURN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let turn_id = format!("turn_{turn_n}");
    {
        let mut ts = session.translation_state.lock().await;
        ts.start_turn(thread_id.clone(), turn_id.clone());
    }

    // Emit synthetic turn/started.
    let started_msg = event_translator::build_turn_started(&thread_id, &turn_id);
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: workspace_id.clone(),
        message: started_msg,
    });

    // Emit synthetic user message item using the translator's stable ID so that
    // the later SSE-driven emission (from translate_part_updated) merges by ID
    // instead of creating a duplicate.
    if !user_text.is_empty() || !user_images.is_empty() {
        let mut content_parts: Vec<Value> = Vec::new();
        if !user_text.is_empty() {
            content_parts.push(json!({ "type": "text", "text": user_text }));
        }
        for image in user_images {
            let trimmed = image.trim();
            if trimmed.is_empty() {
                continue;
            }
            content_parts.push(json!({
                "type": "image",
                "value": frontend_image_value(trimmed)
            }));
        }
        if !content_parts.is_empty() {
            let user_item_id = {
                let mut ts = session.translation_state.lock().await;
                ts.user_message_item(&thread_id)
            };
            event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: workspace_id.clone(),
                message: json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": thread_id,
                        "item": {
                            "id": user_item_id,
                            "type": "userMessage",
                            "content": content_parts
                        }
                    }
                }),
            });
        }
    }

    // Build prompt body. Model selection is per-message in REST.
    let mut body = json!({
        "parts": parts
    });
    let requested_model = normalize_optional_string(model);
    let requested_effort = normalize_optional_string(effort);
    if let Some(ref model_id) = requested_model {
        // Be resilient to stale or legacy (unqualified) persisted selections.
        // If we cannot safely resolve a valid `{ providerID, modelID }`, omit the
        // override and let OpenCode use its current default model.
        if let Some(model_override) =
            resolve_prompt_model_override(session.as_ref(), model_id).await
        {
            body["model"] = model_override;
        }
    }
    if let Some(ref effort_level) = requested_effort {
        body["effort"] = json!(effort_level);
    }
    if let Some(agent_name) = collaboration_agent_name_from_payload(collaboration_mode.as_ref()) {
        body["agent"] = json!(agent_name);
    }

    // Fire-and-forget: POST /session/:id/prompt_async → 204.
    // Turn completion comes from SSE `session.status` → idle.
    let path = format!("/session/{thread_id}/prompt_async");
    let result = session.rest_post(&path, body).await;

    if let Err(ref error) = result {
        emit_turn_error(event_sink, &workspace_id, &thread_id, &turn_id, error);
    }

    // Return immediately — SSE events will drive the rest of the turn.
    Ok(json!({
        "result": {
            "turn": { "id": turn_id }
        }
    }))
}

// ---------------------------------------------------------------------------
// Turn interrupt (REST: POST /session/:id/abort)
// ---------------------------------------------------------------------------

pub(crate) async fn turn_interrupt_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    _turn_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let path = format!("/session/{thread_id}/abort");
    session.rest_post(&path, json!({})).await?;
    Ok(json!({ "ok": true }))
}

pub(crate) async fn turn_steer_core(
    _sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    _workspace_id: String,
    _thread_id: String,
    _turn_id: String,
    _text: String,
    _images: Option<Vec<String>>,
    _app_mentions: Option<Vec<Value>>,
) -> Result<Value, String> {
    Err("turn steering is not supported yet".to_string())
}

pub(crate) async fn collaboration_mode_list_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    let agents = match session.rest_get("/agent").await {
        Ok(response) => response,
        Err(_) => return Ok(json!({ "result": { "data": [] } })),
    };

    let agent_list = agents.as_array().cloned().unwrap_or_default();

    let data: Vec<Value> = agent_list
        .iter()
        .filter_map(collaboration_mode_entry_from_agent)
        .collect();

    Ok(json!({ "result": { "data": data } }))
}

pub(crate) async fn agent_list_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    let agents = match session.rest_get("/agent").await {
        Ok(response) => response,
        Err(_) => return Ok(json!({ "result": { "data": [] } })),
    };

    let agent_list = agents.as_array().cloned().unwrap_or_default();

    let data: Vec<Value> = agent_list
        .iter()
        .filter_map(agent_mention_entry_from_agent)
        .collect();

    Ok(json!({ "result": { "data": data } }))
}

pub(crate) async fn start_review_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    thread_id: String,
    target: Value,
    delivery: Option<String>,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let arguments = review_target_to_opencode_arguments(&target)?;

    let detached = delivery
        .as_deref()
        .map(str::trim)
        .map(|value| value.eq_ignore_ascii_case("detached"))
        .unwrap_or(false);

    let review_thread_id = if detached {
        let fork_path = format!("/session/{thread_id}/fork");
        let forked = session.rest_post(&fork_path, json!({})).await?;
        forked
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "failed to fork review thread".to_string())?
            .to_string()
    } else {
        thread_id.clone()
    };

    let command_path = format!("/session/{review_thread_id}/command");
    let body = json!({
        "command": "review",
        "arguments": arguments,
    });
    let _ = session.rest_post(&command_path, body).await?;

    Ok(json!({
        "result": {
            "reviewThreadId": review_thread_id
        }
    }))
}

// ---------------------------------------------------------------------------
// Model list (REST: GET /config/providers)
// ---------------------------------------------------------------------------

pub(crate) fn model_list_response_from_providers(providers: &Value) -> Value {
    // REST returns:
    //   providers: [{ id, models: { "model-id": { id, name, ... }, ... } }]
    //   default:   { "provider-id": "model-id", ... }
    let defaults = providers
        .get("default")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let provider_list = providers
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut data: Vec<Value> = Vec::new();
    for provider in &provider_list {
        let provider_id = provider
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let default_for_provider = defaults
            .get(provider_id)
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // `models` is a map { "model-id": { id, name, ... } }, not an array.
        let models_map = provider
            .get("models")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        for (model_key, model) in &models_map {
            let selectable_model_id = model_key.trim().to_string();
            let canonical_model_id = model
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let model_id = if selectable_model_id.is_empty() {
                canonical_model_id.clone()
            } else {
                selectable_model_id
            };
            if model_id.is_empty() {
                continue;
            }
            let display_name = model
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&model_id)
                .trim()
                .to_string();
            let qualified_id = format!("{provider_id}/{model_id}");
            let is_default =
                model_id == default_for_provider || canonical_model_id == default_for_provider;

            // Variants keys are reasoning effort levels (e.g. "low", "medium", "high", "max").
            let variants = model
                .get("variants")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            let efforts: Vec<Value> = variants
                .keys()
                .map(|k| json!({ "reasoningEffort": k, "description": "" }))
                .collect();
            let default_effort = ["medium", "high"]
                .iter()
                .find(|e| variants.contains_key(**e))
                .map(|e| json!(e))
                .unwrap_or(json!(null));

            data.push(json!({
                "id": qualified_id,
                "model": model_id,
                "provider": provider_id,
                "displayName": display_name,
                "description": "",
                "supportedReasoningEfforts": efforts,
                "defaultReasoningEffort": default_effort,
                "isDefault": is_default,
            }));
        }
    }

    json!({ "result": { "data": data } })
}

pub(crate) fn model_list_debug_from_providers(providers: &Value) -> Value {
    let defaults = providers
        .get("default")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let provider_list = providers
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let provider_summaries: Vec<Value> = provider_list
        .iter()
        .map(|provider| {
            let provider_id = provider
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let models_map = provider
                .get("models")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            let mut model_entries: Vec<Value> = models_map
                .iter()
                .map(|(key, model)| {
                    let nested_id = model
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    json!({
                        "key": key,
                        "nestedId": nested_id,
                        "name": model.get("name").cloned().unwrap_or(Value::Null),
                        "status": model.get("status").cloned().unwrap_or(Value::Null),
                        "providerID": model.get("providerID").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            model_entries.sort_by(|a, b| {
                a.get("key")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .cmp(b.get("key").and_then(|v| v.as_str()).unwrap_or_default())
            });

            json!({
                "id": provider_id,
                "name": provider.get("name").cloned().unwrap_or(Value::Null),
                "defaultFromConfig": defaults
                    .get(provider.get("id").and_then(|v| v.as_str()).unwrap_or_default())
                    .cloned()
                    .unwrap_or(Value::Null),
                "modelCount": model_entries.len(),
                "models": model_entries,
            })
        })
        .collect();

    let transformed = model_list_response_from_providers(providers);
    let transformed_data = transformed
        .get("result")
        .and_then(|v| v.get("data"))
        .cloned()
        .unwrap_or_else(|| json!([]));

    json!({
        "source": "/config/providers",
        "providerCount": provider_summaries.len(),
        "providers": provider_summaries,
        "transformedCount": transformed_data.as_array().map(|a| a.len()).unwrap_or(0),
        "transformed": transformed_data,
    })
}

pub(crate) async fn model_list_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    // Try to refresh from server; fall back to cache.
    let providers = match session.rest_get("/config/providers").await {
        Ok(fresh) => {
            *session.models_cache.lock().await = Some(fresh.clone());
            fresh
        }
        Err(_) => {
            let cache = session.models_cache.lock().await.clone();
            cache.unwrap_or(json!({}))
        }
    };

    let context_windows = event_translator::extract_model_context_windows(&providers);
    {
        let mut state = session.translation_state.lock().await;
        state.replace_model_context_windows(context_windows);
    }

    Ok(model_list_response_from_providers(&providers))
}

// ---------------------------------------------------------------------------
// Permission / Question response
// ---------------------------------------------------------------------------

pub(crate) async fn respond_to_server_request_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
    request_id: Value,
    result: Value,
) -> Result<(), String> {
    let session = get_session_clone(sessions, &workspace_id).await?;

    let composite = request_id.as_str().unwrap_or_default();
    let (_session_id, resource_id) = composite.split_once(':').unwrap_or((composite, ""));

    if let Some(answers) = result.get("answers") {
        let answers_array = transform_question_answers(answers);
        let body = json!({ "answers": answers_array });
        let path = format!("/question/{resource_id}/reply");
        session.rest_post_bool(&path, body).await?;
    } else if result
        .get("reject")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        // Question rejection: POST /question/:id/reject
        let path = format!("/question/{resource_id}/reject");
        session.rest_post_bool(&path, json!({})).await?;
    } else {
        // Permission response: POST /permission/:id/reply
        let decision = result
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("accept");
        let body = event_translator::build_permission_response(decision);
        let path = format!("/permission/{resource_id}/reply");
        session.rest_post_bool(&path, body).await?;
    }

    Ok(())
}

fn transform_question_answers(answers: &Value) -> Value {
    let Some(obj) = answers.as_object() else {
        return Value::Array(vec![]);
    };

    let mut entries: Vec<(usize, Vec<Value>)> = obj
        .iter()
        .filter_map(|(key, val)| {
            let idx = key.parse::<usize>().ok()?;
            let inner = val.get("answers")?.as_array()?;
            Some((idx, inner.clone()))
        })
        .collect();

    entries.sort_by_key(|(idx, _)| *idx);
    Value::Array(
        entries
            .into_iter()
            .map(|(_, arr)| Value::Array(arr))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Account / login stubs (unchanged)
// ---------------------------------------------------------------------------

pub(crate) async fn account_rate_limits_core(
    _sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    _workspace_id: String,
) -> Result<Value, String> {
    Ok(json!({ "result": {} }))
}

pub(crate) async fn account_read_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: String,
) -> Result<Value, String> {
    let _session = {
        let sessions = sessions.lock().await;
        sessions.get(&workspace_id).cloned()
    };

    let (entry, parent_entry) = resolve_workspace_and_parent(workspaces, &workspace_id).await?;
    let codex_home = resolve_workspace_codex_home(&entry, parent_entry.as_ref())
        .or_else(resolve_default_codex_home);
    let fallback = read_auth_account(codex_home);

    Ok(build_account_response(None, fallback))
}

pub(crate) async fn codex_login_core(
    _sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    _codex_login_cancels: &Mutex<HashMap<String, CodexLoginCancelState>>,
    _workspace_id: String,
) -> Result<Value, String> {
    Err("Login is not supported in-app. Run `opencode auth login` in Terminal.".to_string())
}

pub(crate) async fn codex_login_cancel_core(
    _sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    _codex_login_cancels: &Mutex<HashMap<String, CodexLoginCancelState>>,
    _workspace_id: String,
) -> Result<Value, String> {
    Ok(json!({ "canceled": false }))
}

pub(crate) async fn skills_list_core(
    sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    workspace_id: String,
) -> Result<Value, String> {
    let session = get_session_clone(sessions, &workspace_id).await?;
    let response = session.rest_get("/skill").await?;
    let skills = response
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|skill| {
            let name = skill
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            let path = skill
                .get("location")
                .or_else(|| skill.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let description = skill
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            Some(json!({
                "name": name,
                "path": path,
                "description": description
            }))
        })
        .collect::<Vec<_>>();

    Ok(json!({ "result": { "skills": skills } }))
}

pub(crate) async fn apps_list_core(
    _sessions: &Mutex<HashMap<String, Arc<WorkspaceSession>>>,
    _workspace_id: String,
    _cursor: Option<String>,
    _limit: Option<u32>,
    _thread_id: Option<String>,
) -> Result<Value, String> {
    Ok(json!({ "result": { "data": [], "nextCursor": null } }))
}

pub(crate) async fn remember_approval_rule_core(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: String,
    command: Vec<String>,
) -> Result<Value, String> {
    let command = command
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>();
    if command.is_empty() {
        return Err("empty command".to_string());
    }

    let codex_home = resolve_codex_home_for_workspace_core(workspaces, &workspace_id).await?;
    let rules_path = rules::default_rules_path(&codex_home);
    rules::append_prefix_rule(&rules_path, &command)?;

    Ok(json!({
        "ok": true,
        "rulesPath": rules_path,
    }))
}

pub(crate) async fn get_config_model_core(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    workspace_id: String,
) -> Result<Value, String> {
    let codex_home = resolve_codex_home_for_workspace_core(workspaces, &workspace_id).await?;
    let model = codex_config::read_config_model(Some(codex_home))?;
    Ok(json!({ "model": model }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{WorkspaceKind, WorkspaceSettings};
    use tokio::runtime::Builder;

    #[test]
    fn include_hidden_sessions_enabled_only_for_all_sort_key() {
        assert!(should_include_hidden_sessions(&Some("all".to_string())));
        assert!(should_include_hidden_sessions(&Some(" ALL ".to_string())));
        assert!(!should_include_hidden_sessions(&Some(
            "updated_at".to_string()
        )));
        assert!(!should_include_hidden_sessions(&None));
    }

    #[test]
    fn sort_replay_messages_orders_oldest_first_by_timestamp() {
        let mut messages = vec![
            json!({
                "id": "msg-newer",
                "time": { "created": 200 }
            }),
            json!({
                "id": "msg-older",
                "time": { "created": 100 }
            }),
            json!({
                "id": "msg-middle",
                "createdAt": 150
            }),
        ];

        sort_replay_messages_chronologically(&mut messages);

        let ids: Vec<String> = messages
            .iter()
            .filter_map(|msg| {
                msg.get("id")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned)
            })
            .collect();
        assert_eq!(ids, vec!["msg-older", "msg-middle", "msg-newer"]);
    }

    #[test]
    fn last_revertable_message_id_selects_latest_message_after_sorting() {
        let messages = vec![
            json!({ "info": { "id": "msg_new", "time": { "created": 30 } } }),
            json!({ "info": { "id": "msg_old", "time": { "created": 10 } } }),
            json!({ "info": { "id": "msg_mid", "createdAt": 20 } }),
        ];

        let id = last_revertable_message_id(&messages);
        assert_eq!(id.as_deref(), Some("msg_new"));
    }

    #[test]
    fn last_revertable_message_id_skips_entries_without_message_info_id() {
        let messages = vec![
            json!({ "info": { "time": { "created": 10 } } }),
            json!({ "parts": [] }),
        ];

        assert_eq!(last_revertable_message_id(&messages), None);
    }

    #[test]
    fn replay_filters_messages_after_pending_revert_message() {
        let mut messages = vec![
            json!({ "info": { "id": "m1", "time": { "created": 10 } }, "parts": [] }),
            json!({ "info": { "id": "m2", "time": { "created": 20 } }, "parts": [] }),
            json!({ "info": { "id": "m3", "time": { "created": 30 } }, "parts": [] }),
        ];

        apply_pending_revert_to_replay_messages(
            &mut messages,
            Some(&json!({ "revert": { "messageID": "m2" } })),
        );

        let ids: Vec<&str> = messages
            .iter()
            .filter_map(|entry| entry.get("info")?.get("id")?.as_str())
            .collect();
        assert_eq!(ids, vec!["m1"]);
    }

    #[test]
    fn replay_truncates_parts_for_pending_part_revert() {
        let mut messages = vec![json!({
            "info": { "id": "m1", "time": { "created": 10 } },
            "parts": [
                { "id": "p1", "type": "text", "text": "a" },
                { "id": "p2", "type": "text", "text": "b" },
                { "id": "p3", "type": "text", "text": "c" }
            ]
        })];

        apply_pending_revert_to_replay_messages(
            &mut messages,
            Some(&json!({ "revert": { "messageID": "m1", "partID": "p2" } })),
        );

        let part_ids: Vec<&str> = messages[0]
            .get("parts")
            .and_then(|v| v.as_array())
            .expect("parts array")
            .iter()
            .filter_map(|part| part.get("id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(part_ids, vec!["p1"]);
    }

    #[test]
    fn private_and_loopback_ips_are_disallowed() {
        assert!(ip_is_disallowed(
            "127.0.0.1".parse::<IpAddr>().expect("parse loopback")
        ));
        assert!(ip_is_disallowed(
            "10.0.0.1".parse::<IpAddr>().expect("parse private")
        ));
        assert!(!ip_is_disallowed(
            "8.8.8.8".parse::<IpAddr>().expect("parse public")
        ));
    }

    #[test]
    fn build_rest_prompt_parts_blocks_localhost_image_urls() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let result = build_rest_prompt_parts(
                "hello".to_string(),
                Some(vec!["http://localhost/image.png".to_string()]),
                None,
                None,
            )
            .await;

            let error = result.expect_err("localhost URL should be blocked");
            assert!(error.contains("Blocked image URL host"));
        });
    }

    #[test]
    fn frontend_image_value_materializes_data_urls_to_temp_files() {
        let data_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jXioAAAAASUVORK5CYII=";

        let path = frontend_image_value(data_url);
        let repeated = frontend_image_value(data_url);

        assert!(!path.starts_with("data:"));
        assert_eq!(path, repeated);
        assert!(PathBuf::from(&path).exists());
    }

    #[test]
    fn hidden_session_ids_are_read_from_workspace_settings() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let workspaces = Mutex::new(HashMap::from([(
                "ws-1".to_string(),
                WorkspaceEntry {
                    id: "ws-1".to_string(),
                    name: "Workspace".to_string(),
                    path: "/tmp/ws-1".to_string(),
                    codex_bin: None,
                    kind: WorkspaceKind::Main,
                    parent_id: None,
                    worktree: None,
                    settings: WorkspaceSettings {
                        hidden_session_ids: vec!["ses_bg_1".to_string(), "ses_bg_2".to_string()],
                        ..WorkspaceSettings::default()
                    },
                },
            )]));

            let hidden = hidden_session_ids_for_workspace(&workspaces, "ws-1").await;
            assert!(hidden.contains("ses_bg_1"));
            assert!(hidden.contains("ses_bg_2"));
            assert!(!hidden.contains("ses_fg"));
        });
    }

    #[test]
    fn review_target_to_opencode_arguments_maps_known_targets() {
        assert_eq!(
            review_target_to_opencode_arguments(&json!({ "type": "uncommittedChanges" }))
                .expect("uncommitted"),
            ""
        );
        assert_eq!(
            review_target_to_opencode_arguments(&json!({
                "type": "baseBranch",
                "branch": "main"
            }))
            .expect("base branch"),
            "main"
        );
        assert_eq!(
            review_target_to_opencode_arguments(&json!({
                "type": "commit",
                "sha": "abc123"
            }))
            .expect("commit"),
            "abc123"
        );
        assert_eq!(
            review_target_to_opencode_arguments(&json!({
                "type": "custom",
                "instructions": "focus on tests"
            }))
            .expect("custom"),
            "focus on tests"
        );
    }

    #[test]
    fn session_to_thread_entry_includes_parent_and_timestamps() {
        let entry = session_to_thread_entry(&json!({
            "id": "ses_1",
            "title": "Example",
            "directory": "/tmp/work",
            "parentID": "ses_parent",
            "time": { "created": 10, "updated": 20 }
        }))
        .expect("thread entry");

        assert_eq!(entry.get("id").and_then(|v| v.as_str()), Some("ses_1"));
        assert_eq!(entry.get("name").and_then(|v| v.as_str()), Some("Example"));
        assert_eq!(
            entry.get("parentId").and_then(|v| v.as_str()),
            Some("ses_parent")
        );
        assert_eq!(entry.get("createdAt").and_then(|v| v.as_u64()), Some(10));
        assert_eq!(entry.get("updatedAt").and_then(|v| v.as_u64()), Some(20));
    }

    #[test]
    fn collaboration_agent_name_from_payload_reads_mode_string() {
        let agent = collaboration_agent_name_from_payload(Some(&json!({
            "mode": "explore",
            "settings": {}
        })));
        assert_eq!(agent.as_deref(), Some("explore"));
    }

    #[test]
    fn collaboration_agent_name_from_payload_ignores_missing_or_empty_mode() {
        assert_eq!(
            collaboration_agent_name_from_payload(Some(&json!({ "settings": {} }))),
            None
        );
        assert_eq!(
            collaboration_agent_name_from_payload(Some(&json!({ "mode": "   " }))),
            None
        );
    }

    #[test]
    fn collaboration_mode_entry_from_agent_keeps_primary_capable_and_skips_subagent_hidden() {
        let subagent = collaboration_mode_entry_from_agent(&json!({
            "name": "explore",
            "mode": "subagent",
            "hidden": false,
            "description": "Explore-only agent"
        }));
        assert!(subagent.is_none());

        let primary = collaboration_mode_entry_from_agent(&json!({
            "name": "build",
            "mode": "primary",
            "hidden": false,
            "description": "Primary agent"
        }))
        .expect("primary agent should be included");
        assert_eq!(primary["mode"], "build");
        assert_eq!(primary["label"], "Build");

        let dual_role = collaboration_mode_entry_from_agent(&json!({
            "name": "general",
            "mode": "all",
            "hidden": false
        }))
        .expect("all-mode agent should be included");
        assert_eq!(dual_role["mode"], "general");

        let hidden = collaboration_mode_entry_from_agent(&json!({
            "name": "summary",
            "mode": "primary",
            "hidden": true
        }));
        assert!(hidden.is_none());
    }

    #[test]
    fn replay_task_tool_maps_to_collab_tool_call_item() {
        assert_eq!(replay_tool_kind_to_item_type("task"), "collabToolCall");
        let item = replay_build_tool_item(
            "replay_item_1",
            "ses_parent",
            "collabToolCall",
            "task",
            "completed",
            Some(&json!({
                "description": "Explore the codebase",
                "prompt": "fallback prompt",
                "subagent_type": "explore"
            })),
            "",
        );
        assert_eq!(item["type"], "collabToolCall");
        assert_eq!(item["senderThreadId"], "ses_parent");
        assert_eq!(item["prompt"], "Explore the codebase");
        assert_eq!(item["agentStatus"]["explore"]["status"], "completed");
    }

    #[test]
    fn resume_thread_result_thread_includes_parent_from_session_details() {
        let thread = resume_thread_result_thread(
            "ses_child",
            Some(&json!({
                "id": "ses_child",
                "title": "Child",
                "directory": "/tmp/ws",
                "parentID": "ses_parent",
                "time": { "created": 1, "updated": 2 }
            })),
        );
        assert_eq!(thread["id"], "ses_child");
        assert_eq!(thread["parentId"], "ses_parent");
        assert_eq!(thread["preview"], "Child");
    }

    #[test]
    fn model_list_response_preserves_provider_model_map_keys() {
        let providers = json!({
            "providers": [
                {
                    "id": "cliproxy",
                    "models": {
                        "claude-opus-4-5": {
                            "id": "claude-opus-4-5-20250929",
                            "name": "Claude Opus 4.5 (CLIProxy)",
                            "variants": {}
                        },
                        "claude-opus-4-6": {
                            "id": "claude-opus-4-6-20251001",
                            "name": "Claude Opus 4.6 (CLIProxy)",
                            "variants": {}
                        }
                    }
                }
            ],
            "default": {
                "cliproxy": "claude-opus-4-6-20251001"
            }
        });

        let response = model_list_response_from_providers(&providers);
        let data = response
            .get("result")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_array())
            .expect("data array");

        let rows: Vec<(String, String, bool)> = data
            .iter()
            .map(|item| {
                (
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    item.get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    item.get("isDefault")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                )
            })
            .collect();

        assert!(rows.contains(&(
            "cliproxy/claude-opus-4-5".to_string(),
            "claude-opus-4-5".to_string(),
            false
        )));
        assert!(rows.contains(&(
            "cliproxy/claude-opus-4-6".to_string(),
            "claude-opus-4-6".to_string(),
            true
        )));
    }
}
