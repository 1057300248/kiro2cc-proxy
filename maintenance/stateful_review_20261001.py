#!/usr/bin/env python3
"""Apply bounded, exact-source review fixes on the existing draft branch.
CI runs this once, formats/tests the result, and only then publishes source changes.
This script is not used by the application at runtime.
"""
from pathlib import Path
import re

changed = set()

def put(path, text):
    p = Path(path)
    if p.exists() and p.read_text(encoding='utf-8') == text:
        return
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding='utf-8')
    changed.add(path)

def rep(path, old, new, count=1):
    text = Path(path).read_text(encoding='utf-8')
    if new in text and old not in text:
        return
    actual = text.count(old)
    if actual != count:
        raise RuntimeError(f'{path}: expected {count} exact targets, found {actual}: {old[:100]!r}')
    put(path, text.replace(old, new))

def method(path, name, replacement, indent=4):
    text = Path(path).read_text(encoding='utf-8')
    prefix = ' ' * indent
    start = re.search(rf'(?m)^{prefix}(?:pub\(crate\) )?fn {re.escape(name)}\(', text)
    if not start:
        raise RuntimeError(f'{path}: missing function {name}')
    end = re.search(rf'(?m)^{prefix}\}}', text[start.end():])
    if not end:
        raise RuntimeError(f'{path}: no end for {name}')
    endpos = start.end() + end.end()
    put(path, text[:start.start()] + replacement.rstrip() + text[endpos:])

def append_tests(path, text):
    s = Path(path).read_text(encoding='utf-8')
    marker = '\n#[cfg(test)]\nmod review_20261001 {'
    if marker in s:
        s = s[:s.index(marker)]
    put(path, s.rstrip() + '\n' + text.strip() + '\n')

# One integrity guard shared by both public streaming protocols. A TCP/SSE EOF is
# not a successful message_stop. Validate buffered tool JSON before declaring it done.
put('src/openai/stream_integrity.rs', r'''// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Transport completion and tool-argument integrity, independent of wire formatting.
use std::collections::HashMap;
use serde_json::Value;

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
                        block.get(key).and_then(Value::as_str).map(str::len).unwrap_or(0));
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
                        delta.get(key).and_then(Value::as_str).map(str::len).unwrap_or(0));
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
        guard.observe("content_block_delta", &json!({"delta":{"text":"partial"}})).unwrap();
        assert!(!guard.stopped);
    }
    #[test]
    fn unfinished_tool_json_cannot_complete() {
        let mut guard = StreamIntegrity::default();
        guard.observe("content_block_start", &json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}})).unwrap();
        guard.observe("content_block_delta", &json!({"index":0,"delta":{"partial_json":"{\"x\":"}})).unwrap();
        assert!(guard.observe("message_stop", &json!({})).is_err());
        assert!(!guard.stopped);
    }
    #[test]
    fn complete_tool_json_can_finish_without_delta_stop_reason() {
        let mut guard = StreamIntegrity::default();
        guard.observe("content_block_start", &json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}})).unwrap();
        guard.observe("content_block_delta", &json!({"index":0,"delta":{"partial_json":"{}"}})).unwrap();
        guard.observe("message_stop", &json!({})).unwrap();
        assert!(guard.stopped);
    }
}
''')
rep('src/openai/mod.rs', 'mod sse;', 'mod sse;\nmod stream_integrity;')

rs = 'src/openai/responses_response/stream.rs'
cs = 'src/openai/chat_response/stream.rs'
for path, terminal in [(rs, 'finished'), (cs, 'done_sent')]:
    rep(path, 'use serde_json::{Value, json};', 'use serde_json::{Value, json};\nuse crate::openai::stream_integrity::StreamIntegrity;')
    anchor = '    finished: bool,' if path == rs else '    done_sent: bool,'
    rep(path, anchor, anchor + '\n    integrity: StreamIntegrity,')
    anchor = '            finished: false,' if path == rs else '            done_sent: false,'
    rep(path, anchor, anchor + '\n            integrity: StreamIntegrity::default(),')
    anchor = '    pub(crate) fn on_event(&mut self, name: &str, data: &Value) -> Vec<String> {\n'
    rep(path, anchor, anchor + f'''        if self.{terminal} {{ return Vec::new(); }}
        if let Err(message) = self.integrity.observe(name, data) {{
            return self.on_error(&json!({{"error": {{"type": "api_error", "message": message}}}}));
        }}
''')
    # The getter is used by the body wrapper to stop polling after any terminal event.
    anchor = '    /// 处理一个上游事件，返回待下发的 SSE 帧\n'
    rep(path, anchor, f'    pub(crate) fn is_finished(&self) -> bool {{ self.{terminal} }}\n\n' + anchor)

