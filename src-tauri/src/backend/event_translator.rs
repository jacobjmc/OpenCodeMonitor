//! Translates OpenCode REST SSE events into CodexMonitor-shaped
//! `AppServerEvent` messages that the React frontend already knows how to
//! consume.
//!
//! Design invariant (INV4 from spec): the frontend thread reducer receives
//! events in the **same shape** as the original CodexMonitor protocol. All
//! OpenCode ↔ CodexMonitor translation happens here in Rust.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shared::diff_utils::{generate_apply_patch_changes, generate_edit_diff};

/// Per-session turn state — tracks active turn and item IDs for a single session.
#[derive(Default)]
struct PerSessionTurnState {
    /// Synthesized turn ID for this session.
    turn_id: String,
    /// Map tool Part `id` → synthesized CodexMonitor `itemId`.
    tool_call_items: HashMap<String, String>,
    /// Stable item ID for the current agent-message stream.
    agent_message_item_id: Option<String>,
    /// OpenCode part ID for the current text part (used to handle part removals/reset).
    agent_message_part_id: Option<String>,
    /// Number of bytes already emitted for the current agent text part.
    agent_message_text_len: usize,
    /// Stable item ID for the current reasoning stream.
    reasoning_item_id: Option<String>,
    /// OpenCode part ID for the current reasoning part (used to route `message.part.delta` events).
    reasoning_part_id: Option<String>,
    /// Number of bytes already emitted for the current reasoning part.
    reasoning_text_len: usize,
    /// Stable item ID for the current user-message being assembled from SSE chunks.
    user_message_item_id: Option<String>,
    /// Buffered text for the current user-message being assembled from SSE chunks.
    user_message_text: String,
}

/// Per-workspace state the translator needs to synthesize IDs the frontend
/// expects but the REST SSE protocol does not provide (turn IDs, monotonic
/// item IDs, etc.).
///
/// Multiple sessions can be active in a single workspace (e.g., main session +
/// subagent sessions), so turn state is tracked per session_id.
pub(crate) struct SessionTranslationState {
    /// The most recently active session ID (maps to CodexMonitor's "threadId").
    /// Used as a fallback when events don't specify sessionID.
    pub(crate) session_id: String,
    /// Per-session turn state — allows concurrent sessions to track their own turns.
    session_turns: HashMap<String, PerSessionTurnState>,
    /// Counter for synthesizing unique item IDs (shared across all sessions).
    item_counter: AtomicU64,
    /// Tracks OpenCode message roles by message ID to avoid routing user text
    /// parts into assistant message deltas.
    message_roles: HashMap<String, String>,
    /// Model context windows keyed by `providerID/modelID`.
    model_context_windows: HashMap<String, u64>,
}

impl SessionTranslationState {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            session_id,
            session_turns: HashMap::new(),
            item_counter: AtomicU64::new(1),
            message_roles: HashMap::new(),
            model_context_windows: HashMap::new(),
        }
    }

    pub(crate) fn replace_model_context_windows(&mut self, windows: HashMap<String, u64>) {
        self.model_context_windows = windows;
    }

    pub(crate) fn model_context_window(&self, provider_id: &str, model_id: &str) -> Option<u64> {
        let provider = provider_id.trim();
        let model = model_id.trim();
        if provider.is_empty() && model.is_empty() {
            return None;
        }

        if !provider.is_empty() && !model.is_empty() {
            let key = format!("{provider}/{model}");
            if let Some(value) = self.model_context_windows.get(&key) {
                return Some(*value);
            }
        }

        if model.contains('/') {
            if let Some(value) = self.model_context_windows.get(model) {
                return Some(*value);
            }
            if let Some((model_provider, model_id_only)) = model.split_once('/') {
                let qualified = format!("{model_provider}/{model_id_only}");
                if let Some(value) = self.model_context_windows.get(&qualified) {
                    return Some(*value);
                }
                if !provider.is_empty() {
                    let fallback = format!("{provider}/{model_id_only}");
                    if let Some(value) = self.model_context_windows.get(&fallback) {
                        return Some(*value);
                    }
                }
            }
        }

        None
    }

    fn next_item_id(&self) -> String {
        let n = self.item_counter.fetch_add(1, Ordering::SeqCst);
        format!("item_{n}")
    }

    fn next_turn_id(&self) -> String {
        let n = self.item_counter.fetch_add(1, Ordering::SeqCst);
        format!("turn_rest_{n}")
    }

    /// Start a new turn for a specific session.
    ///
    /// INVARIANT: User message ID is always lowest in a turn. If not already set,
    /// we pre-allocate it here so any subsequent tool IDs are guaranteed higher.
    pub(crate) fn start_turn(&mut self, session_id: String, turn_id: String) {
        self.session_id = session_id.clone();
        let turn_state = self.session_turns.entry(session_id.clone()).or_default();
        // User prompt parts can arrive before `session.status=active` (common for
        // subagent sessions). Preserve the in-progress user message so later chunks
        // keep merging into the same frontend item instead of creating a duplicate.
        let preserved_user_message_item_id = turn_state.user_message_item_id.clone();
        let preserved_user_message_text = turn_state.user_message_text.clone();
        turn_state.turn_id = turn_id;
        turn_state.tool_call_items.clear();
        turn_state.agent_message_item_id = None;
        turn_state.agent_message_part_id = None;
        turn_state.agent_message_text_len = 0;
        turn_state.reasoning_item_id = None;
        turn_state.reasoning_part_id = None;
        turn_state.reasoning_text_len = 0;

        // INVARIANT: Pre-allocate user message ID if not already set.
        // This ensures user message ID is always lower than any tool IDs in this turn.
        if let Some(id) = preserved_user_message_item_id {
            let turn_state = self.session_turns.get_mut(&session_id).unwrap();
            turn_state.user_message_item_id = Some(id);
            turn_state.user_message_text = preserved_user_message_text;
        } else {
            let pre_allocated_id = self.next_item_id();
            let turn_state = self.session_turns.get_mut(&session_id).unwrap();
            turn_state.user_message_item_id = Some(pre_allocated_id);
            turn_state.user_message_text = String::new();
        }
    }

    /// Prepare translation state for replaying historical messages.
    pub(crate) fn prepare_replay(&mut self, session_id: String) {
        self.session_id = session_id.clone();
        if let Some(turn_state) = self.session_turns.get_mut(&session_id) {
            turn_state.turn_id.clear();
            turn_state.tool_call_items.clear();
            turn_state.agent_message_item_id = None;
            turn_state.reasoning_item_id = None;
            turn_state.reasoning_part_id = None;
            turn_state.reasoning_text_len = 0;
            turn_state.agent_message_part_id = None;
            turn_state.agent_message_text_len = 0;
            // Preserve user_message_item_id so replay reuses the same ID
            // the live SSE translator already emitted (prevents duplicates).
            turn_state.user_message_text.clear();
        }
    }

    fn get_turn_state(&self, session_id: &str) -> Option<&PerSessionTurnState> {
        self.session_turns.get(session_id)
    }

    fn get_turn_state_mut(&mut self, session_id: &str) -> &mut PerSessionTurnState {
        self.session_turns
            .entry(session_id.to_string())
            .or_default()
    }

    fn agent_message_item(&mut self, session_id: &str) -> String {
        let turn_state = self.get_turn_state_mut(session_id);
        if let Some(ref id) = turn_state.agent_message_item_id {
            id.clone()
        } else {
            let id = self.next_item_id();
            let turn_state = self.get_turn_state_mut(session_id);
            turn_state.agent_message_item_id = Some(id.clone());
            id
        }
    }

    fn reasoning_item(&mut self, session_id: &str) -> String {
        let turn_state = self.get_turn_state_mut(session_id);
        if let Some(ref id) = turn_state.reasoning_item_id {
            id.clone()
        } else {
            let id = self.next_item_id();
            let turn_state = self.get_turn_state_mut(session_id);
            turn_state.reasoning_item_id = Some(id.clone());
            id
        }
    }

    fn reset_agent_message_item(&mut self, session_id: &str) {
        let turn_state = self.get_turn_state_mut(session_id);
        turn_state.agent_message_item_id = None;
        turn_state.agent_message_part_id = None;
        turn_state.agent_message_text_len = 0;
    }

    fn remove_part_mapping(&mut self, session_id: &str, part_id: &str) {
        if part_id.is_empty() {
            return;
        }
        let turn_state = self.get_turn_state_mut(session_id);
        turn_state.tool_call_items.remove(part_id);
        if turn_state.reasoning_part_id.as_deref() == Some(part_id) {
            turn_state.reasoning_part_id = None;
            turn_state.reasoning_item_id = None;
            turn_state.reasoning_text_len = 0;
        }
        if turn_state.agent_message_part_id.as_deref() == Some(part_id) {
            turn_state.agent_message_part_id = None;
            turn_state.agent_message_item_id = None;
            turn_state.agent_message_text_len = 0;
        }
    }

    fn finish_turn(&mut self, session_id: &str) {
        if let Some(turn_state) = self.session_turns.get_mut(session_id) {
            turn_state.turn_id.clear();
            turn_state.tool_call_items.clear();
            turn_state.agent_message_item_id = None;
            turn_state.agent_message_part_id = None;
            turn_state.agent_message_text_len = 0;
            turn_state.reasoning_item_id = None;
            turn_state.reasoning_part_id = None;
            turn_state.reasoning_text_len = 0;
            turn_state.user_message_item_id = None;
            turn_state.user_message_text.clear();
        }
    }

    pub(crate) fn user_message_item(&mut self, session_id: &str) -> String {
        let turn_state = self.get_turn_state_mut(session_id);
        if let Some(ref id) = turn_state.user_message_item_id {
            id.clone()
        } else {
            let id = self.next_item_id();
            let turn_state = self.get_turn_state_mut(session_id);
            turn_state.user_message_item_id = Some(id.clone());
            id
        }
    }

    pub(crate) fn mark_new_replayed_user_message_boundary(&mut self) {
        if let Some(turn_state) = self.session_turns.get_mut(&self.session_id) {
            turn_state.tool_call_items.clear();
            turn_state.agent_message_item_id = None;
            turn_state.agent_message_part_id = None;
            turn_state.agent_message_text_len = 0;
            turn_state.reasoning_item_id = None;
            turn_state.reasoning_part_id = None;
            turn_state.reasoning_text_len = 0;
            turn_state.user_message_item_id = None;
            turn_state.user_message_text.clear();
        }
    }

    fn remember_message_role(&mut self, message_id: &str, role: &str) {
        let message_id = message_id.trim();
        let role = role.trim();
        if message_id.is_empty() || role.is_empty() {
            return;
        }
        self.message_roles
            .insert(message_id.to_string(), role.to_string());
    }

    fn is_user_message_id(&self, message_id: &str) -> bool {
        self.message_roles
            .get(message_id.trim())
            .map(|role| role == "user")
            .unwrap_or(false)
    }
}

fn unseen_suffix<'a>(full_text: &'a str, emitted_len: usize) -> Option<&'a str> {
    if full_text.is_empty() {
        return None;
    }
    if emitted_len == 0 {
        return Some(full_text);
    }
    if full_text.len() <= emitted_len {
        return None;
    }
    full_text
        .get(emitted_len..)
        .filter(|suffix| !suffix.is_empty())
}

