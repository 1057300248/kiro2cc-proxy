// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! OpenAI 协议端点 handler
//!
//! 每个 handler 都是"翻译 + 转发"的三段式：
//!
//! 1. OpenAI 请求 JSON → Anthropic 请求 JSON
//! 2. 以普通函数调用方式转交 [`crate::anthropic::handlers::post_messages`]，
//!    完整复用其下游链路（多账号故障转移、RPM 计数、用量追踪、prompt cache、限流处理）
//! 3. 把返回的 Anthropic 响应（JSON 或 SSE 流）转回 OpenAI 格式
//!
//! 第 2 步不改动 `post_messages` 的签名或实现——它是 axum handler，同时也是普通 async fn，
//! extractor 参数在这里手动构造即可。代价是请求体多一次序列化往返，换来对下游核心逻辑的零侵入。

use std::net::SocketAddr;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures::StreamExt;
use serde_json::{Value, json};

use crate::anthropic::handlers::post_messages;
use crate::anthropic::middleware::{ApiKeyContext, AppState};

use super::chat_request;
use super::chat_response::{self, ChatStreamConverter};
use super::error;
use super::responses_request;
use super::responses_response::{self, ResponsesStreamConverter};
use super::sse::{SseItem, SseParser};

/// 两种协议的流式转换器共享的形状，让 SSE 包装逻辑只写一遍
trait StreamConverter {
    fn on_event(&mut self, name: &str, data: &Value) -> Vec<String>;
    fn finish(&mut self) -> Vec<String>;
    fn is_finished(&self) -> bool;
}

impl StreamConverter for ChatStreamConverter {
    fn is_finished(&self) -> bool {
        ChatStreamConverter::is_finished(self)
    }
    fn on_event(&mut self, name: &str, data: &Value) -> Vec<String> {
        ChatStreamConverter::on_event(self, name, data)
    }

    fn finish(&mut self) -> Vec<String> {
        ChatStreamConverter::finish(self)
    }
}

impl StreamConverter for ResponsesStreamConverter {
    fn is_finished(&self) -> bool {
        ResponsesStreamConverter::is_finished(self)
    }
    fn on_event(&mut self, name: &str, data: &Value) -> Vec<String> {
        ResponsesStreamConverter::on_event(self, name, data)
    }

    fn finish(&mut self) -> Vec<String> {
        ResponsesStreamConverter::finish(self)
    }
}

/// `POST /v1/chat/completions` — OpenAI Chat Completions 协议
pub(crate) async fn post_chat_completions(
    State(state): State<AppState>,
    identity: Option<Extension<ApiKeyContext>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let incoming = match parse_json_body(&body) {
        Ok(v) => v,
        Err(response) => return *response,
    };

    let converted = match chat_request::convert(&incoming) {
        Ok(c) => c,
        Err(msg) => {
            return error::error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                msg,
                None,
            );
        }
    };

    tracing::info!(
        model = %converted.client_model,
        stream = converted.stream,
        "Received POST /v1/chat/completions request"
    );

    let client_model = converted.client_model;
    let include_usage = converted.include_usage;
    let tool_name_map = converted.tool_name_map;
    let stream_tool_name_map = tool_name_map.clone();
    let converter_model = client_model.clone();
    forward_and_wrap(
        state,
        identity,
        addr,
        headers,
        converted.anthropic_body,
        converted.stream,
        client_model,
        move || {
            Box::new(ChatStreamConverter::with_tool_name_map(
                &converter_model,
                include_usage,
                stream_tool_name_map,
            ))
        },
        move |anthropic, model| {
            chat_response::convert_non_stream_with_tool_name_map(anthropic, model, &tool_name_map)
        },
    )
    .await
}