rep(rs, '    context_exceeded: bool,', '    context_exceeded: bool,\n    aborting: bool,')
rep(rs, '            context_exceeded: false,', '            context_exceeded: false,\n            aborting: false,')
rep(rs, '''        if self.finished {
            return Vec::new();
        }
        // 即使上游一个内容块都没给，也要让客户端看到合法的事件序列''', '''        if self.finished {
            return Vec::new();
        }
        if !self.integrity.stopped {
            return self.on_error(&json!({"error": {"type": "api_error", "message": "Upstream stream ended before message_stop."}}));
        }
        if self.is_compaction && self.truncated {
            return self.on_error(&json!({"error": {"type": "api_error", "message": "Context compaction was truncated; the previous history was not replaced."}}));
        }
        // 即使上游一个内容块都没给，也要让客户端看到合法的事件序列''')
rep(cs, '''        if self.done_sent {
            return frames;
        }
        // 极端情况下''', '''        if self.done_sent {
            return frames;
        }
        if !self.integrity.stopped {
            return self.on_error(&json!({"error": {"type": "api_error", "message": "Upstream stream ended before message_stop."}}));
        }
        // 极端情况下''')
# Suppressed compaction content must not consume externally visible sequence numbers.
rep(rs, '''        let pass = |f: Vec<String>| if is_compaction { Vec::new() } else { f };''', '''        let pass = |f: Vec<String>| if is_compaction { Vec::new() } else { f };''') if False else None
for event, expression in [('content_block_start', 'self.on_block_start(data)'), ('content_block_delta', 'self.on_block_delta(data)'), ('content_block_stop', 'self.close_item(block_index(data))')]:
    old = f'            "{event}" => pass({expression}),'
    new = f'''            "{event}" => {{
                let initial = if is_compaction {{ self.ensure_created() }} else {{ Vec::new() }};
                let sequence = self.sequence;
                let events = {expression};
                if is_compaction {{ self.sequence = sequence; initial }} else {{ events }}
            }},'''
    rep(rs, old, new)
rep(rs, '        let pass = |f: Vec<String>| if is_compaction { Vec::new() } else { f };\n', '')
rep(rs, '''        let mut frames = self.ensure_created();
        frames.extend(self.close_all_open());

        self.finished = true;''', '''        let mut frames = self.ensure_created();
        let sequence = self.sequence;
        let closing = self.close_all_open();
        if self.is_compaction { self.sequence = sequence; } else { frames.extend(closing); }

        self.finished = true;''')
# Every incomplete open item remains incomplete, with no executable tool .done event.
rep(rs, '"status": "completed",', '"status": if self.aborting { "incomplete" } else { "completed" },', count=3)
method(rs, 'on_error', r'''    fn on_error(&mut self, data: &Value) -> Vec<String> {
        if self.finished { return Vec::new(); }
        let (message, error_type, code) = super::super::error::extract_stream_error(data);
        let mut frames = self.ensure_created();
        self.aborting = true;
        let sequence = self.sequence;
        let closing = self.close_all_open();
        if self.is_compaction {
            self.sequence = sequence;
            self.completed_items.clear();
        } else {
            // Re-number after suppressing partial tool input/arguments completion.
            self.sequence = sequence;
            for frame in closing {
                let Some((event, payload)) = frame.split_once("\ndata: ") else { continue; };
                let name = event.trim_start_matches("event: ");
                if matches!(name, "response.function_call_arguments.done" | "response.custom_tool_call_input.done" | "response.custom_tool_call_input.delta") { continue; }
                if let Ok(value) = serde_json::from_str::<Value>(payload.trim()) {
                    frames.push(self.event(name, value));
                }
            }
        }
        self.finished = true;
        frames.push(self.failed_event(error_type, code, &message));
        frames
    }''')
