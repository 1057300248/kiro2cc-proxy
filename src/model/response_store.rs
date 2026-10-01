// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! OpenAI Responses server-side continuation store.
//!
//! The Kiro upstream is stateless from the OpenAI client's point of view. This
//! store reconstructs the prior Responses input/output history for
//! previous_response_id while keeping histories isolated by authenticated API key
//! or a trusted upstream tenant scope.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde_json::{Value, json};

const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
const DEFAULT_MAX_ENTRIES: usize = 1024;
const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct StoreKey {
    owner_api_key_id: u32,
    owner_scope: Option<String>,
    response_id: String,
}

#[derive(Clone, Debug)]
struct StoredResponse {
    history: Arc<Vec<Value>>,
    /// 本响应 output items 在累计 history 中的起始下标。
    output_start: usize,
    expires_at: Instant,
    size_bytes: usize,
}

#[derive(Default)]
struct StoreInner {
    entries: HashMap<StoreKey, StoredResponse>,
    total_bytes: usize,
}

/// Prepared request after continuation references have been resolved.
pub(crate) struct PreparedResponsesRequest {
    /// Body passed to the existing Responses request converter.
    pub(crate) body: Value,
    /// Canonical conversation history up to and including this request input.
    pub(crate) history: Vec<Value>,
    /// OpenAI Responses store flag. Defaults to true.
    pub(crate) store_response: bool,
    /// Whether this request continued a previous response.
    pub(crate) continued: bool,
}

/// Shared in-memory Responses continuation store.
pub(crate) struct ResponseStore {
    inner: Mutex<StoreInner>,
    ttl: Duration,
    max_entries: usize,
    max_total_bytes: usize,
    max_entry_bytes: usize,
}

impl Default for ResponseStore {
    fn default() -> Self {
        Self::new(
            DEFAULT_TTL,
            DEFAULT_MAX_ENTRIES,
            DEFAULT_MAX_TOTAL_BYTES,
            DEFAULT_MAX_ENTRY_BYTES,
        )
    }
}

