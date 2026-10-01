#!/usr/bin/env python3
"""One-shot final review pass. Exact source checks; no production or live credentials."""
from pathlib import Path
import re
changed=set()
def put(path,text):
    p=Path(path); p.parent.mkdir(parents=True,exist_ok=True)
    if not p.exists() or p.read_text()!=text:
        p.write_text(text); changed.add(path)
def rep(path,old,new,count=1):
    s=Path(path).read_text()
    if old not in s and new in s: return
    assert s.count(old)==count,(path,old[:100],s.count(old))
    put(path,s.replace(old,new))
def method(path,name,new):
    s=Path(path).read_text(); m=re.search(r'(?m)^    fn '+re.escape(name)+r'\(',s); assert m,name
    e=re.search(r'(?m)^    \}',s[m.end():]); assert e
    put(path,s[:m.start()]+new.rstrip()+s[m.end()+e.end():])

# Keep protocol syntax errors visible: a later message_stop cannot legitimize lost data.
p='src/openai/sse.rs'
rep(p,'use serde_json::Value;','use serde_json::{Value, json};')
rep(p,'fn parse_block(block: &[u8]) -> Option<SseItem> {','''fn malformed_sse(message: &str) -> SseItem {
    SseItem::Event { name: "error".to_string(), data: json!({"error":{"type":"api_error","message":message}}) }
}

fn parse_block(block: &[u8]) -> Option<SseItem> {
    if block.len() > MAX_BUFFERED_BYTES { return Some(malformed_sse("Upstream SSE frame exceeded the safety limit.")); }''')
rep(p,'''            tracing::warn!(error = %e, "SSE 帧不是合法 UTF-8，已跳过");
            return None;''','''            tracing::warn!(error = %e, "SSE 帧不是合法 UTF-8，终止响应");
            return Some(malformed_sse("Upstream SSE frame is not valid UTF-8."));''')
rep(p,'''            tracing::warn!(event = %event_name, error = %e, "SSE data 不是合法 JSON，已跳过");
            None''','''            tracing::warn!(event = %event_name, error = %e, "SSE data 不是合法 JSON，终止响应");
            Some(malformed_sse("Upstream SSE data is not valid JSON."))''')
rep(p,'            self.buf.clear();','''            self.buf.clear();
            items.push(malformed_sse("Upstream SSE buffer exceeded the safety limit."));''')
rep(p,'''            tracing::warn!(event = %event_name, "SSE 帧缺少 data 字段，已跳过");''','''            tracing::warn!(event = %event_name, "SSE 帧缺少 data 字段，终止响应");
            return Some(malformed_sse("Upstream SSE event is missing its data field."));''')
method(p,'skips_malformed_json_without_breaking_stream',r'''    fn skips_malformed_json_without_breaking_stream() {
        let mut p=SseParser::new();
        let items=p.push(b"event: bad\ndata: {not json}\n\nevent: ok\ndata: {\"type\":\"ok\"}\n\n");
        assert!(matches!(&items[0], SseItem::Event { name, .. } if name == "error"));
        assert_eq!(items[1],ev("ok",json!({"type":"ok"})));
    }''')
rep(p,'            assert!(p.push(&chunk).is_empty());','''            let items=p.push(&chunk);
            assert!(items.iter().all(|item| matches!(item,SseItem::Event { name, .. } if name == "error")));''')
method(p,'oversized_tail_after_valid_frame_is_dropped',r'''    fn oversized_tail_after_valid_frame_is_dropped() {
        let mut p=SseParser::new();
        let mut chunk=b"event: a\ndata: {\"type\":\"a\"}\n\n".to_vec();
        chunk.extend(std::iter::repeat_n(b'x',MAX_BUFFERED_BYTES+1));
        let items=p.push(&chunk);
        assert_eq!(items[0],ev("a",json!({"type":"a"})));
        assert!(matches!(&items[1],SseItem::Event { name,.. } if name == "error"));
        assert!(p.buf.is_empty());
    }''')