rep(rs, '''        if summary.is_empty() {
            tracing::warn!(''', '''        if summary.is_empty() {
            self.completed_items.clear();
            frames.push(self.failed_event("server_error", None, "Context compaction returned no usable summary."));
            return frames;
        }
        if summary.is_empty() {
            tracing::warn!(''')
# Remove the now-unreachable warning branch without altering the successful path.
s = Path(rs).read_text()
s = re.sub(r'        if summary\.is_empty\(\) \{\n            tracing::warn!\(.*?\n        \} else \{(.*?)\n        \}\n\n        let cmp_item', r'        {\1\n        }\n\n        let cmp_item', s, flags=re.S)
put(rs, s)

# Actual body wrapper must stop after terminal frames, not read indefinitely or append late events.
h = 'src/openai/handlers.rs'
rep(h, '    fn finish(&mut self) -> Vec<String>;\n', '    fn finish(&mut self) -> Vec<String>;\n    fn is_finished(&self) -> bool;\n')
for typ in ['ChatStreamConverter', 'ResponsesStreamConverter']:
    anchor = f'impl StreamConverter for {typ} {{\n'
    rep(h, anchor, anchor + f'    fn is_finished(&self) -> bool {{ {typ}::is_finished(self) }}\n')
rep(h, '''                                buf.push_str(&frame);
                            }
                        }
                        // SseItem::Done''', '''                                buf.push_str(&frame);
                            }
                            if converter.is_finished() { break; }
                        }
                        // SseItem::Done''')
rep(h, '''                    return Some((
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(buf)),
                        Some((ds, parser, converter)),
                    ));''', '''                    let next = if converter.is_finished() { None } else { Some((ds, parser, converter)) };
                    return Some((Ok::<Bytes, std::convert::Infallible>(Bytes::from(buf)), next));''')

# Non-stream context exhaustion uses the same failed response/error as streaming.
ns = 'src/openai/responses_response/nonstream.rs'
rep(ns, '''        response["status"] = json!("incomplete");
        response["incomplete_details"] = json!({"reason": "context_window_exceeded"});''', '''        response["status"] = json!("failed");
        response["error"] = json!({"type": "invalid_request_error", "code": "context_length_exceeded", "message": "Conversation context exceeded the model's context window. Compact the conversation or start a new one, then retry."});''')
rep(ns, '''    response
}''', '''    if is_compaction && (is_truncated(stop_reason) || response["output"][0]["encrypted_content"].as_str() == Some("")) {
        response["status"] = json!("failed");
        response.as_object_mut().unwrap().remove("incomplete_details");
        response["error"] = json!({"type":"server_error", "code":"compaction_failed", "message":"Context compaction did not produce a complete, usable summary."});
        response["output"] = json!([]);
    }
    response
}''')

# Continuation: reference = identity check, not a second copy of auto-replayed output.
st = 'src/model/response_store.rs'
rep(st, '''struct StoredResponse {
    history: Vec<Value>,''', '''struct StoredResponse {
    history: Arc<Vec<Value>>,''')
rep(st, '.map(|stored| stored.history.clone())', '.map(|stored| stored.history.as_ref().clone())')
rep(st, '''        let mut conversion_input = previous_history.clone();
        conversion_input.extend(current.iter().cloned());
        object.insert("input".to_string(), Value::Array(conversion_input));
        object.remove("previous_response_id");

        let mut history = previous_history;
        history.extend(current.into_iter().filter(is_history_item));''', '''        let mut conversion_input = previous_history;
        append_unique_items(&mut conversion_input, current)?;
        if conversion_input.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
            && has_pending_tools(&conversion_input)
        {
            return Err("Cannot compact a conversation with unresolved tool calls; provide their outputs first.".to_string());
        }
        let history = conversion_input.iter().filter(|item| is_history_item(item)).cloned().collect();
        object.insert("input".to_string(), Value::Array(conversion_input));
        object.remove("previous_response_id");''')