impl ResponseStore {
    fn new(
        ttl: Duration,
        max_entries: usize,
        max_total_bytes: usize,
        max_entry_bytes: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
            ttl,
            max_entries,
            max_total_bytes,
            max_entry_bytes,
        }
    }

    /// Resolve previous_response_id and item_reference before protocol conversion.
    #[allow(dead_code)]
    pub(crate) fn prepare_request(
        &self,
        owner_api_key_id: u32,
        body: &Value,
    ) -> Result<PreparedResponsesRequest, String> {
        self.prepare_request_with_scope(owner_api_key_id, None, body)
    }

    /// Resolve a request using an optional trusted proxy tenant scope.
    pub(crate) fn prepare_request_with_scope(
        &self,
        owner_api_key_id: u32,
        owner_scope: Option<&str>,
        body: &Value,
    ) -> Result<PreparedResponsesRequest, String> {
        let mut prepared = body.clone();
        let object = prepared
            .as_object_mut()
            .ok_or_else(|| "请求体必须是 JSON object".to_string())?;

        let previous_response_id = match object.get("previous_response_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(s.trim().to_string()),
            Some(_) => {
                return Err("字段 'previous_response_id' 必须是字符串或 null".to_string());
            }
        };

        let store_response = match object.get("store") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(v)) => *v,
            Some(_) => return Err("字段 'store' 必须是 boolean".to_string()),
        };

        let previous = if let Some(id) = previous_response_id.as_deref() {
            Some(
                self.get_response(owner_api_key_id, owner_scope, id)
                    .ok_or_else(|| {
                        "previous_response_id 未找到、已过期，或不属于当前 API Key".to_string()
                    })?,
            )
        } else {
            None
        };
        let previous_history = previous
            .as_ref()
            .map(|stored| stored.history.as_ref().clone())
            .unwrap_or_default();

        let current = normalize_input_items(object.get("input"))?;
        let current = resolve_item_references(
            current,
            previous
                .as_ref()
                .map(|stored| &stored.history[stored.output_start..]),
        )?;

        let mut conversion_input = previous_history;
        append_unique_items(&mut conversion_input, current)?;
        if conversion_input
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
            && has_pending_tools(&conversion_input)
        {
            return Err("Cannot compact a conversation with unresolved tool calls; provide their outputs first.".to_string());
        }
        let history = conversion_input
            .iter()
            .filter(|item| is_history_item(item))
            .cloned()
            .collect();
        object.insert("input".to_string(), Value::Array(conversion_input));
        object.remove("previous_response_id");

        Ok(PreparedResponsesRequest {
            body: prepared,
            history,
            store_response,
            continued: previous_response_id.is_some(),
        })
    }

    /// Create a persistence handle for one response, or None when store=false.
    #[allow(dead_code)]
    pub(crate) fn persistence(
        self: &Arc<Self>,
        owner_api_key_id: u32,
        history: Vec<Value>,
        enabled: bool,
    ) -> Option<ResponsePersistence> {
        self.persistence_with_scope(owner_api_key_id, None, history, enabled)
    }

    /// Create a persistence handle scoped to an optional trusted proxy tenant.
    pub(crate) fn persistence_with_scope(
        self: &Arc<Self>,
        owner_api_key_id: u32,
        owner_scope: Option<&str>,
        history: Vec<Value>,
        enabled: bool,
    ) -> Option<ResponsePersistence> {
        enabled.then(|| ResponsePersistence {
            store: Arc::clone(self),
            owner_api_key_id,
            owner_scope: owner_scope.map(str::to_owned),
            history,
        })
    }

    fn get_response(
        &self,
        owner_api_key_id: u32,
        owner_scope: Option<&str>,
        response_id: &str,
    ) -> Option<StoredResponse> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        prune_expired(&mut inner, now);

        let key = StoreKey {
            owner_api_key_id,
            owner_scope: owner_scope.map(str::to_owned),
            response_id: response_id.to_string(),
        };
        let entry = inner.entries.get_mut(&key)?;
        // Active chains remain usable while requests continue to arrive.
        entry.expires_at = now + self.ttl;
        Some(entry.clone())
    }

    fn save_response(
        &self,
        owner_api_key_id: u32,
        owner_scope: Option<&str>,
        base_history: &[Value],
        response: &Value,
    ) {
        let Some(response_id) = response.get("id").and_then(Value::as_str) else {
            tracing::warn!("Responses store: response 缺少 id，跳过保存");
            return;
        };
        if !matches!(
            response.get("status").and_then(Value::as_str),
            Some("completed") | Some("incomplete")
        ) {
            return;
        }
        let compacted = response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.len() == 1
                    && items[0].get("type").and_then(Value::as_str) == Some("compaction")
            });
        if compacted && response.get("status").and_then(Value::as_str) != Some("completed") {
            return;
        }
        let mut history = if compacted {
            // Preserve explicit conversation-level system/developer messages only.
            // Top-level instructions are per-request and are deliberately not inherited.
            base_history
                .iter()
                .filter(|item| {
                    matches!(
                        item.get("role").and_then(Value::as_str),
                        Some("system") | Some("developer")
                    )
                })
                .cloned()
                .collect()
        } else {
            base_history.to_vec()
        };
        let output_start = history.len();
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            history.extend(output.iter().filter(|item| is_history_item(item)).cloned());
        }

        let size_bytes = match serde_json::to_vec(&history) {
            Ok(bytes) => bytes.len(),
            Err(error) => {
                tracing::warn!(%error, "Responses store: 历史序列化失败，跳过保存");
                return;
            }
        };
        if size_bytes > self.max_entry_bytes {
            tracing::warn!(
                response_id,
                size_bytes,
                max_entry_bytes = self.max_entry_bytes,
                "Responses store: 单条会话历史过大，跳过保存"
            );
            return;
        }

        let now = Instant::now();
        let key = StoreKey {
            owner_api_key_id,
            owner_scope: owner_scope.map(str::to_owned),
            response_id: response_id.to_string(),
        };
        let mut inner = self.inner.lock();
        prune_expired(&mut inner, now);

        if let Some(old) = inner.entries.remove(&key) {
            inner.total_bytes = inner.total_bytes.saturating_sub(old.size_bytes);
        }

        while !inner.entries.is_empty()
            && (inner.entries.len() >= self.max_entries
                || inner.total_bytes.saturating_add(size_bytes) > self.max_total_bytes)
        {
            evict_oldest(&mut inner);
        }

        if inner.total_bytes.saturating_add(size_bytes) > self.max_total_bytes {
            tracing::warn!(
                response_id,
                size_bytes,
                max_total_bytes = self.max_total_bytes,
                "Responses store: 总内存预算不足，跳过保存"
            );
            return;
        }

        inner.total_bytes = inner.total_bytes.saturating_add(size_bytes);
        inner.entries.insert(
            key,
            StoredResponse {
                history: Arc::new(history),
                output_start,
                expires_at: now + self.ttl,
                size_bytes,
            },
        );
    }
}