/// `POST /v1/responses` — OpenAI Responses 协议（Codex CLI 默认 `wire_api`）
pub(crate) async fn post_responses(
    State(state): State<AppState>,
    identity: Option<Extension<ApiKeyContext>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let incoming = match parse_json_body(&body) {
        Ok(v) => v,
        Err(response) => return *response,
    };

    let Some(owner_api_key_id) = identity.as_ref().map(|context| context.0.id) else {
        return error::error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_request_error",
            "Invalid API key.",
            Some("invalid_api_key"),
        );
    };
    let owner_scope = identity
        .as_ref()
        .and_then(|context| context.0.response_store_scope.as_deref());
    if state.response_store_client_auth.is_some() {
        if let Err(message) =
            crate::model::client_auth_scope::check_request_scope(&incoming, owner_scope)
        {
            return error::error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                message,
                Some("client_identity_required"),
            );
        }
    } else if state.response_store_tenant_header.is_some() && owner_scope.is_none() {
        return error::error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Configured Responses tenant header is missing or invalid.",
            None,
        );
    }
    let prepared = match state.response_store.prepare_request_with_scope(
        owner_api_key_id,
        owner_scope,
        &incoming,
    ) {
        Ok(prepared) => prepared,
        Err(msg) => {
            return error::error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                msg,
                None,
            );
        }
    };

    let converted = match responses_request::convert(&prepared.body) {
        Ok(c) => c,
        Err(msg) => {
            // 请求转换失败时不触达上游。
            return error::error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                msg,
                None,
            );
        }
    };

    tracing::info!(
        model = %converted.client_model,
        stream = converted.stream,
        continued = prepared.continued,
        store = prepared.store_response,
        "Received POST /v1/responses request"
    );

    let client_model = converted.client_model;
    let converter_model = client_model.clone();
    // custom 工具名要同时供流式与非流式转换使用，各持一份（集合很小，克隆成本可忽略）
    let custom_tools = converted.custom_tools;
    let stream_custom_tools = custom_tools.clone();
    let tool_name_map = converted.tool_name_map;
    let stream_tool_name_map = tool_name_map.clone();
    let is_compaction = converted.is_compaction;
    let persistence = state.response_store.persistence_with_scope(
        owner_api_key_id,
        owner_scope,
        prepared.history,
        prepared.store_response,
    );
    let stream_persistence = persistence.clone();
    let nonstream_persistence = persistence;
    forward_and_wrap(
        state,
        identity,
        addr,
        headers,
        converted.anthropic_body,
        converted.stream,
        client_model,
        move || {
            let converter = if is_compaction {
                ResponsesStreamConverter::new_compaction_with_tool_name_map(
                    &converter_model,
                    stream_custom_tools,
                    stream_tool_name_map,
                )
            } else {
                ResponsesStreamConverter::new_with_tool_name_map(
                    &converter_model,
                    stream_custom_tools,
                    stream_tool_name_map,
                )
            };
            let converter = match stream_persistence {
                Some(persistence) => converter.with_persistence(persistence),
                None => converter,
            };
            Box::new(converter)
        },
        move |anthropic, model| {
            let response = if is_compaction {
                responses_response::convert_non_stream_compaction_with_tool_name_map(
                    anthropic,
                    model,
                    &custom_tools,
                    &tool_name_map,
                )
            } else {
                responses_response::convert_non_stream_with_tool_name_map(
                    anthropic,
                    model,
                    &custom_tools,
                    &tool_name_map,
                )
            };
            if let Some(persistence) = &nonstream_persistence {
                persistence.persist(&response);
            }
            response
        },
    )
    .await
}

/// 解析请求体 JSON；失败时给出可直接下发的 400 响应
///
/// Err 变体装箱是为躲开 clippy 的 `result_large_err`：`Response` 体积远大于 `Value`，
/// 直接塞进 `Result` 会让成功路径也背上错误变体的栈开销。
fn parse_json_body(body: &Bytes) -> Result<Value, Box<Response>> {
    serde_json::from_slice(body).map_err(|e| {
        Box::new(error::error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Request body is not valid JSON: {}", e),
            None,
        ))
    })
}