# Kiro AWS binary stream has legitimate clean EOF. A residual partial frame or
# decoder data loss is not clean EOF, and must not start a bridge follow-up.
p='src/anthropic/handlers/stream.rs'
rep(p,'''                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();''','''                            let mut decode_failed = decoder.feed(&chunk).is_err();
                            let mut events = Vec::new();''')
rep(p,'''                                            events.extend(bridge_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }''','''                                            events.extend(bridge_events);
                                        } else {
                                            decode_failed = true;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                        decode_failed = true;
                                    }''')
rep(p,'''                            // 转换为 SSE 字节流
                            let bytes''','''                            decode_failed |= binary_stream_is_invalid(&decoder, false);
                            if decode_failed {
                                events.push(stream_interrupted_error_event());
                            }
                            // 转换为 SSE 字节流
                            let bytes''')
rep(p,'Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, deadline, bridge, bridge_ctx, provider, round_in_flight)))\n                        }\n                        Some(Err(e)) => {','Some((stream::iter(bytes), (body_stream, ctx, decoder, decode_failed, ping_interval, deadline, bridge, bridge_ctx, provider, round_in_flight)))\n                        }\n                        Some(Err(e)) => {')
rep(p,'''                        None => {
                            // 桥接态''','''                        None => {
                            if binary_stream_is_invalid(&decoder, true) {
                                let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(Bytes::from(stream_interrupted_error_event().to_sse_string()))];
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, deadline, bridge, bridge_ctx, provider, round_in_flight)));
                            }
                            // 桥接态''')
rep(p,'/// 创建 SSE 事件流','''fn binary_stream_is_invalid(decoder: &EventStreamDecoder, eof: bool) -> bool {
    decoder.is_stopped() || decoder.bytes_skipped() > 0 || (eof && decoder.buffer_len() > 0)
}

/// 创建 SSE 事件流''')
s=Path(p).read_text(); put(p,s+r'''

#[cfg(test)]
mod transport_review_20261001 {
    use super::*;
    #[test]
    fn binary_partial_frame_is_only_fatal_at_eof() {
        let mut d=EventStreamDecoder::new();
        assert!(!binary_stream_is_invalid(&d,true));
        d.feed(&[0,0]).unwrap();
        assert!(!binary_stream_is_invalid(&d,false));
        assert!(binary_stream_is_invalid(&d,true));
    }
}
''')

# Exercise the actual Body -> SSE -> converter -> Body wrapper (not only helpers).
p='src/openai/handlers.rs'; s=Path(p).read_text(); put(p,s+r'''

#[cfg(test)]
mod transport_review_20261001 {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use crate::model::response_store::ResponseStore;
    fn event(name:&str,data:Value)->String { format!("event: {name}\ndata: {data}\n\n") }
    async fn consume(chunks: Vec<Result<Bytes, std::io::Error>>, converter: ResponsesStreamConverter)->String {
        let response=stream_openai_response(Body::from_stream(futures::stream::iter(chunks)),Box::new(converter));
        String::from_utf8(axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap().to_vec()).unwrap()
    }
    #[tokio::test]
    async fn malformed_frame_cannot_be_hidden_by_later_success() {
        let data=event("message_start",json!({}))+"event: content_block_delta\ndata: {broken}\n\n"+&event("message_stop",json!({}));
        let text=consume(vec![Ok(Bytes::from(data))],ResponsesStreamConverter::new("m",HashSet::new())).await;
        assert!(text.contains("response.failed")); assert!(!text.contains("response.completed"));
    }
    #[tokio::test]
    async fn partial_eof_and_transport_error_never_persist() {
        for transport_error in [false,true] {
            let store=Arc::new(ResponseStore::default());
            let converter=ResponsesStreamConverter::new("m",HashSet::new()).with_persistence(store.persistence(1,Vec::new(),true).unwrap());
            let data=event("content_block_start",json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}}))+&event("content_block_delta",json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}));
            let mut chunks=vec![Ok(Bytes::from(data))];
            if transport_error { chunks.push(Err(std::io::Error::other("fixture interrupted"))); }
            let text=consume(chunks,converter).await;
            assert!(text.contains("response.failed")); assert!(!text.contains("response.completed"));
            assert!(!text.contains("response.function_call_arguments.done"));
            let failed:Value=text.lines().filter_map(|l|l.strip_prefix("data: ")).filter_map(|l|serde_json::from_str::<Value>(l).ok()).find(|v|v["type"]=="response.failed").unwrap();
            assert!(store.prepare_request(1,&json!({"previous_response_id":failed["response"]["id"],"input":"next"})).is_err());
        }
    }
    #[tokio::test]
    async fn split_success_finishes_once_and_drops_late_content() {
        let data=event("content_block_start",json!({"index":0,"content_block":{"type":"text","text":"answer"}}))+&event("message_stop",json!({}))+&event("content_block_delta",json!({"index":0,"delta":{"type":"text_delta","text":"MUST_NOT_APPEAR"}}));
        let chunks=data.as_bytes().chunks(7).map(|c|Ok(Bytes::copy_from_slice(c))).collect();
        let text=consume(chunks,ResponsesStreamConverter::new("m",HashSet::new())).await;
        assert_eq!(text.matches("event: response.completed\n").count(),1);
        assert!(!text.contains("MUST_NOT_APPEAR"));
    }
}
''')