rep(st, '        let referenced = previous_output\n', '        let _referenced = previous_output\n')
rep(st, '        resolved.push(referenced);', '        // Already included by previous_response_id; validate, but never replay twice.')
rep(st, '''        if response.get("status").and_then(Value::as_str) == Some("failed") {
            return;
        }

        let mut history = base_history.to_vec();''', '''        if !matches!(response.get("status").and_then(Value::as_str), Some("completed") | Some("incomplete")) {
            return;
        }
        let compacted = response.get("output").and_then(Value::as_array)
            .is_some_and(|items| items.len() == 1 && items[0].get("type").and_then(Value::as_str) == Some("compaction"));
        if compacted && response.get("status").and_then(Value::as_str) != Some("completed") { return; }
        let mut history = if compacted {
            // Preserve explicit conversation-level system/developer messages only.
            // Top-level instructions are per-request and are deliberately not inherited.
            base_history.iter().filter(|item| matches!(item.get("role").and_then(Value::as_str), Some("system") | Some("developer"))).cloned().collect()
        } else { base_history.to_vec() };''')
rep(st, '''            StoredResponse {
                history,
                output_start,''', '''            StoredResponse {
                history: Arc::new(history),
                output_start,''')
# Avoid duplicate ID or call/result identity even when a client re-sends explicit output.
anchor = 'fn is_history_item(item: &Value) -> bool {'
helpers = r'''fn item_keys(item: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(id) = item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) { keys.push(format!("id:{id}")); }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let prefix = match kind {
        "function_call" | "custom_tool_call" => Some("call"),
        "function_call_output" | "custom_tool_call_output" => Some("result"),
        _ => None,
    };
    if let (Some(prefix), Some(id)) = (prefix, item.get("call_id").and_then(Value::as_str)) { keys.push(format!("{prefix}:{id}")); }
    keys
}

fn append_unique_items(history: &mut Vec<Value>, current: Vec<Value>) -> Result<(), String> {
    let mut known: HashMap<String, usize> = HashMap::new();
    for (index, item) in history.iter().enumerate() {
        for key in item_keys(item) { known.insert(key, index); }
    }
    for item in current {
        let keys = item_keys(&item);
        let mut duplicate = false;
        for key in &keys {
            if let Some(&index) = known.get(key) {
                if history[index] != item { return Err("Conflicting duplicate item or tool call ID in input.".to_string()); }
                duplicate = true;
            }
        }
        if !duplicate {
            let index = history.len();
            for key in keys { known.insert(key, index); }
            history.push(item);
        }
    }
    Ok(())
}

fn has_pending_tools(items: &[Value]) -> bool {
    let mut pending = std::collections::HashSet::new();
    for item in items {
        let Some(id) = item.get("call_id").and_then(Value::as_str) else { continue; };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") | Some("custom_tool_call") => { pending.insert(id); }
            Some("function_call_output") | Some("custom_tool_call_output") => { pending.remove(id); }
            _ => {}
        }
    }
    !pending.is_empty()
}

'''
rep(st, anchor, helpers + anchor)

# Unambiguous trusted header: duplicate/coalesced values must fail closed.
mw = 'src/anthropic/middleware.rs'
method(mw, 'response_store_scope', r'''fn response_store_scope(request: &Request<Body>, header_name: Option<&str>) -> Option<String> {
    let name = header_name?;
    let mut values = request.headers().get_all(name).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() || value.is_empty() || value.len() > 1024 || value.contains(',') { return None; }
    let digest = Sha256::digest(value.as_bytes());
    Some(format!("sha256:{digest:x}"))
}''', indent=0)
main = 'src/main.rs'
rep(main, '''    if let Some(header_name) = config.response_store_tenant_header.as_deref() {
        anthropic_app_state''', '''    if let Some(header_name) = config.response_store_tenant_header.as_deref() {
        if header_name.trim().is_empty() || http::header::HeaderName::from_bytes(header_name.trim().as_bytes()).is_err() {
            tracing::error!("Invalid responseStoreTenantHeader; refusing to disable isolation silently");
            std::process::exit(1);
        }
        anthropic_app_state''')