/// Per-response persistence context shared by non-stream and stream paths.
#[derive(Clone)]
pub(crate) struct ResponsePersistence {
    store: Arc<ResponseStore>,
    owner_api_key_id: u32,
    owner_scope: Option<String>,
    history: Vec<Value>,
}

impl ResponsePersistence {
    pub(crate) fn persist(&self, response: &Value) {
        self.store.save_response(
            self.owner_api_key_id,
            self.owner_scope.as_deref(),
            &self.history,
            response,
        );
    }
}

fn normalize_input_items(input: Option<&Value>) -> Result<Vec<Value>, String> {
    match input {
        Some(Value::String(text)) => {
            if text.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}],
                })])
            }
        }
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => Err("字段 'input' 缺失或类型不支持（应为字符串或数组）".to_string()),
    }
}

fn resolve_item_references(
    items: Vec<Value>,
    previous_output: Option<&[Value]>,
) -> Result<Vec<Value>, String> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("item_reference") {
            resolved.push(item);
            continue;
        }

        let Some(previous_output) = previous_output else {
            return Err("item_reference requires previous_response_id".to_string());
        };
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "item_reference.id 缺失或为空".to_string())?;

        let _referenced = previous_output
            .iter()
            .rev()
            .find(|candidate| {
                candidate.get("id").and_then(Value::as_str) == Some(id)
                    || candidate.get("call_id").and_then(Value::as_str) == Some(id)
            })
            .cloned()
            .ok_or_else(|| format!("未知的 item_reference id: {id}"))?;
        // Already included by previous_response_id; validate, but never replay twice.
    }
    Ok(resolved)
}

fn item_keys(item: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(id) = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        keys.push(format!("id:{id}"));
    }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let prefix = match kind {
        "function_call" | "custom_tool_call" => Some("call"),
        "function_call_output" | "custom_tool_call_output" => Some("result"),
        _ => None,
    };
    if let (Some(prefix), Some(id)) = (prefix, item.get("call_id").and_then(Value::as_str)) {
        keys.push(format!("{prefix}:{id}"));
    }
    keys
}

fn append_unique_items(history: &mut Vec<Value>, current: Vec<Value>) -> Result<(), String> {
    let mut known: HashMap<String, usize> = HashMap::new();
    for (index, item) in history.iter().enumerate() {
        for key in item_keys(item) {
            known.insert(key, index);
        }
    }
    for item in current {
        let keys = item_keys(&item);
        let mut duplicate = false;
        for key in &keys {
            if let Some(&index) = known.get(key) {
                if history[index] != item {
                    return Err("Conflicting duplicate item or tool call ID in input.".to_string());
                }
                duplicate = true;
            }
        }
        if !duplicate {
            let index = history.len();
            for key in keys {
                known.insert(key, index);
            }
            history.push(item);
        }
    }
    Ok(())
}

fn has_pending_tools(items: &[Value]) -> bool {
    let mut pending = std::collections::HashSet::new();
    for item in items {
        let Some(id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") | Some("custom_tool_call") => {
                pending.insert(id);
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                pending.remove(id);
            }
            _ => {}
        }
    }
    !pending.is_empty()
}