# Replace the unsafe deployment recipe rather than merely appending a warning.
sections={
 'README.md':('### 通过 new-api 反代并共享上游 Key','### 已知限制',r'''### 通过 new-api 反代并共享上游 Key

Kiro 配置写在本服务的 `config.json`（或环境变量 `RESPONSE_STORE_TENANT_HEADER`），不是 new-api 的请求体覆盖：

```json
{"responseStoreTenantHeader":"X-Kiro2CC-Tenant"}
```

使用配套的 new-api 已认证租户补丁后，渠道 **Header Override** 设置为：

```json
{"X-Kiro2CC-Tenant":"{authenticated_tenant}"}
```

该占位符由 new-api 鉴权成功后的用户 ID / Token ID 派生不透明 HMAC 标识；不是原始请求头。不同 token 互相隔离，同一个已认证 token 的不同请求头格式保持同一范围。渠道密钥轮换会改变范围，需新建会话或回传完整历史。不要以 `{client_header:Authorization}`、`{client_header:x-api-key}` 或任意客户端租户头充当所有者边界。

配套变更位于 `1057300248/wanchuan-new-api` 的 `codex/kiro-authenticated-tenant-20261001` 分支；未合入该补丁的 new-api 不能把这个占位符作为字面量转发。正常上游 Authorization 仍使用 Kiro 渠道密钥。渠道自检使用单独命名空间。

Base URL 使用实际可达的 Kiro 服务地址与监听端口，不追加 `/v1`；`5678` 只是历史示例，不是程序默认端口。保持 `disable_store` 关闭，要续接的轮次使用 `store=true`。Kiro 必须仅对可信网关/私网开放；跨主机使用受保护传输。租户头只是状态命名空间，不替代认证，也不是逐请求签名。

启用后缺失、空值、重复、逗号合并或过长租户头返回 400。租户头只隔离 Responses 历史，Kiro 内部共享 API Key 的 RPM/额度/用量仍聚合，终端计费和限流继续由 new-api 承担。

续接引用不重复插入上一轮 output；异常 EOF、坏 SSE/二进制帧、未完成工具 JSON 都失败且不保存。上下文超限统一使用 `failed` 与 `context_length_exceeded`。压缩只在完整摘要成功后替换旧历史，有未完成工具时拒绝压缩。进程重启/多实例仍不共享内存状态，需同实例粘滞或回传完整历史。

来源取舍与回归清单见 [集成审计](docs/stateful-integration-audit.md)。

'''),
 'README.en.md':('### Reverse proxy through new-api with a shared upstream key','### Known Limitations',r'''### Reverse proxy through new-api with a shared upstream key

Set this in **Kiro's config.json**, not the New API request body override:

```json
{"responseStoreTenantHeader":"X-Kiro2CC-Tenant"}
```

The equivalent Kiro environment variable is `RESPONSE_STORE_TENANT_HEADER=X-Kiro2CC-Tenant`. With the companion authenticated-tenant patch installed in New API, set its channel **Header Override** to:

```json
{"X-Kiro2CC-Tenant":"{authenticated_tenant}"}
```

The new placeholder derives an opaque HMAC scope from the server-authenticated user/token IDs, never from raw client headers. Different API tokens are isolated; alternate authentication header formats for the same verified token do not change its scope. Channel credential rotation changes the scope. Keep upstream Authorization set to the Kiro channel credential. Channel self-tests have a separate scope.

The companion patch is on `1057300248/wanchuan-new-api`, branch `codex/kiro-authenticated-tenant-20261001`. An unpatched gateway must not forward this placeholder literally. Do not use `{client_header:Authorization}`, `{client_header:x-api-key}`, or arbitrary client tenant headers as an ownership boundary.

Use the actual reachable listen address/port as the channel Base URL, without `/v1`; `5678` is only an example, not the application default. Keep `disable_store` off and use `store=true` for responses that need continuation. Only expose Kiro to trusted gateways/private networks; use protected transport across hosts. The tenant header is a namespace, not independent authentication or a request signature.

Missing, empty, duplicate, coalesced, or oversized tenant headers fail closed. Kiro usage/RPM/quota accounting remains aggregated under the shared upstream key; New API owns end-user billing and limits.

References never duplicate automatically restored output. Premature EOF, malformed SSE/binary frames, or incomplete tool JSON fail without saving a successful continuation. Context exhaustion uses `failed`/`context_length_exceeded`. Only a complete successful compaction replaces old history; unresolved tools block compaction. State remains process-local: restarts lose it and multiple instances require affinity or full-history replay.

See [integration audit](docs/stateful-integration-audit.md) for donor decisions and regression boundaries.

''')}
for path,(start,end,body) in sections.items():
    s=Path(path).read_text(); a=s.index(start); b=s.index(end,a); put(path,s[:a]+body+s[b:])
