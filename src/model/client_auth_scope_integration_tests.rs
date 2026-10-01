// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Loopback-only fixtures: real Kiro authentication + store; no New API database,
//! external identity service, upstream model/account, or real client credential.

use super::*;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use axum::{Router, Extension, Json, extract::State, http::StatusCode, middleware, routing::post};
use serde_json::json;
use crate::anthropic::middleware::{ApiKeyContext, AppState, auth_middleware};
use crate::model::api_key::{ApiKey, ApiKeyManager};

const A: &str = "AbCdEf0123456789AbCdEf0123456789AbCdEf0123456789";
const B: &str = "BbCdEf0123456789AbCdEf0123456789AbCdEf0123456789";

struct Fixture {
    url: String,
    gateway_key: String,
    task: tokio::task::JoinHandle<()>,
    directory: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn fixture_response(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiKeyContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert!(!headers.contains_key(DEFAULT_CLIENT_AUTH_HEADER));
    let scope = identity.response_store_scope.as_deref();
    if let Err(message) = check_request_scope(&body, scope) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":message}})));
    }
    let prepared = match state.response_store.prepare_request_with_scope(identity.id, scope, &body) {
        Ok(prepared) => prepared,
        Err(message) => return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":message}}))),
    };
    let response = json!({
        "id":format!("resp_{}",uuid::Uuid::new_v4().simple()),
        "status":"completed",
        "output":[{"type":"message","role":"assistant","content":"fixture-answer"}],
        "replayed_input":prepared.body["input"]
    });
    if let Some(persistence) = state.response_store.persistence_with_scope(identity.id, scope, prepared.history, prepared.store_response) {
        persistence.persist(&response);
    }
    (StatusCode::OK, Json(response))
}

async fn fixture(mode_enabled: bool) -> Fixture {
    let directory = std::env::temp_dir().join(format!("kiro-client-scope-{}",uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let key = ApiKey::new(7,"gateway-fixture".to_string(),None,None,"usd".to_string(),None,None);
    let gateway_key = key.key.clone();
    let key_path = directory.join("api_keys.json");
    std::fs::write(&key_path,serde_json::to_vec(&vec![key]).unwrap()).unwrap();
    let mut state = AppState::new().with_api_key_manager(Arc::new(ApiKeyManager::load(&key_path).unwrap()));
    if mode_enabled {
        state.response_store_client_auth = Some(Arc::new(ClientAuthScope::new(DEFAULT_CLIENT_AUTH_HEADER,None,&"42".repeat(32)).unwrap()));
    }
    let app = Router::new()
        .route("/v1/responses",post(fixture_response))
        .route("/real/responses",post(crate::openai::post_responses))
        .layer(middleware::from_fn_with_state(state.clone(),auth_middleware))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    Fixture { url:format!("http://{address}"),gateway_key,task,directory }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap()
}
fn request(client: &reqwest::Client, f: &Fixture, credential: Option<&str>, body: Value) -> reqwest::RequestBuilder {
    let request = client.post(format!("{}/v1/responses",f.url)).bearer_auth(&f.gateway_key).json(&body);
    match credential {
        Some(value) => request.header(DEFAULT_CLIENT_AUTH_HEADER,value),
        None => request,
    }
}

#[tokio::test]
async fn shared_gateway_roundtrip_isolated_and_canonicalized() {
    let f = fixture(true).await; let client = client();
    let first = request(&client,&f,Some(&format!("Bearer sk-{A}")),json!({"input":"private-A","store":true})).send().await.unwrap();
    assert_eq!(first.status(),StatusCode::OK);
    let first: Value = first.json().await.unwrap();
    let id = first["id"].as_str().unwrap();
    let second = request(&client,&f,Some(A),json!({"input":"next","previous_response_id":id,"store":false})).send().await.unwrap();
    assert_eq!(second.status(),StatusCode::OK);
    let second: Value = second.json().await.unwrap();
    assert!(second["replayed_input"].to_string().contains("private-A"));
    assert!(!second.to_string().contains(A));
    let other = request(&client,&f,Some(&format!("Bearer sk-{B}")),json!({"input":"x","previous_response_id":id,"user":A,"metadata":{"tenant":A}}))
        .header("x-kiro2cc-tenant",A).send().await.unwrap();
    assert_eq!(other.status(),StatusCode::BAD_REQUEST);
    let text = other.text().await.unwrap();
    assert!(!text.contains("private-A")); assert!(!text.contains(A)); assert!(!text.contains(B));
    // store=false completion itself cannot subsequently be resumed.
    let not_saved = request(&client,&f,Some(A),json!({"input":"x","previous_response_id":second["id"]})).send().await.unwrap();
    assert_eq!(not_saved.status(),StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_identity_never_stores_or_reads_even_with_store_false() {
    let f = fixture(true).await; let client = client();
    let plain = request(&client,&f,None,json!({"input":"probe","store":false})).send().await.unwrap();
    assert_eq!(plain.status(),StatusCode::OK);
    for body in [json!({"input":"x"}),json!({"input":"x","store":true}),json!({"input":"x","store":false,"previous_response_id":"resp_other"}),json!({"input":[{"type":"item_reference","id":"i"}],"store":false})] {
        let response = request(&client,&f,None,body).header("x-kiro2cc-tenant",A).send().await.unwrap();
        assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    }
    // This is the production handler, not the fixture's independent policy call.
    let response = client.post(format!("{}/real/responses",f.url)).bearer_auth(&f.gateway_key)
        .json(&json!({"model":"gpt-5-codex","input":"x"})).send().await.unwrap();
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("Forwarded client Authorization"));
}

#[tokio::test]
async fn ambiguous_forwarding_and_gateway_key_copy_fail_closed() {
    let f = fixture(true).await; let client = client();
    for value in ["midjourney-proxy".to_string(),"{authenticated_tenant}".to_string(),format!("Bearer {}",f.gateway_key),format!("Bearer sk-{A}-123")] {
        let response = request(&client,&f,Some(&value),json!({"input":"x","store":false})).send().await.unwrap();
        assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        assert!(!response.text().await.unwrap().contains(&value));
    }
    let mut headers = HeaderMap::new();
    headers.append(DEFAULT_CLIENT_AUTH_HEADER,A.parse().unwrap());
    headers.append(DEFAULT_CLIENT_AUTH_HEADER,B.parse().unwrap());
    let duplicate = client.post(format!("{}/v1/responses",f.url)).bearer_auth(&f.gateway_key)
        .headers(headers).json(&json!({"input":"x"})).send().await.unwrap();
    assert_eq!(duplicate.status(),StatusCode::BAD_REQUEST);
    let unauthenticated = client.post(format!("{}/v1/responses",f.url)).bearer_auth("wrong-gateway-key")
        .header(DEFAULT_CLIENT_AUTH_HEADER,A).json(&json!({"input":"x"})).send().await.unwrap();
    assert_eq!(unauthenticated.status(),StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn forwarding_into_unconfigured_mode_is_not_silently_shared() {
    let f = fixture(false).await; let client = client();
    let response = request(&client,&f,Some(A),json!({"input":"x"})).send().await.unwrap();
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("not enabled"));
}