fn is_history_item(item: &Value) -> bool {
    history_item_type(item).is_some()
}

fn history_item_type(item: &Value) -> Option<&str> {
    match item.get("type").and_then(Value::as_str) {
        Some(
            "message"
            | "function_call"
            | "custom_tool_call"
            | "function_call_output"
            | "custom_tool_call_output"
            | "reasoning"
            | "compaction"
            | "compaction_summary",
        ) => item.get("type").and_then(Value::as_str),
        None if item.get("role").is_some() => Some("message"),
        _ => None,
    }
}

fn prune_expired(inner: &mut StoreInner, now: Instant) {
    let expired: Vec<StoreKey> = inner
        .entries
        .iter()
        .filter(|(_, entry)| entry.expires_at <= now)
        .map(|(key, _)| key.clone())
        .collect();
    for key in expired {
        if let Some(entry) = inner.entries.remove(&key) {
            inner.total_bytes = inner.total_bytes.saturating_sub(entry.size_bytes);
        }
    }
}

fn evict_oldest(inner: &mut StoreInner) {
    let oldest = inner
        .entries
        .iter()
        .min_by_key(|(_, entry)| entry.expires_at)
        .map(|(key, _)| key.clone());
    if let Some(key) = oldest
        && let Some(entry) = inner.entries.remove(&key)
    {
        inner.total_bytes = inner.total_bytes.saturating_sub(entry.size_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Arc<ResponseStore> {
        Arc::new(ResponseStore::new(
            Duration::from_secs(60),
            16,
            1024 * 1024,
            256 * 1024,
        ))
    }

    fn save_seed(store: &Arc<ResponseStore>, owner: u32) {
        let persistence = store
            .persistence(
                owner,
                vec![json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "first"}],
                })],
                true,
            )
            .unwrap();
        persistence.persist(&json!({
            "id": "resp_seed",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_seed",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer", "annotations": []}],
            }],
        }));
    }

    fn save_scoped_seed(store: &Arc<ResponseStore>, owner: u32, scope: &str) {
        let persistence = store
            .persistence_with_scope(
                owner,
                Some(scope),
                vec![json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "first"}],
                })],
                true,
            )
            .unwrap();
        persistence.persist(&json!({
            "id": "resp_scoped",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_scoped",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer", "annotations": []}],
            }],
        }));
    }

    #[test]
    fn previous_response_is_replayed_for_same_api_key() {
        let store = test_store();
        save_seed(&store, 7);

        let prepared = store
            .prepare_request(
                7,
                &json!({
                    "model": "gpt-5-codex",
                    "previous_response_id": "resp_seed",
                    "input": "second",
                }),
            )
            .unwrap();

        let input = prepared.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["text"], "second");
        assert!(prepared.continued);
    }

    #[test]
    fn previous_response_is_isolated_by_api_key() {
        let store = test_store();
        save_seed(&store, 7);
        let err = store
            .prepare_request(
                8,
                &json!({
                    "model": "gpt-5-codex",
                    "previous_response_id": "resp_seed",
                    "input": "second",
                }),
            )
            .err()
            .expect("other API key must not resolve the response");
        assert!(err.contains("未找到"));
    }

    #[test]
    fn previous_response_is_isolated_by_trusted_tenant_scope() {
        let store = test_store();
        save_scoped_seed(&store, 7, "tenant-a");

        assert!(
            store
                .prepare_request_with_scope(
                    7,
                    Some("tenant-a"),
                    &json!({
                        "model": "gpt-5-codex",
                        "previous_response_id": "resp_scoped",
                        "input": "same tenant",
                    }),
                )
                .is_ok()
        );

        let err = store
            .prepare_request_with_scope(
                7,
                Some("tenant-b"),
                &json!({
                    "model": "gpt-5-codex",
                    "previous_response_id": "resp_scoped",
                    "input": "other tenant",
                }),
            )
            .err()
            .expect("other tenant must not resolve the response");
        assert!(err.contains("未找到"));
    }

    #[test]
    fn item_reference_is_resolved_from_previous_output() {
        let store = test_store();
        save_seed(&store, 7);

        let prepared = store
            .prepare_request(
                7,
                &json!({
                    "model": "gpt-5-codex",
                    "previous_response_id": "resp_seed",
                    "input": [
                        {"type": "item_reference", "id": "msg_seed"},
                        {"type": "message", "role": "user", "content": "continue"}
                    ],
                }),
            )
            .unwrap();

        let input = prepared.body["input"].as_array().unwrap();
        assert_eq!(input.last().unwrap()["role"], "user");
        assert!(
            input
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some("msg_seed"))
        );
    }

    #[test]
    fn item_reference_without_previous_response_is_rejected() {
        let store = test_store();
        let err = store
            .prepare_request(
                7,
                &json!({
                    "model": "gpt-5-codex",
                    "input": [{"type": "item_reference", "id": "msg_seed"}],
                }),
            )
            .err()
            .expect("reference without previous response must fail");
        assert!(err.contains("previous_response_id"));
    }

    #[test]
    fn expired_response_cannot_be_resumed() {
        let store = Arc::new(ResponseStore::new(
            Duration::ZERO,
            16,
            1024 * 1024,
            256 * 1024,
        ));
        save_seed(&store, 7);
        let err = store
            .prepare_request(
                7,
                &json!({
                    "model": "gpt-5-codex",
                    "previous_response_id": "resp_seed",
                    "input": "second",
                }),
            )
            .err()
            .expect("expired response must not be resumable");
        assert!(err.contains("未找到") || err.contains("已过期"));
    }

    #[test]
    fn failed_response_is_not_saved() {
        let store = test_store();
        let persistence = store
            .persistence(7, Vec::new(), true)
            .expect("persistence enabled");
        persistence.persist(&json!({
            "id": "resp_failed",
            "status": "failed",
            "output": []
        }));
        let err = store
            .prepare_request(
                7,
                &json!({
                    "model": "gpt-5-codex",
                    "previous_response_id": "resp_failed",
                    "input": "retry",
                }),
            )
            .err()
            .expect("failed response must not be stored");
        assert!(err.contains("未找到"));
    }

    #[test]
    fn capacity_evicts_the_oldest_response() {
        let store = Arc::new(ResponseStore::new(
            Duration::from_secs(60),
            1,
            1024 * 1024,
            256 * 1024,
        ));
        save_seed(&store, 7);
        let second = store
            .persistence(7, Vec::new(), true)
            .expect("persistence enabled");
        second.persist(&json!({
            "id": "resp_second",
            "status": "completed",
            "output": [{"type": "message", "id": "msg_second", "role": "assistant", "content": []}]
        }));
        assert!(
            store
                .prepare_request(
                    7,
                    &json!({
                        "model": "gpt-5-codex",
                        "previous_response_id": "resp_seed",
                        "input": "old"
                    }),
                )
                .is_err()
        );
        assert!(
            store
                .prepare_request(
                    7,
                    &json!({
                        "model": "gpt-5-codex",
                        "previous_response_id": "resp_second",
                        "input": "new"
                    }),
                )
                .is_ok()
        );
    }

    #[test]
    fn store_defaults_true_and_false_is_honored() {
        let store = test_store();
        let defaulted = store
            .prepare_request(7, &json!({"model": "gpt-5-codex", "input": "x"}))
            .unwrap();
        assert!(defaulted.store_response);

        let disabled = store
            .prepare_request(
                7,
                &json!({"model": "gpt-5-codex", "input": "x", "store": false}),
            )
            .unwrap();
        assert!(!disabled.store_response);
        assert!(
            store
                .persistence(7, disabled.history, disabled.store_response)
                .is_none()
        );
    }
}
#[cfg(test)]
mod review_20261001 {
    use super::*;
    fn store() -> Arc<ResponseStore> {
        Arc::new(ResponseStore::default())
    }
    fn seed(s: &Arc<ResponseStore>, scope: &str, output: Value) {
        s.persistence_with_scope(
            7,
            Some(scope),
            vec![json!({"role":"user","content":"first"})],
            true,
        )
        .unwrap()
        .persist(&json!({"id":"resp_seed","status":"completed","output":[output]}));
    }
    #[test]
    fn references_do_not_repeat_calls_or_messages() {
        for item in [
            json!({"id":"i","type":"message","role":"assistant","content":"answer"}),
            json!({"id":"i","type":"function_call","call_id":"c","name":"f","arguments":"{}"}),
            json!({"id":"i","type":"custom_tool_call","call_id":"c","name":"f","input":"x"}),
        ] {
            let s = store();
            seed(&s, "a", item);
            let prepared = s.prepare_request_with_scope(7,Some("a"),&json!({"input":[{"type":"item_reference","id":"i"},{"role":"user","content":"next"}],"previous_response_id":"resp_seed"})).unwrap();
            assert_eq!(
                prepared.body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|v| v["id"] == "i")
                    .count(),
                1
            );
        }
    }
    #[test]
    fn conflicting_tool_id_is_rejected_but_repeated_text_is_kept() {
        let mut history =
            vec![json!({"type":"function_call","call_id":"c","name":"f","arguments":"{}"})];
        let same = history[0].clone();
        append_unique_items(&mut history, vec![same]).unwrap();
        assert_eq!(history.len(), 1);
        assert!(
            append_unique_items(
                &mut history,
                vec![json!({"type":"function_call","call_id":"c","name":"g","arguments":"{}"})]
            )
            .is_err()
        );
        append_unique_items(
            &mut history,
            vec![
                json!({"role":"user","content":"again"}),
                json!({"role":"user","content":"again"}),
            ],
        )
        .unwrap();
        assert_eq!(history.len(), 3);
    }
    #[test]
    fn compacted_snapshot_replaces_old_history() {
        let s = store();
        let history = vec![
            json!({"role":"developer","content":"rules"}),
            json!({"role":"user","content":"large old transcript"}),
        ];
        s.persistence(7,history,true).unwrap().persist(&json!({"id":"compact","status":"completed","output":[{"type":"compaction","id":"cmp","encrypted_content":"c3VtbWFyeQ=="}]}));
        let p = s
            .prepare_request(7, &json!({"previous_response_id":"compact","input":"next"}))
            .unwrap();
        assert!(!p.body.to_string().contains("large old transcript"));
        assert!(p.body.to_string().contains("rules"));
        assert_eq!(p.body["input"].as_array().unwrap().len(), 3);
    }
    #[test]
    fn compaction_rejects_unresolved_tools() {
        let s = store();
        seed(
            &s,
            "a",
            json!({"type":"function_call","id":"i","call_id":"c","name":"f","arguments":"{}"}),
        );
        assert!(
            s.prepare_request_with_scope(
                7,
                Some("a"),
                &json!({"previous_response_id":"resp_seed","input":[{"type":"compaction_trigger"}]})
            )
            .is_err()
        );
        assert!(s.prepare_request_with_scope(7,Some("a"),&json!({"previous_response_id":"resp_seed","input":[{"type":"function_call_output","call_id":"c","output":"done"},{"type":"compaction_trigger"}]})).is_ok());
    }
    #[test]
    fn scoped_history_cannot_fall_back_to_unscoped_namespace() {
        let s = store();
        seed(&s, "a", json!({"role":"assistant","content":"answer"}));
        for scope in [None, Some("b")] {
            assert!(
                s.prepare_request_with_scope(
                    7,
                    scope,
                    &json!({"previous_response_id":"resp_seed","input":"x"})
                )
                .is_err()
            );
        }
    }
    #[test]
    fn unfinished_and_failed_responses_are_not_saved() {
        let s = store();
        for status in ["in_progress", "failed", "queued"] {
            s.persistence(7, Vec::new(), true)
                .unwrap()
                .persist(&json!({"id":status,"status":status,"output":[]}));
            assert!(
                s.prepare_request(7, &json!({"previous_response_id":status,"input":"x"}))
                    .is_err()
            );
        }
    }
}