# Collision-resistant aliases occupy a reserved namespace, including case collisions.
cr = 'src/openai/chat_request.rs'
method(cr, 'kiro_tool_name', r'''pub(super) fn kiro_tool_name(name: &str) -> String {
    const PREFIX: &str = "kiro2cc_";
    let safe = !name.is_empty() && name.len() <= MAX_KIRO_TOOL_NAME_CHARS
        && !name.to_ascii_lowercase().starts_with(PREFIX)
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if safe { return name.to_string(); }
    let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
    format!("{PREFIX}{}", &digest[..MAX_KIRO_TOOL_NAME_CHARS - PREFIX.len()])
}''', indent=0)
s = Path(cr).read_text()
s = s.replace('const TOOL_NAME_HASH_CHARS: usize = 12;\n', '')
put(cr, s)

# Update existing tests to the corrected public contract, retaining their coverage.
rt = 'src/openai/responses_response/tests.rs'
s = Path(rt).read_text()
s = s.replace('let first = conv.finish();', 'let first = conv.on_event("message_stop", &json!({}));')
start = s.index('    fn interrupted_stream_closes_open_items()')
end = s.index('    #[test]', start)
chunk = s[start:end].replace('"response.completed",', '"response.failed",')
s = s[:start] + chunk + s[end:]
# Existing non-stream context test must assert failure, not a private enum extension.
start = s.index('    fn context_window_exceeded_has_distinct_nonstream_reason()')
end = s.index('    #[test]', start)
chunk = s[start:end].replace('assert_eq!(out["status"], "incomplete");', 'assert_eq!(out["status"], "failed");')
chunk = chunk.replace('out["incomplete_details"],\n            json!({"reason": "context_window_exceeded"})', 'out["error"]["code"],\n            json!("context_length_exceeded")')
s = s[:start] + chunk + s[end:]
put(rt, s)
method(rt, 'interrupted_custom_tool_stream_yields_partial_json_as_input', r'''    fn interrupted_custom_tool_stream_yields_partial_json_as_input() {
        let mut conv = ResponsesStreamConverter::new("m", custom_names(&["exec"]));
        conv.on_event("content_block_start", &json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"exec"}}));
        conv.on_event("content_block_delta", &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"half"}}));
        let frames = conv.finish();
        assert!(!event_names(&frames).iter().any(|n| n == "response.custom_tool_call_input.done"));
        let (_, last) = parse_frame(frames.last().unwrap());
        assert_eq!(last["response"]["status"], "failed");
        assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    }''')
ct = 'src/openai/chat_response/tests.rs'
method(ct, 'truncated_tool_stream_infers_tool_calls_finish_reason', r'''    fn truncated_tool_stream_infers_tool_calls_finish_reason() {
        let mut conv = ChatStreamConverter::new("m", false);
        conv.on_event("content_block_start", &json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}}));
        let frames = conv.finish();
        assert!(parse_frame(&frames[0]).unwrap().get("error").is_some());
        assert!(!frames.concat().contains("\"finish_reason\":\"tool_calls\""));
        assert_eq!(frames.last().unwrap(), "data: [DONE]\n\n");
    }''')
method(ct, 'truncated_stream_still_gets_finish_and_done', r'''    fn truncated_stream_still_gets_finish_and_done() {
        let frames = run_stream(&[("message_start", json!({}))], false);
        assert_eq!(frames.len(), 3);
        assert!(parse_frame(&frames[1]).unwrap().get("error").is_some());
        assert_eq!(frames[2], "data: [DONE]\n\n");
    }''')