for path in ['openspec/specs/openai-compat/spec.md','docs/代码速查表.md']:
    s=Path(path).read_text()
    s=s.replace('非流式响应返回 `incomplete_details.reason = context_window_exceeded`；流式以 `response.failed` 收尾，`response.error.code = context_length_exceeded`','非流式响应返回 `status = failed` 及 `error.code = context_length_exceeded`；流式以 `response.failed` 收尾，`response.error.code = context_length_exceeded`')
    s=s.replace('上下文耗尽 → `context_window_exceeded`','上下文耗尽 → `status=failed` / `error.code=context_length_exceeded`')
    s=s.replace('`context_window_exceeded`','`context_length_exceeded`')
    s=s.replace('{client_header:Authorization}','{authenticated_tenant}')
    put(path,s)
put('docs/stateful-integration-audit.md',r'''# Stateful Responses integration audit — 2026-10-01

## Scope and base

The draft integration remains based on TsinHzl `c6c72b30b7cf72c63ec56cc6f246837af623a264`. This is a selective feature port, not a merge of every donor fork or a claim that the base is the latest upstream. No master/main merge, release deployment, billing migration, or live-account testing is performed by this review.

## Donor decisions

| Donor / capability | Result in this branch | Evidence / decision |
|---|---|---|
| TsinHzl endpoint buckets, sticky session, retry/concurrency, cache/account pool | Retained | Existing `src/kiro/provider/`, `src/kiro/token_manager/`, Anthropic forwarding path is retained; OpenAI adapters still reuse `post_messages` |
| amaranth777 previous_response_id / store / item_reference | Adapted, not copied wholesale | `src/model/response_store.rs` and `src/openai/handlers.rs`; authenticated owner+scope, bounded in-process store, failure exclusion, reference de-duplication |
| amaranth777 custom tools / default namespace / Codex tools | Retained from modern base plus aliases | `responses_request/tools.rs`, `items.rs`, response converters; the old monolithic donor response implementation is not overlaid |
| byteawake response.failed and context exhaustion | Adapted and corrected | `responses_response/stream.rs`, `nonstream.rs`; both protocols now report actual failures instead of successful completion or a private incomplete-reason enum |
| byteawake Chat stop-reason / usage improvements (`336c5048909b2e5555d00afc46aebcaeb1ebe4fa`) | Selectively adapted | Usage baseline and completed-tool inference remain; unlike the donor's permissive EOF fallback, EOF does not authorize a half-finished tool call |
| byteawake failure terminal (`bd3cd9951a620ce6126ce645f3c167ab2ed76068`) | Adapted | Typed failed terminal, no successful persistence after failure |
| HapticTide long tool alias (`196c16ec5c4cc33db5b71f9c3f342640a41afcbc`) | Reimplemented for both Chat and Responses | Invalid/long names, history and response restoration; reserved alias namespace/case collision handling |
| byteawake long-output/thinking changes | Not blindly copied | Current base `anthropic/converter/fields.rs` already has named output caps and model-specific reasoning/effort handling. Preserve its later latency choices; copying old defaults could regress first-token latency. No live-model performance claim is made |
| byteawake AWS SSO device enrollment UI/client | Not included | Donor `src/kiro/sso_oidc.rs` exists; target `src/kiro/mod.rs` has no such enrollment module. Existing target `token_manager/refresh.rs` supports IdC/Builder-ID/IAM refresh of supplied credentials. New interactive enrollment is separate account-management functionality, not a prerequisite for gateway continuation |

Reference repositories: https://github.com/TsinHzl/kiro2cc-proxy , https://github.com/amaranth777/kiro2cc-proxy , https://github.com/byteawake/kiro2cc-proxy , https://github.com/HapticTide/kiro2cc-proxy . Retain original MIT notices and authorship; adapted behavior is not represented as an upstream merge.

## Review fixes and additional hardening

1. Trusted tenant identity is generated by the companion New API change from authenticated user/token context using `{authenticated_tenant}`. Raw client auth headers are not identities. The Kiro receiver rejects ambiguous tenant headers.
2. Already restored output references do not append a second copy. Conflicting repeated item/call IDs fail; repeated ordinary user text remains valid.
3. Normal message_stop is distinct from transport EOF. Failed/unfinished tools remain incomplete and are not emitted as executable `.done` calls. Late events after terminal are ignored.
4. Compaction replaces superseded history only after a complete usable summary; explicit system/developer history remains. Pending tools prevent compaction.
5. Non-stream context exhaustion uses a failed response and the same error code as streaming.
6. Arc snapshots avoid deep-copying histories while holding the store lock. TTL, entry count and serialized-byte budgets remain bounded; they are not exact measurements of process RSS.
7. Malformed SSE and broken/residual AWS binary frames terminate instead of being silently skipped into a successful response. The binary protocol's legitimate clean EOF remains supported.
8. Regression tests exercise the actual Body/SSE wrapper for malformed input, split chunks, early EOF, transport errors, late events, and successful completion.

## Validation and remaining boundaries

The final head is validated by the read-only Stateful Responses Review workflow (format, locked all-target check, Clippy, tests, release build). Workflow logs are the source of truth for exact counts; no fixed count is embedded here. Existing upstream Clippy warnings are not represented as zero warnings.

No live upstream credentials were used. Full deployed New API → Kiro → actual account/model smoke testing, route affinity and network ACL verification are still operator checks. No claim of universal protocol/model compatibility is made.

The continuation store is still in-memory, not durable/shared; restarting or routing a chain to another process loses it. It is reasonable to add an independently tested Redis/SQLite backend later rather than treating durability as already implemented. Scope is per authenticated downstream API token, not automatically per human/user across different tokens. Shared-key usage and limits remain New API's responsibility. The raw compatibility endpoints do not promise every native OpenAI feature (for example encrypted reasoning or arbitrary tool_choice). These are explicit boundaries, not silently enabled features.
''')
Path('.review-changed-paths').write_text('\n'.join(sorted(changed))+'\n')
print('Final review patched:', '\n'.join(sorted(changed)))