/// 转发到下游并按 `stream` 把响应包成对应的 OpenAI 形态
///
/// 两个端点的差异只在 `make_converter` 与 `convert_non_stream` 两个转换入口，
/// 状态码透传、SSE 判定、非 JSON 响应体兜底完全共用。
#[allow(clippy::too_many_arguments)]
async fn forward_and_wrap(
    state: AppState,
    identity: Option<Extension<ApiKeyContext>>,
    addr: SocketAddr,
    headers: HeaderMap,
    anthropic_body: Value,
    stream: bool,
    client_model: String,
    make_converter: impl FnOnce() -> Box<dyn StreamConverter + Send>,
    convert_non_stream: impl FnOnce(&Value, &str) -> Value,
) -> Response {
    let upstream = forward_to_anthropic(state, identity, addr, headers, &anthropic_body).await;

    let (parts, upstream_body) = upstream.into_parts();
    let status = parts.status;

    if !status.is_success() {
        let bytes = read_body(upstream_body).await;
        return (status, Json(error::convert_error_body(status, &bytes))).into_response();
    }

    if stream {
        if is_event_stream(&parts.headers) {
            return stream_openai_response(upstream_body, make_converter());
        }
        // 请求了流式却拿到非 SSE 响应：不静默丢弃，降级为一次性 JSON 并留痕
        tracing::warn!("客户端请求流式但下游返回非 SSE 响应，已降级为非流式返回");
    }

    let bytes = read_body(upstream_body).await;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => (status, Json(convert_non_stream(&v, &client_model))).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "下游返回的响应体不是合法 JSON");
            error::error_response(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "Upstream returned a malformed response body.",
                None,
            )
        }
    }
}

/// 把 Anthropic 请求体交给下游 handler
fn forward_to_anthropic(
    state: AppState,
    identity: Option<Extension<ApiKeyContext>>,
    addr: SocketAddr,
    headers: HeaderMap,
    anthropic_body: &Value,
) -> impl std::future::Future<Output = Response> + use<> {
    // Value 一定可序列化，unwrap_or_default 只为避免 panic 路径
    let bytes = Bytes::from(serde_json::to_vec(anthropic_body).unwrap_or_default());
    post_messages(State(state), identity, ConnectInfo(addr), headers, bytes)
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

/// 非流式响应体读取上限
///
/// 只有非流式响应与错误响应体走 [`read_body`]（SSE 走流式转发，不经此处），这两类的正常体积
/// 都在几百 KB 以内。给到 64 MiB 是宽松兜底：既不会误伤超长输出，又能在下游异常返回巨大响应体
/// 时避免整体读进内存。
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024 * 1024;

/// 读取响应体全部字节；读取失败或超出 [`MAX_RESPONSE_BODY_BYTES`] 时返回空切片
///
/// 返回空切片后调用方走各自的兜底路径（错误体用固定文案、成功体按 JSON 解析失败转 502），
/// 不会 panic。
async fn read_body(body: Body) -> Bytes {
    match axum::body::to_bytes(body, MAX_RESPONSE_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                limit = MAX_RESPONSE_BODY_BYTES,
                "读取下游响应体失败（可能超出上限）"
            );
            Bytes::new()
        }
    }
}