fn parse_u64(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().and_then(|n| (n > 0).then_some(n as u64)))
        })
        .or_else(|| {
            value
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
        })
}

pub(crate) fn extract_model_context_windows(config_providers: &Value) -> HashMap<String, u64> {
    let mut windows = HashMap::new();

    let providers = config_providers
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for provider in &providers {
        let provider_id = provider
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default();
        if provider_id.is_empty() {
            continue;
        }

        let Some(models) = provider.get("models").and_then(|v| v.as_object()) else {
            continue;
        };

        for (fallback_model_id, model) in models {
            let model_id = model
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_model_id.as_str());
            if model_id.is_empty() {
                continue;
            }

            let context = parse_u64(model.get("limit").and_then(|v| v.get("context")));
            if let Some(context) = context {
                let key = format!("{provider_id}/{model_id}");
                windows.insert(key, context);
            }
        }
    }

    windows
}

// ---------------------------------------------------------------------------
// Public translation entry points
// ---------------------------------------------------------------------------

/// Translate an OpenCode REST SSE event into one or more CodexMonitor-shaped
/// JSON-RPC messages (method + params).
///
/// SSE events have shape: `{ type: "<event_type>", properties: { ... } }`
///
/// Returns an empty Vec when the event should be silently dropped.
pub(crate) fn translate_sse_event(
    sse_event: &Value,
    state: &mut SessionTranslationState,
) -> Vec<Value> {
    let event_type = match sse_event.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return vec![],
    };
    let properties = sse_event.get("properties").unwrap_or(&Value::Null);

    match event_type {
        "message.part.updated" => translate_part_updated(properties, state),
        "message.part.removed" => translate_part_removed(properties, state),
        "message.part.delta" => translate_part_delta(properties, state),
        "message.updated" => translate_message_updated(properties, state),
        "session.created" => translate_session_created_or_updated(properties, state, true),
        "session.updated" => translate_session_created_or_updated(properties, state, true),
        "session.status" => translate_session_status(properties, state),
        "session.idle" => translate_session_idle(properties, state),
        "session.error" => translate_session_error(properties, state),
        "permission.asked" => translate_sse_permission(properties, state),
        "question.asked" => translate_question_asked(properties, state),
        "question.replied" => translate_question_completed(properties),
        "question.rejected" => translate_question_completed(properties),
        "todo.updated" => translate_todo_updated(properties, state),
        "server.heartbeat"
        | "file.watcher.updated"
        | "session.deleted"
        | "session.diff"
        | "config.updated" => vec![],
        _ => {
            #[cfg(debug_assertions)]
            eprintln!("[event_translator] unknown SSE event type: {event_type}");
            vec![]
        }
    }
}

fn session_record_from_properties<'a>(properties: &'a Value) -> Option<&'a Value> {
    if let Some(session) = properties.get("session") {
        Some(session)
    } else if properties.get("id").is_some() {
        Some(properties)
    } else {
        None
    }
}

fn session_id_from_record(session: &Value) -> String {
    session
        .get("id")
        .or_else(|| session.get("sessionID"))
        .or_else(|| session.get("sessionId"))
        .or_else(|| session.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn session_title_from_record(session: &Value) -> String {
    session
        .get("title")
        .or_else(|| session.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn session_parent_id_from_record(session: &Value) -> Option<String> {
    session
        .get("parentID")
        .or_else(|| session.get("parentId"))
        .or_else(|| session.get("parent_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn session_updated_at_from_record(session: &Value) -> Value {
    session
        .get("updatedAt")
        .or_else(|| session.get("updated_at"))
        .or_else(|| session.get("time").and_then(|time| time.get("updated")))
        .or_else(|| session.get("time").and_then(|time| time.get("updatedAt")))
        .or_else(|| session.get("time").and_then(|time| time.get("updated_at")))
        .cloned()
        .unwrap_or(Value::Null)
}

fn session_created_at_from_record(session: &Value) -> Value {
    session
        .get("createdAt")
        .or_else(|| session.get("created_at"))
        .or_else(|| session.get("time").and_then(|time| time.get("created")))
        .or_else(|| session.get("time").and_then(|time| time.get("createdAt")))
        .or_else(|| session.get("time").and_then(|time| time.get("created_at")))
        .cloned()
        .unwrap_or(Value::Null)
}

fn translate_session_created_or_updated(
    properties: &Value,
    state: &mut SessionTranslationState,
    emit_thread_started: bool,
) -> Vec<Value> {
    let Some(session) = session_record_from_properties(properties) else {
        return vec![];
    };
    let thread_id = session_id_from_record(session);
    if thread_id.is_empty() {
        return vec![];
    }

    state.session_id = thread_id.clone();
    let title = session_title_from_record(session);
    let mut events = Vec::new();

    if emit_thread_started {
        let mut thread = json!({
            "id": thread_id,
            "preview": title,
            "updatedAt": session_updated_at_from_record(session),
            "createdAt": session_created_at_from_record(session),
        });
        if let Some(parent_id) = session_parent_id_from_record(session) {
            thread["parentId"] = json!(parent_id);
        }
        events.push(json!({
            "method": "thread/started",
            "params": {
                "thread": thread
            }
        }));
    }

    if !title.is_empty() {
        events.push(json!({
            "method": "thread/name/updated",
            "params": {
                "threadId": thread_id,
                "threadName": title
            }
        }));
    }

    events
}

// ---------------------------------------------------------------------------
// message.part.updated — the main event for streaming content
// ---------------------------------------------------------------------------

fn translate_part_updated(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let part = match properties.get("part") {
        Some(p) => p,
        None => return vec![],
    };
    let delta = properties
        .get("delta")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let part_message_id = part
        .get("messageID")
        .or_else(|| part.get("messageId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if let Some(sid) = part.get("sessionID").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            state.session_id = sid.to_string();
        }
    }
    let thread_id = state.session_id.clone();
    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();

    let emit_reasoning_summary = |state: &mut SessionTranslationState, text: &str| {
        let summary = text.trim();
        if summary.is_empty() {
            return vec![];
        }
        let item_id = state.reasoning_item(&thread_id);
        vec![
            json!({
                "method": "item/reasoning/summaryPartAdded",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id
                }
            }),
            json!({
                "method": "item/reasoning/summaryTextDelta",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id,
                    "delta": summary
                }
            }),
        ]
    };

    match part_type {
        "text" => {
            if state.is_user_message_id(part_message_id) {
                // Emit a userMessage item so the prompt is visible immediately
                // (particularly important for subagent sessions whose prompts
                // aren't injected by send_user_message_core).
                let effective_text_owned;
                let effective_text = if delta.is_empty() {
                    let full_text = part
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let current_user_text = state
                        .get_turn_state(&thread_id)
                        .map(|ts| ts.user_message_text.clone())
                        .unwrap_or_default();
                    if full_text.starts_with(current_user_text.as_str()) {
                        let suffix = &full_text[current_user_text.len()..];
                        effective_text_owned = suffix.to_string();
                        effective_text_owned.as_str()
                    } else {
                        full_text
                    }
                } else {
                    delta
                };
                if effective_text.is_empty() {
                    return vec![];
                }
                state
                    .get_turn_state_mut(&thread_id)
                    .user_message_text
                    .push_str(effective_text);
                let item_id = state.user_message_item(&thread_id);
                let full_text = state
                    .get_turn_state(&thread_id)
                    .map(|ts| ts.user_message_text.clone())
                    .unwrap_or_default();
                return vec![json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": thread_id,
                        "item": {
                            "id": item_id,
                            "type": "userMessage",
                            "content": [{ "type": "text", "text": full_text }]
                        }
                    }
                })];
            }
            if let Some(pid) = part.get("id").and_then(|v| v.as_str()) {
                let turn_state = state.get_turn_state_mut(&thread_id);
                if turn_state.agent_message_part_id.as_deref() != Some(pid) {
                    turn_state.agent_message_part_id = Some(pid.to_string());
                    turn_state.agent_message_text_len = 0;
                }
            }
            let effective_delta = if delta.is_empty() {
                let full_text = part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let emitted_len = state
                    .get_turn_state(&thread_id)
                    .map(|ts| ts.agent_message_text_len)
                    .unwrap_or(0);
                match unseen_suffix(full_text, emitted_len) {
                    Some(suffix) => suffix,
                    None => return vec![],
                }
            } else {
                delta
            };
            if effective_delta.is_empty() {
                return vec![];
            }
            if delta.is_empty() {
                let full_len = part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(effective_delta.len());
                state.get_turn_state_mut(&thread_id).agent_message_text_len = full_len;
            } else {
                state.get_turn_state_mut(&thread_id).agent_message_text_len +=
                    effective_delta.len();
            }

            let item_id = state.agent_message_item(&thread_id);
            vec![json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id,
                    "delta": effective_delta
                }
            })]
        }

        "reasoning" => {
            if state.is_user_message_id(part_message_id) {
                return vec![];
            }
            if let Some(pid) = part.get("id").and_then(|v| v.as_str()) {
                let turn_state = state.get_turn_state_mut(&thread_id);
                if turn_state.reasoning_part_id.as_deref() != Some(pid) {
                    turn_state.reasoning_part_id = Some(pid.to_string());
                    turn_state.reasoning_text_len = 0;
                }
            }
            let effective_delta = if delta.is_empty() {
                let full_text = part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let emitted_len = state
                    .get_turn_state(&thread_id)
                    .map(|ts| ts.reasoning_text_len)
                    .unwrap_or(0);
                match unseen_suffix(full_text, emitted_len) {
                    Some(suffix) => suffix,
                    None => return vec![],
                }
            } else {
                delta
            };
            if effective_delta.is_empty() {
                return vec![];
            }
            if delta.is_empty() {
                let full_len = part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(effective_delta.len());
                state.get_turn_state_mut(&thread_id).reasoning_text_len = full_len;
            } else {
                state.get_turn_state_mut(&thread_id).reasoning_text_len += effective_delta.len();
            }
            let item_id = state.reasoning_item(&thread_id);
            vec![json!({
                "method": "item/reasoning/textDelta",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id,
                    "delta": effective_delta
                }
            })]
        }

        "step-start" => vec![],
        "step-finish" => vec![],

        "subtask" => {
            let description = part
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let prompt = part
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let label = if description.trim().is_empty() {
                prompt
            } else {
                description
            };
            emit_reasoning_summary(state, label)
        }

        "agent" => {
            let name = part
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            emit_reasoning_summary(state, name)
        }

        "tool" => translate_tool_part(part, state, &thread_id),

        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// message.part.removed — part reset/removal (used by OpenCode reset/revert flows)
// ---------------------------------------------------------------------------

fn translate_part_removed(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    if let Some(sid) = properties.get("sessionID").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            state.session_id = sid.to_string();
        }
    }
    let thread_id = state.session_id.clone();
    let part_id = properties
        .get("partID")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    state.remove_part_mapping(&thread_id, part_id);
    vec![]
}

// ---------------------------------------------------------------------------
// message.part.delta — incremental text/reasoning streaming chunks
// ---------------------------------------------------------------------------