# Usage unit tests test a normally completed stream without message_delta, not EOF.
for path in [ct, rt]:
    s = Path(path).read_text()
    pattern = r'(?ms)^    fn ([A-Za-z0-9_]*usage[A-Za-z0-9_]*)\([^\n]*\).*?^    \}'
    def update_usage_test(m):
        block = m.group(0)
        if '"message_stop"' not in block:
            block = block.replace('frames.extend(conv.finish());', 'frames.extend(conv.on_event("message_stop", &json!({})));')
            block = block.replace('let frames = conv.finish();', 'let frames = conv.on_event("message_stop", &json!({}));')
        return block
    put(path, re.sub(pattern, update_usage_test, s))

append_tests(st, r'''
#[cfg(test)]
mod review_20261001 {
    use super::*;
    fn store() -> Arc<ResponseStore> { Arc::new(ResponseStore::default()) }
    fn seed(s: &Arc<ResponseStore>, scope: &str, output: Value) {
        s.persistence_with_scope(7, Some(scope), vec![json!({"role":"user","content":"first"})], true).unwrap()
            .persist(&json!({"id":"resp_seed","status":"completed","output":[output]}));
    }
    #[test]
    fn references_do_not_repeat_calls_or_messages() {
        for item in [json!({"id":"i","type":"message","role":"assistant","content":"answer"}), json!({"id":"i","type":"function_call","call_id":"c","name":"f","arguments":"{}"}), json!({"id":"i","type":"custom_tool_call","call_id":"c","name":"f","input":"x"})] {
            let s = store(); seed(&s, "a", item);
            let prepared = s.prepare_request_with_scope(7,Some("a"),&json!({"input":[{"type":"item_reference","id":"i"},{"role":"user","content":"next"}],"previous_response_id":"resp_seed"})).unwrap();
            assert_eq!(prepared.body["input"].as_array().unwrap().iter().filter(|v| v["id"] == "i").count(),1);
        }
    }
    #[test]
    fn conflicting_tool_id_is_rejected_but_repeated_text_is_kept() {
        let mut history = vec![json!({"type":"function_call","call_id":"c","name":"f","arguments":"{}"})];
        let same = history[0].clone();
        append_unique_items(&mut history, vec![same]).unwrap(); assert_eq!(history.len(),1);
        assert!(append_unique_items(&mut history,vec![json!({"type":"function_call","call_id":"c","name":"g","arguments":"{}"})]).is_err());
        append_unique_items(&mut history,vec![json!({"role":"user","content":"again"}),json!({"role":"user","content":"again"})]).unwrap();
        assert_eq!(history.len(),3);
    }
    #[test]
    fn compacted_snapshot_replaces_old_history() {
        let s = store();
        let history=vec![json!({"role":"developer","content":"rules"}),json!({"role":"user","content":"large old transcript"})];
        s.persistence(7,history,true).unwrap().persist(&json!({"id":"compact","status":"completed","output":[{"type":"compaction","id":"cmp","encrypted_content":"c3VtbWFyeQ=="}]}));
        let p=s.prepare_request(7,&json!({"previous_response_id":"compact","input":"next"})).unwrap();
        assert!(!p.body.to_string().contains("large old transcript"));
        assert!(p.body.to_string().contains("rules"));
        assert_eq!(p.body["input"].as_array().unwrap().len(),3);
    }
    #[test]
    fn compaction_rejects_unresolved_tools() {
        let s=store(); seed(&s,"a",json!({"type":"function_call","id":"i","call_id":"c","name":"f","arguments":"{}"}));
        assert!(s.prepare_request_with_scope(7,Some("a"),&json!({"previous_response_id":"resp_seed","input":[{"type":"compaction_trigger"}]})).is_err());
        assert!(s.prepare_request_with_scope(7,Some("a"),&json!({"previous_response_id":"resp_seed","input":[{"type":"function_call_output","call_id":"c","output":"done"},{"type":"compaction_trigger"}]})).is_ok());
    }
    #[test]
    fn scoped_history_cannot_fall_back_to_unscoped_namespace() {
        let s=store(); seed(&s,"a",json!({"role":"assistant","content":"answer"}));
        for scope in [None,Some("b")] { assert!(s.prepare_request_with_scope(7,scope,&json!({"previous_response_id":"resp_seed","input":"x"})).is_err()); }
    }
    #[test]
    fn unfinished_and_failed_responses_are_not_saved() {
        let s=store();
        for status in ["in_progress","failed","queued"] {
            s.persistence(7,Vec::new(),true).unwrap().persist(&json!({"id":status,"status":status,"output":[]}));
            assert!(s.prepare_request(7,&json!({"previous_response_id":status,"input":"x"})).is_err());
        }
    }
}
''')
append_tests(rs, r'''
#[cfg(test)]
mod review_20261001 {
    use super::*;
    use std::sync::Arc;
    use crate::model::response_store::ResponseStore;
    fn parsed(frame: &str) -> Value { serde_json::from_str(frame.split_once("\ndata: ").unwrap().1.trim()).unwrap() }
    #[test]
    fn premature_eof_never_persists_and_terminal_is_idempotent() {
        let store=Arc::new(ResponseStore::default());
        let mut c=ResponsesStreamConverter::new("m",HashSet::new()).with_persistence(store.persistence(1,Vec::new(),true).unwrap());
        c.on_event("content_block_start",&json!({"index":0,"content_block":{"type":"text","text":"half"}}));
        let frames=c.finish(); let last=parsed(frames.last().unwrap());
        assert_eq!(last["type"],"response.failed");
        assert!(store.prepare_request(1,&json!({"previous_response_id":last["response"]["id"],"input":"x"})).is_err());
        assert!(c.on_event("content_block_delta",&json!({"delta":{"type":"text_delta","text":"late"}})).is_empty());
        assert!(c.on_event("message_stop",&json!({})).is_empty());
    }
    #[test]
    fn compaction_suppresses_hidden_items_and_keeps_contiguous_sequence() {
        let mut c=ResponsesStreamConverter::new_compaction("m",HashSet::new()); let mut all=Vec::new();
        for (name,data) in [("content_block_start",json!({"index":0,"content_block":{"type":"text","text":"summary"}})),("message_stop",json!({}))] { all.extend(c.on_event(name,&data)); }
        for (i,frame) in all.iter().enumerate() { assert_eq!(parsed(frame)["sequence_number"],json!(i)); }
        assert_eq!(parsed(all.last().unwrap())["response"]["output"][0]["type"],"compaction");
        assert!(!all.concat().contains("response.output_text.done"));
    }
    #[test]
    fn truncated_compaction_cannot_report_completed() {
        let mut c=ResponsesStreamConverter::new_compaction("m",HashSet::new());
        c.on_event("content_block_start",&json!({"index":0,"content_block":{"type":"text","text":"partial"}}));
        c.on_event("message_delta",&json!({"delta":{"stop_reason":"max_tokens"}}));
        let frames=c.on_event("message_stop",&json!({}));
        assert_eq!(parsed(frames.last().unwrap())["type"],"response.failed");
    }
}
''')
append_tests(mw, r'''
#[cfg(test)]
mod review_20261001 {
    use super::*;
    #[test]
    fn duplicate_and_coalesced_tenant_headers_are_rejected() {
        let mut req=Request::builder().body(Body::empty()).unwrap();
        req.headers_mut().append("x-tenant","a".parse().unwrap());
        req.headers_mut().append("x-tenant","b".parse().unwrap());
        assert!(response_store_scope(&req,Some("x-tenant")).is_none());
        req.headers_mut().insert("x-tenant","a,b".parse().unwrap());
        assert!(response_store_scope(&req,Some("x-tenant")).is_none());
    }
}
''')
append_tests(cr, r'''
#[cfg(test)]
mod review_20261001 {
    use super::*;
    #[test]
    fn aliases_cannot_impersonate_real_tool_names_or_case_variants() {
        let original="long/invalid/name";
        let alias=kiro_tool_name(original);
        assert_ne!(kiro_tool_name(&alias),alias);
        assert_ne!(kiro_tool_name("Read"),kiro_tool_name("read"));
        assert_eq!(kiro_tool_name("read"),"read");
        assert!(alias.len()<=64);
    }
}
''')

Path('.review-changed-paths').write_text('\n'.join(sorted(changed))+'\n')
print('Patched', len(changed), 'files:')
print('\n'.join(sorted(changed)))