/// 把 Anthropic SSE 流包装为 OpenAI SSE 流（具体帧格式由 `converter` 决定）
fn stream_openai_response(body: Body, converter: Box<dyn StreamConverter + Send>) -> Response {
    let parser = SseParser::new();
    let data_stream = body.into_data_stream();

    // 状态用 Option 包裹：返回 None 即终止流
    let out = futures::stream::unfold(Some((data_stream, parser, converter)), |st| async move {
        let (mut ds, mut parser, mut converter) = st?;
        loop {
            match ds.next().await {
                Some(Ok(chunk)) => {
                    let mut buf = String::new();
                    for item in parser.push(&chunk) {
                        if let SseItem::Event { name, data } = item {
                            for frame in converter.on_event(&name, &data) {
                                buf.push_str(&frame);
                            }
                            if converter.is_finished() {
                                break;
                            }
                        }
                        // SseItem::Done：Anthropic 侧不发，收到也无需转发
                    }
                    if buf.is_empty() {
                        // 本次 chunk 只含半帧或 ping，继续等下一块
                        continue;
                    }
                    let next = if converter.is_finished() {
                        None
                    } else {
                        Some((ds, parser, converter))
                    };
                    return Some((
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(buf)),
                        next,
                    ));
                }
                Some(Err(e)) => {
                    // 传输层中断：下发错误帧收尾，不能伪装成正常结束
                    tracing::warn!(error = %e, "读取下游 SSE 流中断，已下发错误帧终止");
                    let mut buf = String::new();
                    for frame in converter.on_event(
                            "error",
                            &json!({
                                "type": "error",
                                "error": {
                                    "type": "overloaded_error",
                                    "message": "Upstream connection was interrupted before the response finished. Please retry."
                                }
                            }),
                        ) {
                            buf.push_str(&frame);
                        }
                    return Some((Ok(Bytes::from(buf)), None));
                }
                None => {
                    let mut buf = String::new();
                    for item in parser.finish() {
                        if let SseItem::Event { name, data } = item {
                            for frame in converter.on_event(&name, &data) {
                                buf.push_str(&frame);
                            }
                        }
                    }
                    for frame in converter.finish() {
                        buf.push_str(&frame);
                    }
                    if buf.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(buf)), None));
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(out))
        .expect("构造 SSE 响应不会失败")
}

#[cfg(test)]
mod transport_review_20261001 {
    use super::*;
    use crate::model::response_store::ResponseStore;
    use std::collections::HashSet;
    use std::sync::Arc;
    fn event(name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }
    async fn consume(
        chunks: Vec<Result<Bytes, std::io::Error>>,
        converter: ResponsesStreamConverter,
    ) -> String {
        let response = stream_openai_response(
            Body::from_stream(futures::stream::iter(chunks)),
            Box::new(converter),
        );
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn malformed_frame_cannot_be_hidden_by_later_success() {
        let data = event("message_start", json!({}))
            + "event: content_block_delta\ndata: {broken}\n\n"
            + &event("message_stop", json!({}));
        let text = consume(
            vec![Ok(Bytes::from(data))],
            ResponsesStreamConverter::new("m", HashSet::new()),
        )
        .await;
        assert!(text.contains("response.failed"));
        assert!(!text.contains("response.completed"));
    }
    #[tokio::test]
    async fn partial_eof_and_transport_error_never_persist() {
        for transport_error in [false, true] {
            let store = Arc::new(ResponseStore::default());
            let converter = ResponsesStreamConverter::new("m", HashSet::new())
                .with_persistence(store.persistence(1, Vec::new(), true).unwrap());
            let data = event(
                "content_block_start",
                json!({"index":0,"content_block":{"type":"tool_use","id":"c","name":"f"}}),
            ) + &event(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
            );
            let mut chunks = vec![Ok(Bytes::from(data))];
            if transport_error {
                chunks.push(Err(std::io::Error::other("fixture interrupted")));
            }
            let text = consume(chunks, converter).await;
            assert!(text.contains("response.failed"));
            assert!(!text.contains("response.completed"));
            assert!(!text.contains("response.function_call_arguments.done"));
            let failed: Value = text
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["type"] == "response.failed")
                .unwrap();
            assert!(
                store
                    .prepare_request(
                        1,
                        &json!({"previous_response_id":failed["response"]["id"],"input":"next"})
                    )
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn split_success_finishes_once_and_drops_late_content() {
        let data = event(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":"answer"}}),
        ) + &event("message_stop", json!({}))
            + &event(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"text_delta","text":"MUST_NOT_APPEAR"}}),
            );
        let chunks = data
            .as_bytes()
            .chunks(7)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let text = consume(chunks, ResponsesStreamConverter::new("m", HashSet::new())).await;
        assert_eq!(text.matches("event: response.completed\n").count(), 1);
        assert!(!text.contains("MUST_NOT_APPEAR"));
    }
}