fn translate_part_delta(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let delta = match properties.get("delta").and_then(|v| v.as_str()) {
        Some(d) if !d.is_empty() => d,
        _ => return vec![],
    };

    if let Some(sid) = properties.get("sessionID").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            state.session_id = sid.to_string();
        }
    }

    let field = properties
        .get("field")
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    let message_id = properties
        .get("messageID")
        .or_else(|| properties.get("messageId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let thread_id = state.session_id.clone();
    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();

    match field {
        "text" => {
            if state.is_user_message_id(message_id) {
                state
                    .get_turn_state_mut(&thread_id)
                    .user_message_text
                    .push_str(delta);
                let item_id = state.user_message_item(&thread_id);
                let full_text = state
                    .get_turn_state(&thread_id)
                    .map(|ts| ts.user_message_text.clone())
                    .unwrap_or_default();
                return vec![json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": thread_id,
                        "item": {
                            "id": item_id,
                            "type": "userMessage",
                            "content": [{ "type": "text", "text": full_text }]
                        }
                    }
                })];
            }
            let part_id = properties
                .get("partID")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            let is_reasoning = state
                .get_turn_state(&thread_id)
                .and_then(|ts| ts.reasoning_part_id.as_ref())
                .map(|rpid| part_id == rpid)
                .unwrap_or(false);

            if is_reasoning {
                let item_id = state.reasoning_item(&thread_id);
                state.get_turn_state_mut(&thread_id).reasoning_text_len += delta.len();
                return vec![json!({
                    "method": "item/reasoning/textDelta",
                    "params": {
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "itemId": item_id,
                        "delta": delta
                    }
                })];
            }

            let item_id = state.agent_message_item(&thread_id);
            let turn_state = state.get_turn_state_mut(&thread_id);
            if !part_id.is_empty() && turn_state.agent_message_part_id.as_deref() != Some(part_id) {
                turn_state.agent_message_part_id = Some(part_id.to_string());
            }
            turn_state.agent_message_text_len += delta.len();
            vec![json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id,
                    "delta": delta
                }
            })]
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Tool part translation
// ---------------------------------------------------------------------------

fn translate_tool_part(
    part: &Value,
    state: &mut SessionTranslationState,
    thread_id: &str,
) -> Vec<Value> {
    let tool_state = match part.get("state") {
        Some(s) => s,
        None => return vec![],
    };
    let status = tool_state
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");
    let tool_name = part
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let part_id = part.get("id").and_then(|v| v.as_str()).unwrap_or_default();

    let turn_state = state.get_turn_state_mut(thread_id);
    let item_id = if let Some(existing) = turn_state.tool_call_items.get(part_id) {
        existing.clone()
    } else {
        let id = state.next_item_id();
        if !part_id.is_empty() {
            let turn_state = state.get_turn_state_mut(thread_id);
            turn_state
                .tool_call_items
                .insert(part_id.to_string(), id.clone());
        }
        id
    };

    let item_type = tool_kind_to_item_type(tool_name);
    let raw_input = tool_state.get("input").cloned();

    // For explore items, use the title field from OpenCode (relative path for read,
    // pattern for grep, etc.) or fall back to extracting from input
    let title = tool_state
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut events = Vec::new();

    if tool_name == "task" {
        let collab_status = match status {
            "pending" | "running" => "in_progress",
            "completed" => "completed",
            "error" => "failed",
            _ => "in_progress",
        };
        let item =
            build_task_collab_tool_item(&item_id, collab_status, raw_input.as_ref(), thread_id);
        let method = match status {
            "pending" | "running" => "item/started",
            "completed" | "error" => "item/completed",
            _ => "item/started",
        };
        events.push(json!({
            "method": method,
            "params": {
                "threadId": thread_id,
                "item": item
            }
        }));
        if status == "completed" || status == "error" {
            state.reset_agent_message_item(thread_id);
        }
        return events;
    }

    // Handle todowrite tool specially — emit a todo item
    if tool_name == "todowrite" {
        let todo_status = match status {
            "pending" | "running" => "pending",
            _ => "completed",
        };
        let todos = build_todo_list(raw_input.as_ref(), tool_state);
        let item = json!({
            "id": item_id,
            "type": "todowrite",
            "status": todo_status,
            "todos": todos
        });
        let method = match status {
            "pending" | "running" => "item/started",
            _ => "item/completed",
        };
        events.push(json!({
            "method": method,
            "params": {
                "threadId": thread_id,
                "item": item
            }
        }));
        // Also emit a plan update so the PlanPanel sidebar shows the todo list
        let turn_id = state
            .get_turn_state(thread_id)
            .map(|ts| ts.turn_id.clone())
            .unwrap_or_default();
        events.push(build_plan_from_todos(thread_id, &turn_id, &todos));
        if status == "completed" || status == "error" {
            state.reset_agent_message_item(thread_id);
        }
        return events;
    }

    // Handle explore-type tools (read, grep, glob, list) specially
    if item_type == "explore" {
        let explore_status = match status {
            "pending" | "running" => "exploring",
            _ => "explored",
        };
        let entry = build_explore_entry(tool_name, title, raw_input.as_ref());
        let item = json!({
            "id": item_id,
            "type": "explore",
            "status": explore_status,
            "entries": [entry]
        });
        let method = if status == "completed" || status == "error" {
            "item/completed"
        } else {
            "item/started"
        };
        events.push(json!({
            "method": method,
            "params": {
                "threadId": thread_id,
                "item": item
            }
        }));
        if status == "completed" || status == "error" {
            state.reset_agent_message_item(thread_id);
        }
        return events;
    }

    match status {
        "pending" | "running" => {
            let started_item = build_tool_item(
                &item_id,
                item_type,
                tool_name,
                "in_progress",
                raw_input.as_ref(),
                None,
                None,
            );
            events.push(json!({
                "method": "item/started",
                "params": {
                    "threadId": thread_id,
                    "item": started_item
                }
            }));

            if let Some(ref input) = raw_input {
                let delta_text = tool_input_delta_text(tool_name, input);
                if !delta_text.is_empty() {
                    let method = if item_type == "fileChange" {
                        "item/fileChange/outputDelta"
                    } else {
                        "item/commandExecution/outputDelta"
                    };
                    events.push(json!({
                        "method": method,
                        "params": {
                            "threadId": thread_id,
                            "itemId": item_id,
                            "delta": delta_text
                        }
                    }));
                }
            }
        }
        "completed" => {
            let output_text = tool_state
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let item = build_tool_item(
                &item_id,
                item_type,
                tool_name,
                "completed",
                raw_input.as_ref(),
                Some(output_text),
                None,
            );
            events.push(json!({
                "method": "item/completed",
                "params": {
                    "threadId": thread_id,
                    "item": item
                }
            }));
            state.reset_agent_message_item(thread_id);
        }
        "error" => {
            let error_text = tool_state
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let item = build_tool_item(
                &item_id,
                item_type,
                tool_name,
                "failed",
                raw_input.as_ref(),
                Some(error_text),
                None,
            );
            events.push(json!({
                "method": "item/completed",
                "params": {
                    "threadId": thread_id,
                    "item": item
                }
            }));
            state.reset_agent_message_item(thread_id);
        }
        _ => {}
    }

    events
}

fn tool_input_delta_text(tool_name: &str, raw_input: &Value) -> String {
    if tool_name == "apply_patch" {
        return build_apply_patch_delta_summary(raw_input)
            .unwrap_or_else(|| "Applying patch...".to_string());
    }

    serde_json::to_string_pretty(raw_input).unwrap_or_default()
}

fn build_apply_patch_delta_summary(raw_input: &Value) -> Option<String> {
    let patch_text = raw_input.get("patchText").and_then(|v| v.as_str())?;
    if patch_text.trim().is_empty() {
        return Some("Applying patch...".to_string());
    }

    let changes = generate_apply_patch_changes(raw_input)?;
    if changes.is_empty() {
        return Some("Applying patch...".to_string());
    }

    let mut labels = Vec::new();
    for change in changes.iter().take(3) {
        let kind = change
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("modify");
        let path = change
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("file");
        labels.push(format!("{kind} {path}"));
    }

    let total = changes.len();
    let mut summary = format!(
        "Applying patch to {total} file{}",
        if total == 1 { "" } else { "s" }
    );
    if !labels.is_empty() {
        summary.push_str(": ");
        summary.push_str(&labels.join(", "));
    }
    if total > labels.len() {
        summary.push_str(&format!(", +{} more", total - labels.len()));
    }

    Some(summary)
}

