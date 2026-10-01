// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Transport completion and tool-argument integrity, independent of wire formatting.
use serde_json::Value;
use std::collections::HashMap;

const MAX_STREAM_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct StreamIntegrity {
    pub(crate) stopped: bool,
    bytes_seen: usize,
    tools: HashMap<i64, String>,
}

impl StreamIntegrity {
    pub(crate) fn observe(&mut self, name: &str, data: &Value) -> Result<(), &'static str> {
        let index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
        if name == "content_block_start" {
            if let Some(block) = data.get("content_block") {
                for key in ["text", "thinking"] {
                    self.bytes_seen = self.bytes_seen.saturating_add(
                        block
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::len)
                            .unwrap_or(0),
                    );
                }
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block.get("id").and_then(Value::as_str).is_some()
                    && block.get("name").and_then(Value::as_str).is_some()
                {
                    if self.tools.insert(index, String::new()).is_some() {
                        return Err("Upstream reused an unfinished tool block index.");
                    }
                }
            }
        }
        if name == "content_block_delta" {
            if let Some(delta) = data.get("delta") {
                for key in ["text", "thinking", "partial_json"] {
                    self.bytes_seen = self.bytes_seen.saturating_add(
                        delta
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::len)
                            .unwrap_or(0),
                    );
                }
                if self.bytes_seen > MAX_STREAM_OUTPUT_BYTES {
                    return Err("Upstream streaming output exceeded the safety limit.");
                }
                if let Some(args) = self.tools.get_mut(&index) {
                    if let Some(part) = delta.get("partial_json").and_then(Value::as_str) {
                        args.push_str(part);
                    }
                }
            }
        }
        if self.bytes_seen > MAX_STREAM_OUTPUT_BYTES {
            return Err("Upstream streaming output exceeded the safety limit.");
        }
        if name == "content_block_stop" {
            if let Some(args) = self.tools.remove(&index) {
                Self::validate_arguments(&args)?;
            }
        }
        if name == "message_stop" {
            // A trusted message_stop can close a missing block_stop, but cannot
            // turn half-written JSON into an executable function call.
            for args in self.tools.values() {
                Self::validate_arguments(args)?;
            }
            self.tools.clear();
            self.stopped = true;
        }
        Ok(())
    }

    fn validate_arguments(args: &str) -> Result<(), &'static str> {
        if args.trim().is_empty() {
            return Ok(()); // an explicitly completed no-argument tool
        }
        if serde_json::from_str::<Value>(args).is_ok_and(|value| value.is_object()) {
            Ok(())
        } else {
            Err("Upstream completed a tool call with invalid or incomplete JSON arguments.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn eof_is_not_completion() {
        let mut guard = StreamIntegrity::default();
        guard
            .observe("content_block_delta", &json!({"delta":{"text":"partial"}}))
            .unwrap();
        assert!(!guard.stopped);
    }
    #[test]
    fn unfinished_tool_json_cannot_complete() {
        let mut guard = StreamIntegrity::default();
        guard
            .observe(
                "content_block_start",
                &json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}}),
            )
            .unwrap();
        guard
            .observe(
                "content_block_delta",
                &json!({"index":0,"delta":{"partial_json":"{\"x\":"}}),
            )
            .unwrap();
        assert!(guard.observe("message_stop", &json!({})).is_err());
        assert!(!guard.stopped);
    }
    #[test]
    fn complete_tool_json_can_finish_without_delta_stop_reason() {
        let mut guard = StreamIntegrity::default();
        guard
            .observe(
                "content_block_start",
                &json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}}),
            )
            .unwrap();
        guard
            .observe(
                "content_block_delta",
                &json!({"index":0,"delta":{"partial_json":"{}"}}),
            )
            .unwrap();
        guard.observe("message_stop", &json!({})).unwrap();
        assert!(guard.stopped);
    }
}
