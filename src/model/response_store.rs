// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! OpenAI Responses server-side continuation store.
//!
//! The Kiro upstream is stateless from the OpenAI client's point of view. This
//! store reconstructs the prior Responses input/output history for
//! previous_response_id while keeping histories isolated by authenticated API key.

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
    response_id: String,
}

#[derive(Clone, Debug)]
struct StoredResponse {
    history: Vec<Value>,
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
    pub(crate) fn prepare_request(
        &self,
        owner_api_key_id: u32,
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
            Some(self.get_response(owner_api_key_id, id).ok_or_else(|| {
                "previous_response_id 未找到、已过期，或不属于当前 API Key".to_string()
            })?)
        } else {
            None
        };
        let previous_history = previous
            .as_ref()
            .map(|stored| stored.history.clone())
            .unwrap_or_default();

        let current = normalize_input_items(object.get("input"))?;
        let current = resolve_item_references(
            current,
            previous
                .as_ref()
                .map(|stored| &stored.history[stored.output_start..]),
        )?;

        let mut conversion_input = previous_history.clone();
        conversion_input.extend(current.iter().cloned());
        object.insert("input".to_string(), Value::Array(conversion_input));
        object.remove("previous_response_id");

        let mut history = previous_history;
        history.extend(current.into_iter().filter(is_history_item));

        Ok(PreparedResponsesRequest {
            body: prepared,
            history,
            store_response,
            continued: previous_response_id.is_some(),
        })
    }

    /// Create a persistence handle for one response, or None when store=false.
    pub(crate) fn persistence(
        self: &Arc<Self>,
        owner_api_key_id: u32,
        history: Vec<Value>,
        enabled: bool,
    ) -> Option<ResponsePersistence> {
        enabled.then(|| ResponsePersistence {
            store: Arc::clone(self),
            owner_api_key_id,
            history,
        })
    }

    fn get_response(&self, owner_api_key_id: u32, response_id: &str) -> Option<StoredResponse> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        prune_expired(&mut inner, now);

        let key = StoreKey {
            owner_api_key_id,
            response_id: response_id.to_string(),
        };
        let entry = inner.entries.get_mut(&key)?;
        // Active chains remain usable while requests continue to arrive.
        entry.expires_at = now + self.ttl;
        Some(entry.clone())
    }

    fn save_response(&self, owner_api_key_id: u32, base_history: &[Value], response: &Value) {
        let Some(response_id) = response.get("id").and_then(Value::as_str) else {
            tracing::warn!("Responses store: response 缺少 id，跳过保存");
            return;
        };
        if response.get("status").and_then(Value::as_str) == Some("failed") {
            return;
        }

        let mut history = base_history.to_vec();
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
                history,
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
    history: Vec<Value>,
}

impl ResponsePersistence {
    pub(crate) fn persist(&self, response: &Value) {
        self.store
            .save_response(self.owner_api_key_id, &self.history, response);
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

        let referenced = previous_output
            .iter()
            .rev()
            .find(|candidate| {
                candidate.get("id").and_then(Value::as_str) == Some(id)
                    || candidate.get("call_id").and_then(Value::as_str) == Some(id)
            })
            .cloned()
            .ok_or_else(|| format!("未知的 item_reference id: {id}"))?;
        resolved.push(referenced);
    }
    Ok(resolved)
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