fn build_explore_entry(tool_name: &str, title: &str, raw_input: Option<&Value>) -> Value {
    let kind = tool_to_explore_kind(tool_name);

    // For read: title is the relative path (e.g., "src/foo.ts")
    // For grep: title is the pattern (e.g., "useState")
    // For glob/list: title is the search path (e.g., "src")
    let (label, detail) = match tool_name {
        "read" => {
            // Extract filename from path for label, full path as detail
            let path = if !title.is_empty() {
                title
            } else {
                raw_input
                    .and_then(|i| i.get("filePath"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("file")
            };
            let filename = path.rsplit('/').next().unwrap_or(path);
            if filename == path {
                (path.to_string(), None)
            } else {
                (filename.to_string(), Some(path.to_string()))
            }
        }
        "grep" => {
            // Title is the pattern, optionally add path context from input
            let pattern = if !title.is_empty() {
                title.to_string()
            } else {
                raw_input
                    .and_then(|i| i.get("pattern"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("pattern")
                    .to_string()
            };
            let path = raw_input
                .and_then(|i| i.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let label = if !path.is_empty() {
                format!("{} in {}", pattern, path)
            } else {
                pattern
            };
            (label, None)
        }
        "glob" | "list" | "ls" => {
            // Title is the search path
            let path = if !title.is_empty() {
                title.to_string()
            } else {
                raw_input
                    .and_then(|i| i.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string()
            };
            (path, None)
        }
        _ => (title.to_string(), None),
    };

    let mut entry = json!({
        "kind": kind,
        "label": label
    });
    if let Some(d) = detail {
        entry["detail"] = json!(d);
    }
    entry
}

fn task_collab_prompt(raw_input: Option<&Value>) -> String {
    raw_input
        .and_then(|input| input.get("description"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            raw_input
                .and_then(|input| input.get("prompt"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn task_collab_agent_status_map(item_status: &str, raw_input: Option<&Value>) -> Option<Value> {
    let agent_name = raw_input
        .and_then(|input| input.get("subagent_type"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();

    let status = match item_status {
        "in_progress" => "running",
        "completed" => "completed",
        "failed" => "failed",
        _ => "unknown",
    };

    let mut map = serde_json::Map::new();
    map.insert(agent_name, json!({ "status": status }));
    Some(Value::Object(map))
}

fn build_task_collab_tool_item(
    item_id: &str,
    status: &str,
    raw_input: Option<&Value>,
    thread_id: &str,
) -> Value {
    let mut item = json!({
        "id": item_id,
        "type": "collabToolCall",
        "tool": "task",
        "status": status,
        "senderThreadId": thread_id,
    });
    let prompt = task_collab_prompt(raw_input);
    if !prompt.is_empty() {
        item["prompt"] = json!(prompt);
    }
    if let Some(agent_status) = task_collab_agent_status_map(status, raw_input) {
        item["agentStatus"] = agent_status;
    }
    item
}

// ---------------------------------------------------------------------------
// message.updated — token/usage info
// ---------------------------------------------------------------------------

fn translate_message_updated(
    properties: &Value,
    state: &mut SessionTranslationState,
) -> Vec<Value> {
    let info = match properties.get("info") {
        Some(i) => i,
        None => return vec![],
    };

    if let Some(sid) = info.get("sessionID").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            state.session_id = sid.to_string();
        }
    }

    let thread_id = state.session_id.clone();

    if let Some(message_id) = info.get("id").and_then(|v| v.as_str()) {
        if let Some(role) = info.get("role").and_then(|v| v.as_str()) {
            state.remember_message_role(message_id, role);
        }
    }

    let model = info.get("model");
    let provider_id = info
        .get("providerID")
        .or_else(|| info.get("provider_id"))
        .or_else(|| model.and_then(|m| m.get("providerID")))
        .or_else(|| model.and_then(|m| m.get("provider_id")))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let model_id = info
        .get("modelID")
        .or_else(|| info.get("model_id"))
        .or_else(|| model.and_then(|m| m.get("modelID")))
        .or_else(|| model.and_then(|m| m.get("model_id")))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let model_context_window = state
        .model_context_window(provider_id, model_id)
        .unwrap_or(0);

    // Token usage: try nested `tokens` object first (current format), then flat keys (legacy).
    let tokens = info.get("tokens");
    let input_tokens = parse_u64(
        tokens
            .and_then(|t| t.get("input"))
            .or_else(|| info.get("inputTokens")),
    )
    .unwrap_or(0);
    let output_tokens = parse_u64(
        tokens
            .and_then(|t| t.get("output"))
            .or_else(|| info.get("outputTokens")),
    )
    .unwrap_or(0);
    let cached_tokens = parse_u64(
        tokens
            .and_then(|t| t.get("cache").and_then(|c| c.get("read")))
            .or_else(|| info.get("cachedInputTokens"))
            .or_else(|| info.get("cacheReadInputTokens")),
    )
    .unwrap_or(0);
    let reasoning_tokens = parse_u64(
        tokens
            .and_then(|t| t.get("reasoning"))
            .or_else(|| info.get("reasoningOutputTokens"))
            .or_else(|| info.get("reasoningTokens")),
    )
    .unwrap_or(0);
    let total =
        parse_u64(tokens.and_then(|t| t.get("total"))).unwrap_or(input_tokens + output_tokens);

    if total == 0 {
        return vec![];
    }

    vec![json!({
        "method": "thread/tokenUsage/updated",
        "params": {
            "threadId": thread_id,
            "tokenUsage": {
                "total": {
                    "totalTokens": total,
                    "inputTokens": input_tokens,
                    "cachedInputTokens": cached_tokens,
                    "outputTokens": output_tokens,
                    "reasoningOutputTokens": reasoning_tokens
                },
                "last": {
                    "totalTokens": total,
                    "inputTokens": input_tokens,
                    "cachedInputTokens": cached_tokens,
                    "outputTokens": output_tokens,
                    "reasoningOutputTokens": reasoning_tokens
                },
                "modelContextWindow": model_context_window
            }
        }
    })]
}

// ---------------------------------------------------------------------------
// session.status — idle/active/error transitions
// ---------------------------------------------------------------------------

fn translate_session_status(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let session_id = properties
        .get("sessionID")
        .or_else(|| properties.get("session_id"))
        .or_else(|| properties.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !session_id.is_empty() {
        state.session_id = session_id.to_string();
    }

    let status = properties
        .get("status")
        .and_then(|s| s.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let thread_id = state.session_id.clone();
    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();

    match status {
        "active" | "running" | "busy" => {
            if thread_id.is_empty() || !turn_id.is_empty() {
                return vec![];
            }
            let synthetic_turn_id = state.next_turn_id();
            state.start_turn(thread_id.clone(), synthetic_turn_id.clone());
            vec![build_turn_started(&thread_id, &synthetic_turn_id)]
        }
        "idle" => {
            if thread_id.is_empty() {
                return vec![];
            }
            let mut events = Vec::new();
            if let Some(msg_completed) = build_agent_message_completed(state, &thread_id) {
                events.push(msg_completed);
            }
            // Background helper prompts can complete without ever emitting an
            // "active" status, so still emit turn/completed with an empty turn
            // id to unblock shared background collectors.
            events.push(build_turn_completed(&thread_id, &turn_id));
            state.finish_turn(&thread_id);
            events
        }
        "error" => {
            let error_msg = properties
                .get("status")
                .and_then(|s| s.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            let events = vec![json!({
                "method": "error",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "willRetry": false,
                    "error": {
                        "message": error_msg
                    }
                }
            })];
            state.finish_turn(&thread_id);
            events
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// session.idle — separate event type indicating session has become idle
// ---------------------------------------------------------------------------

fn translate_session_idle(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let session_id = properties
        .get("sessionID")
        .or_else(|| properties.get("session_id"))
        .or_else(|| properties.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if !session_id.is_empty() {
        state.session_id = session_id.to_string();
    }

    let thread_id = state.session_id.clone();
    if thread_id.is_empty() {
        return vec![];
    }

    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();

    let mut events = Vec::new();
    if let Some(msg_completed) = build_agent_message_completed(state, &thread_id) {
        events.push(msg_completed);
    }
    // Emit turn/completed even if turn_id is empty - background prompts don't track turns
    events.push(build_turn_completed(&thread_id, &turn_id));
    state.finish_turn(&thread_id);
    events
}

fn translate_session_error(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let session_id = properties
        .get("sessionID")
        .or_else(|| properties.get("session_id"))
        .or_else(|| properties.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if !session_id.is_empty() {
        state.session_id = session_id.to_string();
    }

    let thread_id = state.session_id.clone();
    if thread_id.is_empty() {
        return vec![];
    }

    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();

    let error = properties.get("error").unwrap_or(&Value::Null);
    let error_msg = error
        .get("data")
        .and_then(|data| data.get("message"))
        .or_else(|| error.get("message"))
        .or_else(|| error.get("error"))
        .or_else(|| error.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown error");

    state.finish_turn(&thread_id);
    vec![json!({
        "method": "error",
        "params": {
            "threadId": thread_id,
            "turnId": turn_id,
            "willRetry": false,
            "error": {
                "message": error_msg
            }
        }
    })]
}

// ---------------------------------------------------------------------------
// permission.updated — permission requests from the agent
// ---------------------------------------------------------------------------

fn translate_sse_permission(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let permission_id = properties
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let session_id = properties
        .get("sessionID")
        .and_then(|v| v.as_str())
        .unwrap_or(&state.session_id);
    // OpenCode sends "permission" field, not "type"
    let perm_type = properties
        .get("permission")
        .and_then(|v| v.as_str())
        .unwrap_or("command");
    // OpenCode sends "patterns" array, not "pattern" string
    let patterns = properties.get("patterns");

    // Encode sessionId:permissionId into the event `id` so the frontend
    // can round-trip it back to `respond_to_server_request`.
    let composite_id = format!("{session_id}:{permission_id}");

    // Defensive handling: patterns can be array, string, or missing
    let command_label = if let Some(pats) = patterns {
        if let Some(arr) = pats.as_array() {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            // Fallback if patterns is unexpectedly a string
            pats.as_str().unwrap_or(perm_type).to_string()
        }
    } else {
        perm_type.to_string()
    };

    vec![json!({
        "id": composite_id,
        "method": "codex/requestApproval",
        "params": {
            "permission": perm_type,
            "command": [command_label]
        }
    })]
}

/// Build the REST body for a permission decision.
///
/// Maps frontend decisions to OpenCode reply format:
/// - `"accept"` → `{ reply: "once" }`
/// - `"always"` → `{ reply: "always" }`
/// - `"decline"` (or other) → `{ reply: "reject" }`
pub(crate) fn build_permission_response(decision: &str) -> Value {
    let reply = match decision {
        "accept" => "once",
        "always" => "always",
        _ => "reject",
    };
    json!({ "reply": reply })
}

// ---------------------------------------------------------------------------
// question.asked — user input request from Claude (mcp_question tool)
// ---------------------------------------------------------------------------

fn translate_question_asked(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let question_id = properties
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let session_id = properties
        .get("sessionID")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| state.session_id.clone());

    if question_id.is_empty() {
        return vec![];
    }

    if !session_id.is_empty() {
        state.session_id = session_id.clone();
    }

    let thread_id = state.session_id.clone();
    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();
    let item_id = state.next_item_id();

    // Composite ID for response routing: "sessionId:questionId"
    let composite_id = format!("{session_id}:{question_id}");

    // Transform questions array to frontend-expected shape
    let questions: Vec<Value> = properties
        .get("questions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(idx, q)| {
                    let header = q.get("header").and_then(|v| v.as_str()).unwrap_or_default();
                    let question_text = q
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    // OpenCode uses "custom" field to indicate if custom input is allowed
                    let is_other = q.get("custom").and_then(|v| v.as_bool()).unwrap_or(true);

                    let options: Vec<Value> = q
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|opts| {
                            opts.iter()
                                .map(|opt| {
                                    json!({
                                        "label": opt.get("label").and_then(|v| v.as_str()).unwrap_or_default(),
                                        "description": opt.get("description").and_then(|v| v.as_str()).unwrap_or_default()
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    json!({
                        "id": idx.to_string(),
                        "header": header,
                        "question": question_text,
                        "isOther": is_other,
                        "options": options
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    vec![json!({
        "id": composite_id,
        "method": "item/tool/requestUserInput",
        "params": {
            "threadId": thread_id,
            "turnId": turn_id,
            "itemId": item_id,
            "questions": questions
        }
    })]
}

// ---------------------------------------------------------------------------
// question.replied / question.rejected — cleanup after user responds/dismisses
// ---------------------------------------------------------------------------

fn translate_question_completed(properties: &Value) -> Vec<Value> {
    let session_id = properties
        .get("sessionID")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let request_id = properties
        .get("requestID")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if request_id.is_empty() {
        return vec![];
    }

    let composite_id = format!("{session_id}:{request_id}");

    vec![json!({
        "method": "item/tool/userInputCompleted",
        "params": {
            "requestId": composite_id,
            "workspaceId": session_id
        }
    })]
}

/// Translate an OpenCode `todo.updated` SSE event into a `turn/plan/updated`
/// event so the PlanPanel sidebar reflects the current session todo list.
fn translate_todo_updated(properties: &Value, state: &mut SessionTranslationState) -> Vec<Value> {
    let session_id = properties
        .get("sessionID")
        .or_else(|| properties.get("sessionId"))
        .and_then(|v| v.as_str())
        .unwrap_or(&state.session_id)
        .to_string();
    let thread_id = if session_id.is_empty() {
        state.session_id.clone()
    } else {
        session_id
    };
    let turn_id = state
        .get_turn_state(&thread_id)
        .map(|ts| ts.turn_id.clone())
        .unwrap_or_default();
    let todos = properties
        .get("todos")
        .cloned()
        .unwrap_or_else(|| json!([]));
    vec![build_plan_from_todos(&thread_id, &turn_id, &todos)]
}

// ---------------------------------------------------------------------------
// Synthetic turn events
// ---------------------------------------------------------------------------

pub(crate) fn build_turn_started(session_id: &str, turn_id: &str) -> Value {
    json!({
        "method": "turn/started",
        "params": {
            "threadId": session_id,
            "turn": {
                "id": turn_id,
                "threadId": session_id
            }
        }
    })
}

pub(crate) fn build_turn_completed(session_id: &str, turn_id: &str) -> Value {
    json!({
        "method": "turn/completed",
        "params": {
            "threadId": session_id,
            "turn": {
                "id": turn_id,
                "threadId": session_id
            }
        }
    })
}

pub(crate) fn build_agent_message_completed(
    state: &SessionTranslationState,
    session_id: &str,
) -> Option<Value> {
    let turn_state = state.get_turn_state(session_id)?;
    let item_id = turn_state.agent_message_item_id.as_ref()?;
    Some(json!({
        "method": "item/completed",
        "params": {
            "threadId": session_id,
            "item": {
                "id": item_id,
                "type": "agentMessage",
                "text": ""
            }
        }
    }))
}

// ---------------------------------------------------------------------------
// Tool call helpers
// ---------------------------------------------------------------------------

fn tool_kind_to_item_type(kind: &str) -> &str {
    match kind {
        "edit" | "write" | "create" | "apply_patch" => "fileChange",
        "bash" | "command" | "terminal" => "commandExecution",
        "read" | "grep" | "glob" | "list" | "ls" => "explore",
        "todowrite" => "todowrite",
        _ => "commandExecution",
    }
}

/// Build a JSON array of todo items from the todowrite tool's input or metadata.
/// Falls back to extracting todos from the input field if metadata is not present.
fn build_todo_list(raw_input: Option<&Value>, tool_state: &Value) -> Value {
    // Prefer metadata.todos (populated on completion) over input.todos (the request payload)
    if let Some(todos) = tool_state
        .get("metadata")
        .and_then(|m| m.get("todos"))
        .and_then(|t| t.as_array())
    {
        let items: Vec<Value> = todos
            .iter()
            .filter_map(|todo| {
                let content = todo.get("content").and_then(|v| v.as_str())?;
                let status = todo
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");
                let priority = todo
                    .get("priority")
                    .and_then(|v| v.as_str())
                    .unwrap_or("medium");
                Some(json!({ "content": content, "status": status, "priority": priority }))
            })
            .collect();
        return json!(items);
    }
    // Fall back to input.todos
    if let Some(input) = raw_input {
        if let Some(todos) = input.get("todos").and_then(|t| t.as_array()) {
            let items: Vec<Value> = todos
                .iter()
                .filter_map(|todo| {
                    let content = todo.get("content").and_then(|v| v.as_str())?;
                    let status = todo
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("pending");
                    let priority = todo
                        .get("priority")
                        .and_then(|v| v.as_str())
                        .unwrap_or("medium");
                    Some(json!({ "content": content, "status": status, "priority": priority }))
                })
                .collect();
            return json!(items);
        }
    }
    json!([])
}

/// Map an OpenCode todo status string to a CodexMonitor plan step status.
fn todo_status_to_plan_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "in_progress" => "inProgress",
        "cancelled" => "completed",
        _ => "pending",
    }
}

/// Build a `turn/plan/updated` event from a list of todo JSON values.
/// Returns `None` when the todo list is empty (the caller should decide
/// whether to emit a clear event in that case).
fn build_plan_from_todos(thread_id: &str, turn_id: &str, todos: &Value) -> Value {
    let steps: Vec<Value> = todos
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|todo| {
            let content = todo.get("content").and_then(|v| v.as_str())?;
            let status = todo
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            Some(json!({
                "step": content,
                "status": todo_status_to_plan_status(status)
            }))
        })
        .collect();
    json!({
        "method": "turn/plan/updated",
        "params": {
            "threadId": thread_id,
            "turnId": turn_id,
            "explanation": null,
            "plan": steps
        }
    })
}

/// Map OpenCode tool names to explore entry kinds.
fn tool_to_explore_kind(tool_name: &str) -> &str {
    match tool_name {
        "read" => "read",
        "grep" => "search",
        "glob" | "list" | "ls" => "list",
        _ => "run",
    }
}

fn command_parts_from_raw_input(raw_input: &Value, fallback_title: &str) -> Vec<String> {
    if let Some(command) = raw_input.get("command") {
        if let Some(parts) = command.as_array() {
            let values: Vec<String> = parts
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            if !values.is_empty() {
                return values;
            }
        }
        if let Some(text) = command.as_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return vec![trimmed.to_string()];
            }
        }
    }
    let fallback = fallback_title.trim();
    if fallback.is_empty() {
        Vec::new()
    } else {
        vec![fallback.to_string()]
    }
}

fn cwd_from_raw_input(raw_input: &Value) -> String {
    ["workdir", "cwd", "path"]
        .iter()
        .find_map(|key| raw_input.get(key).and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}

fn file_path_from_raw_input(raw_input: &Value) -> Option<String> {
    ["filePath", "path"]
        .iter()
        .find_map(|key| raw_input.get(key).and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn build_tool_item(
    item_id: &str,
    item_type: &str,
    title: &str,
    status: &str,
    raw_input: Option<&Value>,
    output: Option<&str>,
    changes_from_content: Option<Vec<Value>>,
) -> Value {
    let mut item = json!({
        "id": item_id,
        "type": item_type,
        "status": status
    });

    if item_type == "commandExecution" {
        let command_parts = raw_input
            .map(|input| command_parts_from_raw_input(input, title))
            .unwrap_or_else(|| {
                let trimmed = title.trim();
                if trimmed.is_empty() {
                    Vec::new()
                } else {
                    vec![trimmed.to_string()]
                }
            });
        if !command_parts.is_empty() {
            item["command"] = json!(command_parts);
        }
        if let Some(input) = raw_input {
            let cwd = cwd_from_raw_input(input);
            if !cwd.is_empty() {
                item["cwd"] = json!(cwd);
            }
        }
        if let Some(output_text) = output {
            if !output_text.trim().is_empty() {
                item["aggregatedOutput"] = json!(output_text);
            }
        }
        return item;
    }

    if item_type == "fileChange" {
        let mut changes = changes_from_content.unwrap_or_default();
        if changes.is_empty() {
            if let Some(input) = raw_input {
                if let Some(parsed_changes) = generate_apply_patch_changes(input) {
                    changes = parsed_changes;
                } else if let Some(path) = file_path_from_raw_input(input) {
                    let mut change = json!({ "path": path, "kind": "modify" });
                    if let Some(diff) = generate_edit_diff(input, &path) {
                        change["diff"] = json!(diff);
                    }
                    changes.push(change);
                }
            }
        }
        if !changes.is_empty() {
            item["changes"] = json!(changes);
        }
        if let Some(output_text) = output {
            if !output_text.trim().is_empty() {
                item["output"] = json!(output_text);
            }
        }
        return item;
    }

    item
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn make_state() -> SessionTranslationState {
        let mut s = SessionTranslationState::new("ses_test123".into());
        s.start_turn("ses_test123".into(), "turn_1".into());
        s
    }

    #[test]
    fn text_part_produces_agent_message_delta() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "part_1",
                    "sessionID": "ses_test123",
                    "text": "Hello world"
                },
                "delta": "Hello world"
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/agentMessage/delta");
        assert_eq!(events[0]["params"]["threadId"], "ses_test123");
        assert_eq!(events[0]["params"]["delta"], "Hello world");
        // Same item ID on second call.
        let item_id = events[0]["params"]["itemId"].as_str().unwrap().to_string();
        let events2 = translate_sse_event(&event, &mut state);
        assert_eq!(events2[0]["params"]["itemId"].as_str().unwrap(), item_id);
    }

    #[test]
    fn reasoning_part_produces_reasoning_delta() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "reasoning",
                    "id": "part_r1",
                    "sessionID": "ses_test123"
                },
                "delta": "Let me think..."
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/reasoning/textDelta");
        assert_eq!(events[0]["params"]["delta"], "Let me think...");
    }

    #[test]
    fn tool_part_pending_produces_item_started() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_1",
                    "sessionID": "ses_test123",
                    "tool": "bash",
                    "state": {
                        "status": "running",
                        "input": { "command": ["ls", "-la"] }
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert!(events.len() >= 1);
        assert_eq!(events[0]["method"], "item/started");
        assert_eq!(events[0]["params"]["item"]["type"], "commandExecution");
    }

    #[test]
    fn tool_part_completed_produces_item_completed() {
        let mut state = make_state();
        // First register the tool.
        let running = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_2",
                    "tool": "edit",
                    "state": { "status": "running", "input": { "filePath": "foo.rs" } }
                }
            }
        });
        translate_sse_event(&running, &mut state);

        let completed = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_2",
                    "tool": "edit",
                    "state": {
                        "status": "completed",
                        "input": { "filePath": "foo.rs" },
                        "output": "File written."
                    }
                }
            }
        });
        let events = translate_sse_event(&completed, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        assert_eq!(events[0]["params"]["item"]["type"], "fileChange");
        assert_eq!(events[0]["params"]["item"]["status"], "completed");
    }

    #[test]
    fn edit_tool_generates_unified_diff() {
        let mut state = make_state();
        let completed = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_edit_diff",
                    "tool": "edit",
                    "state": {
                        "status": "completed",
                        "input": {
                            "filePath": "src/main.rs",
                            "oldString": "fn main() {\n    println!(\"Hello\");\n}",
                            "newString": "fn main() {\n    println!(\"Hello, world!\");\n}"
                        },
                        "output": "File edited."
                    }
                }
            }
        });
        let events = translate_sse_event(&completed, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "fileChange");

        let changes = item["changes"].as_array().expect("changes should be array");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["path"], "src/main.rs");

        let diff = changes[0]["diff"].as_str().expect("diff should be string");
        assert!(diff.contains("--- a/src/main.rs"));
        assert!(diff.contains("+++ b/src/main.rs"));
        assert!(diff.contains("@@"));
        assert!(diff.contains("-    println!(\"Hello\");"));
        assert!(diff.contains("+    println!(\"Hello, world!\");"));
    }

    #[test]
    fn apply_patch_tool_generates_file_change_entries() {
        let mut state = make_state();
        let completed = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_apply_patch",
                    "tool": "apply_patch",
                    "state": {
                        "status": "completed",
                        "input": {
                            "patchText": "*** Begin Patch\n*** Update File: src/main.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n*** Add File: notes.txt\n+hello\n*** Delete File: old.txt\n*** Update File: src/from.rs\n*** Move to: src/to.rs\n@@ -1,1 +1,1 @@\n-before\n+after\n*** End Patch"
                        },
                        "output": "Patch applied successfully."
                    }
                }
            }
        });

        let events = translate_sse_event(&completed, &mut state);
        assert_eq!(events.len(), 1);
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "fileChange");

        let changes = item["changes"].as_array().expect("changes should be array");
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0]["path"], "src/main.rs");
        assert_eq!(changes[0]["kind"], "modify");
        assert!(changes[0]["diff"]
            .as_str()
            .expect("modify diff")
            .contains("--- a/src/main.rs"));

        assert_eq!(changes[1]["path"], "notes.txt");
        assert_eq!(changes[1]["kind"], "add");
        assert!(changes[1]["diff"]
            .as_str()
            .expect("add diff")
            .contains("--- /dev/null"));

        assert_eq!(changes[2]["path"], "old.txt");
        assert_eq!(changes[2]["kind"], "delete");
        assert!(changes[2].get("diff").is_none());

        assert_eq!(changes[3]["path"], "src/to.rs");
        let rename_diff = changes[3]["diff"].as_str().expect("rename diff");
        assert!(rename_diff.contains("--- a/src/from.rs"));
        assert!(rename_diff.contains("+++ b/src/to.rs"));
    }

    #[test]
    fn apply_patch_running_delta_uses_summary_not_patch_text() {
        let mut state = make_state();
        let running = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_apply_patch_running",
                    "tool": "apply_patch",
                    "state": {
                        "status": "running",
                        "input": {
                            "patchText": "*** Begin Patch\n*** Update File: src/main.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n*** Add File: notes.txt\n+hello\n*** End Patch"
                        }
                    }
                }
            }
        });

        let events = translate_sse_event(&running, &mut state);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["method"], "item/started");
        assert_eq!(events[1]["method"], "item/fileChange/outputDelta");
        let delta = events[1]["params"]["delta"].as_str().expect("delta string");
        assert!(delta.contains("Applying patch to 2 files"));
        assert!(delta.contains("modify src/main.rs"));
        assert!(delta.contains("add notes.txt"));
        assert!(!delta.contains("*** Begin Patch"));
        assert!(!delta.contains("patchText"));
    }

    #[test]
    fn text_after_tool_completion_uses_new_agent_message_item() {
        let mut state = make_state();

        let first_text = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "part_before_tool",
                    "sessionID": "ses_test123"
                },
                "delta": "Before tool call."
            }
        });
        let before_events = translate_sse_event(&first_text, &mut state);
        let before_item_id = before_events[0]["params"]["itemId"]
            .as_str()
            .unwrap()
            .to_string();

        let tool_running = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_split_1",
                    "sessionID": "ses_test123",
                    "tool": "bash",
                    "state": {
                        "status": "running",
                        "input": { "command": ["pwd"] }
                    }
                }
            }
        });
        translate_sse_event(&tool_running, &mut state);

        let tool_completed = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_split_1",
                    "sessionID": "ses_test123",
                    "tool": "bash",
                    "state": {
                        "status": "completed",
                        "input": { "command": ["pwd"] },
                        "output": "/Users/jacob"
                    }
                }
            }
        });
        translate_sse_event(&tool_completed, &mut state);

        let second_text = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "part_after_tool",
                    "sessionID": "ses_test123"
                },
                "delta": "After tool call."
            }
        });
        let after_events = translate_sse_event(&second_text, &mut state);
        let after_item_id = after_events[0]["params"]["itemId"]
            .as_str()
            .unwrap()
            .to_string();

        assert_ne!(before_item_id, after_item_id);
    }

    #[test]
    fn message_updated_produces_token_usage() {
        let mut state = make_state();
        let event = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "inputTokens": 5000,
                    "outputTokens": 1000,
                    "cachedInputTokens": 200,
                    "reasoningOutputTokens": 50
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "thread/tokenUsage/updated");
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["totalTokens"],
            6000
        );
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["inputTokens"],
            5000
        );
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["outputTokens"],
            1000
        );
    }

    #[test]
    fn message_updated_uses_model_context_window_when_available() {
        let mut state = make_state();
        let mut windows = HashMap::new();
        windows.insert("anthropic/claude-sonnet-4-5".to_string(), 200_000);
        state.replace_model_context_windows(windows);

        let event = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "providerID": "anthropic",
                    "modelID": "claude-sonnet-4-5",
                    "tokens": {
                        "input": 1200,
                        "output": 300,
                        "reasoning": 50,
                        "cache": { "read": 25, "write": 0 }
                    }
                }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "thread/tokenUsage/updated");
        assert_eq!(
            events[0]["params"]["tokenUsage"]["modelContextWindow"],
            200_000
        );
    }

    #[test]
    fn extract_model_context_windows_reads_provider_limits() {
        let payload = json!({
            "providers": [
                {
                    "id": "anthropic",
                    "models": {
                        "claude-sonnet-4-5": {
                            "id": "claude-sonnet-4-5",
                            "limit": {
                                "context": 200000,
                                "output": 8192
                            }
                        },
                        "claude-haiku-4-5": {
                            "id": "claude-haiku-4-5",
                            "limit": {
                                "context": "100000"
                            }
                        }
                    }
                }
            ]
        });

        let windows = extract_model_context_windows(&payload);
        assert_eq!(windows.get("anthropic/claude-sonnet-4-5"), Some(&200_000));
        assert_eq!(windows.get("anthropic/claude-haiku-4-5"), Some(&100_000));
    }

    #[test]
    fn session_status_idle_produces_turn_completed() {
        let mut state = make_state();
        let event = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_test123",
                "status": { "type": "idle" }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert!(events.iter().any(|e| e["method"] == "turn/completed"));
    }

    #[test]
    fn session_status_idle_without_active_turn_still_completes() {
        let mut state = SessionTranslationState::new(String::new());
        let event = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_background_1",
                "status": { "type": "idle" }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "turn/completed");
        assert_eq!(events[0]["params"]["threadId"], "ses_background_1");
        assert_eq!(events[0]["params"]["turn"]["id"], "");
    }

    #[test]
    fn session_error_produces_error_event_without_active_turn() {
        let mut state = SessionTranslationState::new(String::new());
        let event = json!({
            "type": "session.error",
            "properties": {
                "sessionID": "ses_background_2",
                "error": {
                    "name": "BadRequestError",
                    "data": {
                        "message": "model not available"
                    }
                }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "error");
        assert_eq!(events[0]["params"]["threadId"], "ses_background_2");
        assert_eq!(events[0]["params"]["turnId"], "");
        assert_eq!(events[0]["params"]["error"]["message"], "model not available");
    }

    #[test]
    fn session_status_active_produces_turn_started_when_missing() {
        let mut state = SessionTranslationState::new(String::new());
        let event = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_subagent_1",
                "status": { "type": "active" }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "turn/started");
        assert_eq!(events[0]["params"]["threadId"], "ses_subagent_1");
        assert!(events[0]["params"]["turn"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("turn_rest_"));
    }

    #[test]
    fn session_status_active_is_ignored_when_turn_already_active() {
        let mut state = SessionTranslationState::new(String::new());
        state.start_turn("ses_subagent_1".into(), "turn_existing".into());
        let event = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_subagent_1",
                "status": { "type": "active" }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn session_created_emits_thread_started_and_name_with_parent() {
        let mut state = SessionTranslationState::new(String::new());
        let event = json!({
            "type": "session.created",
            "properties": {
                "session": {
                    "id": "ses_child",
                    "title": "Explore session handling (subagent)",
                    "parentID": "ses_parent",
                    "updatedAt": 1700000000
                }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["method"], "thread/started");
        assert_eq!(events[0]["params"]["thread"]["id"], "ses_child");
        assert_eq!(events[0]["params"]["thread"]["parentId"], "ses_parent");
        assert_eq!(
            events[0]["params"]["thread"]["preview"],
            "Explore session handling (subagent)"
        );
        assert_eq!(events[1]["method"], "thread/name/updated");
        assert_eq!(events[1]["params"]["threadId"], "ses_child");
        assert_eq!(
            events[1]["params"]["threadName"],
            "Explore session handling (subagent)"
        );
    }

    #[test]
    fn concurrent_sessions_get_correct_turn_ids() {
        let mut state = SessionTranslationState::new(String::new());

        // Session A starts a turn
        state.start_turn("ses_A".into(), "turn_A1".into());
        // Session B starts a turn (should NOT clobber A's turn)
        state.start_turn("ses_B".into(), "turn_B1".into());

        // Idle event for session A should use turn_A1, not turn_B1
        let event_a = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_A",
                "status": { "type": "idle" }
            }
        });
        let events = translate_sse_event(&event_a, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "turn/completed");
        assert_eq!(events[0]["params"]["threadId"], "ses_A");
        assert_eq!(events[0]["params"]["turn"]["id"], "turn_A1");

        // Idle event for session B should use turn_B1
        let event_b = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_B",
                "status": { "type": "idle" }
            }
        });
        let events = translate_sse_event(&event_b, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["params"]["turn"]["id"], "turn_B1");
    }

    #[test]
    fn session_status_error_produces_error_event() {
        let mut state = make_state();
        let event = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "ses_test123",
                "status": { "type": "error", "message": "rate limited" }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "error");
        assert_eq!(events[0]["params"]["error"]["message"], "rate limited");
    }

    #[test]
    fn permission_asked_produces_approval_request() {
        let mut state = make_state();
        let event = json!({
            "type": "permission.asked",
            "properties": {
                "id": "perm_42",
                "permission": "bash",
                "sessionID": "ses_test123",
                "patterns": ["rm -rf /tmp/test"]
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "codex/requestApproval");
        assert_eq!(events[0]["id"], "ses_test123:perm_42");
        assert_eq!(events[0]["params"]["permission"], "bash");
        assert_eq!(events[0]["params"]["command"][0], "rm -rf /tmp/test");
    }

    #[test]
    fn turn_lifecycle_events() {
        let started = build_turn_started("ses_abc", "turn_1");
        assert_eq!(started["method"], "turn/started");
        assert_eq!(started["params"]["threadId"], "ses_abc");
        assert_eq!(started["params"]["turn"]["id"], "turn_1");

        let completed = build_turn_completed("ses_abc", "turn_1");
        assert_eq!(completed["method"], "turn/completed");
        assert_eq!(completed["params"]["turn"]["id"], "turn_1");
    }

    #[test]
    fn permission_response_shapes() {
        let accept = build_permission_response("accept");
        assert_eq!(accept["reply"], "once");

        let always = build_permission_response("always");
        assert_eq!(always["reply"], "always");

        let deny = build_permission_response("decline");
        assert_eq!(deny["reply"], "reject");
    }

    #[test]
    fn unknown_event_type_returns_empty() {
        let mut state = make_state();
        let event = json!({
            "type": "some.unknown.event",
            "properties": {}
        });
        let events = translate_sse_event(&event, &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn chunk_text_preserves_whitespace() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "part_ws",
                    "sessionID": "ses_test123"
                },
                "delta": " line with trailing space "
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events[0]["params"]["delta"], " line with trailing space ");
    }

    #[test]
    fn part_delta_text_produces_agent_message_delta() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_1",
                "partID": "prt_text_1",
                "field": "text",
                "delta": "hello"
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/agentMessage/delta");
        assert_eq!(events[0]["params"]["threadId"], "ses_test123");
        assert_eq!(events[0]["params"]["delta"], "hello");
    }

    #[test]
    fn text_part_updated_uses_only_unseen_suffix_after_streamed_deltas() {
        let mut state = make_state();

        let delta1 = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_1",
                "partID": "prt_text_1",
                "field": "text",
                "delta": "I'm currently in Plan Mode (read-only), "
            }
        });
        let events1 = translate_sse_event(&delta1, &mut state);
        assert_eq!(events1.len(), 1);
        assert_eq!(
            events1[0]["params"]["delta"],
            "I'm currently in Plan Mode (read-only), "
        );

        let delta2 = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_1",
                "partID": "prt_text_1",
                "field": "text",
                "delta": "so I can't make any file edits right now."
            }
        });
        let events2 = translate_sse_event(&delta2, &mut state);
        assert_eq!(events2.len(), 1);
        assert_eq!(
            events2[0]["params"]["delta"],
            "so I can't make any file edits right now."
        );

        let updated = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "prt_text_1",
                    "sessionID": "ses_test123",
                    "messageID": "msg_1",
                    "text": "I'm currently in Plan Mode (read-only), so I can't make any file edits right now.\n\nTo make edits, switch out of Plan Mode first."
                }
            }
        });
        let events3 = translate_sse_event(&updated, &mut state);
        assert_eq!(events3.len(), 1);
        assert_eq!(events3[0]["method"], "item/agentMessage/delta");
        assert_eq!(
            events3[0]["params"]["delta"],
            "\n\nTo make edits, switch out of Plan Mode first."
        );
    }

    #[test]
    fn user_text_part_updated_emits_user_message_item() {
        let mut state = make_state();

        let message_updated = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_user_1",
                    "sessionID": "ses_test123",
                    "role": "user"
                }
            }
        });
        let _ = translate_sse_event(&message_updated, &mut state);

        let part_updated = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "prt_user_text_1",
                    "sessionID": "ses_test123",
                    "messageID": "msg_user_1",
                    "text": "User prompt text"
                }
            }
        });
        let events = translate_sse_event(&part_updated, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        assert_eq!(events[0]["params"]["item"]["type"], "userMessage");
        assert_eq!(
            events[0]["params"]["item"]["content"][0]["text"],
            "User prompt text"
        );
    }

    #[test]
    fn user_text_part_delta_emits_user_message_item() {
        let mut state = make_state();

        let message_updated = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_user_1",
                    "sessionID": "ses_test123",
                    "role": "user"
                }
            }
        });
        let _ = translate_sse_event(&message_updated, &mut state);

        let part_delta = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_user_1",
                "partID": "prt_user_text_1",
                "field": "text",
                "delta": "User prompt text"
            }
        });
        let events = translate_sse_event(&part_delta, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        assert_eq!(events[0]["params"]["item"]["type"], "userMessage");
        assert_eq!(
            events[0]["params"]["item"]["content"][0]["text"],
            "User prompt text"
        );
    }

    #[test]
    fn user_text_accumulates_across_deltas() {
        let mut state = make_state();

        let message_updated = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_user_1",
                    "sessionID": "ses_test123",
                    "role": "user"
                }
            }
        });
        let _ = translate_sse_event(&message_updated, &mut state);

        let delta1 = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_user_1",
                "partID": "prt_user_text_1",
                "field": "text",
                "delta": "Hello "
            }
        });
        let events1 = translate_sse_event(&delta1, &mut state);
        assert_eq!(events1[0]["params"]["item"]["content"][0]["text"], "Hello ");

        let delta2 = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_user_1",
                "partID": "prt_user_text_1",
                "field": "text",
                "delta": "world"
            }
        });
        let events2 = translate_sse_event(&delta2, &mut state);
        assert_eq!(
            events2[0]["params"]["item"]["content"][0]["text"],
            "Hello world"
        );

        // Both emissions use the same stable item ID
        assert_eq!(
            events1[0]["params"]["item"]["id"],
            events2[0]["params"]["item"]["id"]
        );
    }

    #[test]
    fn session_active_preserves_in_progress_user_message_stream_state() {
        let mut state = make_state();

        let _ = translate_sse_event(
            &json!({
                "type": "message.updated",
                "properties": {
                    "info": {
                        "id": "msg_user_1",
                        "sessionID": "ses_test123",
                        "role": "user"
                    }
                }
            }),
            &mut state,
        );

        let first = translate_sse_event(
            &json!({
                "type": "message.part.updated",
                "properties": {
                    "part": {
                        "type": "text",
                        "id": "prt_user_text_1",
                        "sessionID": "ses_test123",
                        "messageID": "msg_user_1",
                        "text": "Give me a quick summary"
                    }
                }
            }),
            &mut state,
        );
        let first_item_id = first[0]["params"]["item"]["id"].clone();

        let active_events = translate_sse_event(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": "ses_test123",
                    "status": { "type": "active" }
                }
            }),
            &mut state,
        );
        // Turn is already active from make_state(), so session.status=active is a no-op
        assert_eq!(active_events.len(), 0);

        let second = translate_sse_event(
            &json!({
                "type": "message.part.delta",
                "properties": {
                    "sessionID": "ses_test123",
                    "messageID": "msg_user_1",
                    "partID": "prt_user_text_1",
                    "field": "text",
                    "delta": " of the docs folder"
                }
            }),
            &mut state,
        );

        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["method"], "item/completed");
        assert_eq!(second[0]["params"]["item"]["id"], first_item_id);
        assert_eq!(
            second[0]["params"]["item"]["content"][0]["text"],
            "Give me a quick summary of the docs folder"
        );
    }

    #[test]
    fn user_text_part_updated_does_not_produce_agent_message_delta() {
        let mut state = make_state();

        let message_updated = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_user_1",
                    "sessionID": "ses_test123",
                    "role": "user"
                }
            }
        });
        let _ = translate_sse_event(&message_updated, &mut state);

        let part_updated = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "prt_user_text_1",
                    "sessionID": "ses_test123",
                    "messageID": "msg_user_1",
                    "text": "User prompt text"
                }
            }
        });
        let events = translate_sse_event(&part_updated, &mut state);
        // Should produce a userMessage item, NOT an agentMessage delta
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        assert_ne!(events[0]["method"], "item/agentMessage/delta");
    }

    #[test]
    fn concurrent_sessions_isolate_user_message_text() {
        let mut state = SessionTranslationState::new(String::new());

        // Parent session: register user message role
        let _ = translate_sse_event(
            &json!({
                "type": "message.updated",
                "properties": {
                    "info": { "id": "msg_parent_user", "sessionID": "ses_parent", "role": "user" }
                }
            }),
            &mut state,
        );

        // Parent session: user text arrives
        let parent_events = translate_sse_event(
            &json!({
                "type": "message.part.updated",
                "properties": {
                    "part": {
                        "type": "text",
                        "id": "prt_parent_text",
                        "sessionID": "ses_parent",
                        "messageID": "msg_parent_user",
                        "text": "spin up an explore subagent"
                    }
                }
            }),
            &mut state,
        );
        assert_eq!(parent_events.len(), 1);
        assert_eq!(
            parent_events[0]["params"]["item"]["content"][0]["text"],
            "spin up an explore subagent"
        );

        // Subagent session created
        let _ = translate_sse_event(
            &json!({
                "type": "session.created",
                "properties": {
                    "session": {
                        "id": "ses_child",
                        "title": "Explore subagent",
                        "parentID": "ses_parent"
                    }
                }
            }),
            &mut state,
        );

        // Subagent session: register user message role
        let _ = translate_sse_event(
            &json!({
                "type": "message.updated",
                "properties": {
                    "info": { "id": "msg_child_user", "sessionID": "ses_child", "role": "user" }
                }
            }),
            &mut state,
        );

        // Subagent session: user text arrives — must NOT include parent's text
        let child_events = translate_sse_event(
            &json!({
                "type": "message.part.updated",
                "properties": {
                    "part": {
                        "type": "text",
                        "id": "prt_child_text",
                        "sessionID": "ses_child",
                        "messageID": "msg_child_user",
                        "text": "Explore the codebase"
                    }
                }
            }),
            &mut state,
        );
        assert_eq!(child_events.len(), 1);
        assert_eq!(
            child_events[0]["params"]["item"]["content"][0]["text"],
            "Explore the codebase"
        );
        assert_eq!(child_events[0]["params"]["threadId"], "ses_child");

        // Verify parent and child use different item IDs
        assert_ne!(
            parent_events[0]["params"]["item"]["id"],
            child_events[0]["params"]["item"]["id"]
        );
    }

    #[test]
    fn prepare_replay_preserves_user_message_item_id() {
        let mut state = SessionTranslationState::new(String::new());

        // Simulate live SSE: register user role and emit user text
        let _ = translate_sse_event(
            &json!({
                "type": "message.updated",
                "properties": {
                    "info": { "id": "msg_u1", "sessionID": "ses_sub", "role": "user" }
                }
            }),
            &mut state,
        );
        let live_events = translate_sse_event(
            &json!({
                "type": "message.part.updated",
                "properties": {
                    "part": {
                        "type": "text",
                        "id": "prt_u1",
                        "sessionID": "ses_sub",
                        "messageID": "msg_u1",
                        "text": "subagent prompt"
                    }
                }
            }),
            &mut state,
        );
        let live_id = live_events[0]["params"]["item"]["id"].as_str().unwrap();

        // prepare_replay should preserve the user_message_item_id
        state.prepare_replay("ses_sub".into());
        let replay_id = state.user_message_item("ses_sub");
        assert_eq!(replay_id, live_id, "replay must reuse the live SSE item ID");
    }

    #[test]
    fn replay_user_message_boundary_allocates_new_user_item_id() {
        let mut state = SessionTranslationState::new(String::new());
        state.prepare_replay("ses_sub".into());

        let first = state.user_message_item("ses_sub");
        state.mark_new_replayed_user_message_boundary();
        let second = state.user_message_item("ses_sub");

        assert_ne!(
            first, second,
            "each replayed user message should get its own item ID"
        );
    }

    #[test]
    fn user_message_id_always_lower_than_tool_ids_in_turn() {
        let mut state = SessionTranslationState::new("ses_test".into());

        // Simulate turn starting with session.status=active
        state.start_turn("ses_test".into(), "turn_1".into());

        // Get user message ID (should be pre-allocated by start_turn)
        let user_id = state.user_message_item("ses_test");

        // Simulate tool calls getting IDs
        let tool_id_1 = state.next_item_id();
        let tool_id_2 = state.next_item_id();

        // Parse sequence numbers
        let user_seq: u64 = user_id.strip_prefix("item_").unwrap().parse().unwrap();
        let tool_seq_1: u64 = tool_id_1.strip_prefix("item_").unwrap().parse().unwrap();
        let tool_seq_2: u64 = tool_id_2.strip_prefix("item_").unwrap().parse().unwrap();

        assert!(
            user_seq < tool_seq_1,
            "User message ID ({user_id}) must be lower than first tool ID ({tool_id_1})"
        );
        assert!(
            user_seq < tool_seq_2,
            "User message ID ({user_id}) must be lower than second tool ID ({tool_id_2})"
        );
    }

    #[test]
    fn start_turn_preserves_existing_user_message_id() {
        let mut state = SessionTranslationState::new("ses_test".into());

        // Simulate user message arriving BEFORE session.status=active
        let early_user_id = state.user_message_item("ses_test");

        // Now turn starts - should preserve the existing user message ID
        state.start_turn("ses_test".into(), "turn_1".into());

        // Get user message ID again - should be the same
        let after_start_id = state.user_message_item("ses_test");

        assert_eq!(
            early_user_id, after_start_id,
            "start_turn must preserve existing user message ID"
        );

        // Tool IDs should still be higher
        let tool_id = state.next_item_id();
        let user_seq: u64 = early_user_id
            .strip_prefix("item_")
            .unwrap()
            .parse()
            .unwrap();
        let tool_seq: u64 = tool_id.strip_prefix("item_").unwrap().parse().unwrap();

        assert!(
            user_seq < tool_seq,
            "User message ID ({early_user_id}) must be lower than tool ID ({tool_id})"
        );
    }

    #[test]
    fn part_delta_routes_reasoning_by_part_id() {
        let mut state = make_state();

        // First, announce a reasoning part via message.part.updated.
        let reasoning_announce = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "reasoning",
                    "id": "prt_reasoning_1",
                    "sessionID": "ses_test123",
                    "text": ""
                }
            }
        });
        translate_sse_event(&reasoning_announce, &mut state);

        // Now a delta for that reasoning part should produce reasoning event.
        let delta_event = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "partID": "prt_reasoning_1",
                "field": "text",
                "delta": "thinking..."
            }
        });
        let events = translate_sse_event(&delta_event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/reasoning/textDelta");
        assert_eq!(events[0]["params"]["delta"], "thinking...");
    }

    #[test]
    fn part_removed_resets_reasoning_mapping_for_next_part() {
        let mut state = make_state();

        let reasoning_announce_1 = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "reasoning",
                    "id": "prt_reasoning_1",
                    "sessionID": "ses_test123",
                    "text": ""
                }
            }
        });
        translate_sse_event(&reasoning_announce_1, &mut state);

        let first_delta = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "partID": "prt_reasoning_1",
                "field": "text",
                "delta": "thinking..."
            }
        });
        let first_events = translate_sse_event(&first_delta, &mut state);
        let first_item_id = first_events[0]["params"]["itemId"]
            .as_str()
            .unwrap()
            .to_string();

        let removed = json!({
            "type": "message.part.removed",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_1",
                "partID": "prt_reasoning_1"
            }
        });
        let removed_events = translate_sse_event(&removed, &mut state);
        assert!(removed_events.is_empty());

        let reasoning_announce_2 = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "reasoning",
                    "id": "prt_reasoning_2",
                    "sessionID": "ses_test123",
                    "text": ""
                }
            }
        });
        translate_sse_event(&reasoning_announce_2, &mut state);

        let second_delta = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "partID": "prt_reasoning_2",
                "field": "text",
                "delta": "rethinking..."
            }
        });
        let second_events = translate_sse_event(&second_delta, &mut state);
        let second_item_id = second_events[0]["params"]["itemId"].as_str().unwrap();
        assert_eq!(second_events[0]["method"], "item/reasoning/textDelta");
        assert_ne!(second_item_id, first_item_id);
    }

    #[test]
    fn part_removed_resets_text_mapping_for_next_part() {
        let mut state = make_state();

        let text_part_1 = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "prt_text_1",
                    "sessionID": "ses_test123"
                },
                "delta": "hello"
            }
        });
        let first_events = translate_sse_event(&text_part_1, &mut state);
        let first_item_id = first_events[0]["params"]["itemId"]
            .as_str()
            .unwrap()
            .to_string();

        let removed = json!({
            "type": "message.part.removed",
            "properties": {
                "sessionID": "ses_test123",
                "messageID": "msg_1",
                "partID": "prt_text_1"
            }
        });
        let removed_events = translate_sse_event(&removed, &mut state);
        assert!(removed_events.is_empty());

        let text_part_2 = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "text",
                    "id": "prt_text_2",
                    "sessionID": "ses_test123"
                },
                "delta": "hi again"
            }
        });
        let second_events = translate_sse_event(&text_part_2, &mut state);
        let second_item_id = second_events[0]["params"]["itemId"].as_str().unwrap();
        assert_eq!(second_events[0]["method"], "item/agentMessage/delta");
        assert_ne!(second_item_id, first_item_id);
    }

    #[test]
    fn subtask_part_produces_reasoning_summary_events() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "subtask",
                    "id": "prt_subtask_1",
                    "sessionID": "ses_test123",
                    "messageID": "msg_1",
                    "prompt": "Investigate SQLite fallback and verify build",
                    "description": "Designing SQLite usage fallback",
                    "agent": "code",
                }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["method"], "item/reasoning/summaryPartAdded");
        assert_eq!(events[1]["method"], "item/reasoning/summaryTextDelta");
        assert_eq!(
            events[1]["params"]["delta"],
            "Designing SQLite usage fallback"
        );
    }

    #[test]
    fn agent_part_produces_reasoning_summary_events() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "agent",
                    "id": "prt_agent_1",
                    "sessionID": "ses_test123",
                    "messageID": "msg_1",
                    "name": "Verifying type inference and build errors"
                }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["method"], "item/reasoning/summaryPartAdded");
        assert_eq!(events[1]["method"], "item/reasoning/summaryTextDelta");
        assert_eq!(
            events[1]["params"]["delta"],
            "Verifying type inference and build errors"
        );
    }

    #[test]
    fn reasoning_part_updated_uses_part_text_when_delta_missing() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "reasoning",
                    "id": "prt_reasoning_full",
                    "sessionID": "ses_test123",
                    "text": "Thinking through edge cases"
                }
            }
        });

        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/reasoning/textDelta");
        assert_eq!(events[0]["params"]["delta"], "Thinking through edge cases");
    }

    #[test]
    fn nested_token_format_produces_token_usage() {
        let mut state = make_state();
        let event = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "sessionID": "ses_test123",
                    "tokens": {
                        "total": 6000,
                        "input": 5000,
                        "output": 1000,
                        "reasoning": 50,
                        "cache": { "read": 200, "write": 0 }
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "thread/tokenUsage/updated");
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["inputTokens"],
            5000
        );
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["outputTokens"],
            1000
        );
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["cachedInputTokens"],
            200
        );
        assert_eq!(
            events[0]["params"]["tokenUsage"]["total"]["reasoningOutputTokens"],
            50
        );
    }

    #[test]
    fn part_delta_empty_delta_returns_empty() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test123",
                "partID": "prt_1",
                "field": "text",
                "delta": ""
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn read_tool_produces_explore_item() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_read_1",
                    "sessionID": "ses_test123",
                    "tool": "read",
                    "state": {
                        "status": "completed",
                        "title": "src/utils/foo.ts",
                        "input": { "filePath": "src/utils/foo.ts" },
                        "output": "file contents..."
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "explore");
        assert_eq!(item["status"], "explored");
        let entries = item["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["kind"], "read");
        assert_eq!(entries[0]["label"], "foo.ts");
        assert_eq!(entries[0]["detail"], "src/utils/foo.ts");
    }

    #[test]
    fn grep_tool_produces_explore_item_with_search_kind() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_grep_1",
                    "sessionID": "ses_test123",
                    "tool": "grep",
                    "state": {
                        "status": "completed",
                        "title": "useState",
                        "input": { "pattern": "useState", "path": "src" },
                        "output": "matches..."
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "explore");
        let entries = item["entries"].as_array().unwrap();
        assert_eq!(entries[0]["kind"], "search");
        assert_eq!(entries[0]["label"], "useState in src");
    }

    #[test]
    fn task_tool_running_produces_collab_tool_call_started() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_task_1",
                    "sessionID": "ses_test123",
                    "tool": "task",
                    "state": {
                        "status": "running",
                        "input": {
                            "description": "Explore the codebase",
                            "prompt": "fallback prompt",
                            "subagent_type": "explore"
                        }
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/started");
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "collabToolCall");
        assert_eq!(item["tool"], "task");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["senderThreadId"], "ses_test123");
        assert_eq!(item["prompt"], "Explore the codebase");
        assert_eq!(item["agentStatus"]["explore"]["status"], "running");
    }

    #[test]
    fn task_tool_lifecycle_reuses_item_id_and_maps_completion_statuses() {
        let mut state = make_state();
        let running = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_task_2",
                    "sessionID": "ses_test123",
                    "tool": "task",
                    "state": {
                        "status": "running",
                        "input": { "subagent_type": "general", "prompt": "Do work" }
                    }
                }
            }
        });
        let completed = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_task_2",
                    "sessionID": "ses_test123",
                    "tool": "task",
                    "state": {
                        "status": "completed",
                        "input": { "subagent_type": "general", "prompt": "Do work" }
                    }
                }
            }
        });
        let running_events = translate_sse_event(&running, &mut state);
        let completed_events = translate_sse_event(&completed, &mut state);
        assert_eq!(running_events.len(), 1);
        assert_eq!(completed_events.len(), 1);
        assert_eq!(completed_events[0]["method"], "item/completed");
        assert_eq!(
            completed_events[0]["params"]["item"]["type"],
            "collabToolCall"
        );
        assert_eq!(completed_events[0]["params"]["item"]["status"], "completed");
        assert_eq!(
            running_events[0]["params"]["item"]["id"],
            completed_events[0]["params"]["item"]["id"]
        );
        assert_eq!(
            completed_events[0]["params"]["item"]["agentStatus"]["general"]["status"],
            "completed"
        );
    }

    #[test]
    fn task_tool_error_produces_failed_collab_tool_call() {
        let mut state = make_state();
        let error_event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_task_3",
                    "sessionID": "ses_test123",
                    "tool": "task",
                    "state": {
                        "status": "error",
                        "input": { "description": "Try something", "subagent_type": "explore" },
                        "output": "failed"
                    }
                }
            }
        });
        let events = translate_sse_event(&error_event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/completed");
        assert_eq!(events[0]["params"]["item"]["type"], "collabToolCall");
        assert_eq!(events[0]["params"]["item"]["status"], "failed");
        assert_eq!(
            events[0]["params"]["item"]["agentStatus"]["explore"]["status"],
            "failed"
        );
    }

    #[test]
    fn glob_tool_produces_explore_item_with_list_kind() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_glob_1",
                    "sessionID": "ses_test123",
                    "tool": "glob",
                    "state": {
                        "status": "running",
                        "title": "src/components",
                        "input": { "path": "src/components" }
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "item/started");
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "explore");
        assert_eq!(item["status"], "exploring");
        let entries = item["entries"].as_array().unwrap();
        assert_eq!(entries[0]["kind"], "list");
        assert_eq!(entries[0]["label"], "src/components");
    }

    #[test]
    fn list_tool_produces_explore_item() {
        let mut state = make_state();
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "type": "tool",
                    "id": "tc_list_1",
                    "sessionID": "ses_test123",
                    "tool": "list",
                    "state": {
                        "status": "completed",
                        "title": ".",
                        "input": {},
                        "output": "files..."
                    }
                }
            }
        });
        let events = translate_sse_event(&event, &mut state);
        let item = &events[0]["params"]["item"];
        assert_eq!(item["type"], "explore");
        let entries = item["entries"].as_array().unwrap();
        assert_eq!(entries[0]["kind"], "list");
        assert_eq!(entries[0]["label"], ".");
    }
}
