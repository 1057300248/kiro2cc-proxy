// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Opt-in, config-only New API integration. This is a credential fingerprint,
//! NOT an independent authentication protocol: the trusted gateway must overwrite
//! this header from the Authorization it actually used to authenticate the caller.

use std::fmt;

use http::{HeaderMap, header::HeaderName};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::config::Config;

pub(crate) const DEFAULT_CLIENT_AUTH_HEADER: &str = "x-kiro2cc-client-authorization";
const DOMAIN: &[u8] = b"kiro2cc:responses:new-api-key:v1\0";
const INVALID_CREDENTIAL: &str = "Invalid forwarded client credential; use a standard New API Authorization key.";

/// Only the parsed header name is public in Debug. The HMAC key never enters
/// Config serialization, request extensions, error messages, or tracing fields.
pub(crate) struct ClientAuthScope {
    header: HeaderName,
    key: [u8; 32],
}

impl fmt::Debug for ClientAuthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientAuthScope")
            .field("header", &self.header)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl ClientAuthScope {
    pub(crate) fn from_config(config: &Config) -> Result<Option<Self>, &'static str> {
        let Some(header) = config.response_store_client_authorization_header.as_deref() else {
            return Ok(None);
        };
        // Never silently fall back to an older/shared namespace on bad config.
        let key = std::env::var("RESPONSE_STORE_HMAC_KEY")
            .map_err(|_| "Client authorization mode requires RESPONSE_STORE_HMAC_KEY (64 hex characters).")?;
        Self::new(header, config.response_store_tenant_header.as_deref(), &key).map(Some)
    }

    pub(crate) fn new(
        header: &str,
        legacy_tenant_header: Option<&str>,
        hex_key: &str,
    ) -> Result<Self, &'static str> {
        if legacy_tenant_header.is_some() {
            return Err("Configure either responseStoreClientAuthorizationHeader or responseStoreTenantHeader, not both.");
        }
        let header = header.trim().to_ascii_lowercase();
        // Never consume a routing, transport, or the gateway authentication header.
        if !header.starts_with("x-kiro2cc-") || header.len() > 128 {
            return Err("Client authorization header must be a valid X-Kiro2CC-* internal header.");
        }
        let header = HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| "Invalid client authorization header name.")?;
        let mut key = [0_u8; 32];
        if hex_key.len() != 64 || hex::decode_to_slice(hex_key, &mut key).is_err() {
            return Err("RESPONSE_STORE_HMAC_KEY must contain exactly 64 hexadecimal characters.");
        }
        if key.iter().all(|byte| *byte == 0) {
            return Err("RESPONSE_STORE_HMAC_KEY cannot be all zero; generate a random 32-byte key.");
        }
        Ok(Self { header, key })
    }

    /// Called at the authentication boundary. Remove ALL copies before invoking
    /// handlers/forwarders, including malformed or duplicate values. No raw
    /// credential is returned. Missing and malformed headers remain distinct.
    pub(crate) fn take_scope(
        &self,
        headers: &mut HeaderMap,
        gateway_key: Option<&str>,
    ) -> Result<Option<String>, &'static str> {
        let result = self.read_scope(headers, gateway_key);
        headers.remove(&self.header);
        result
    }

    fn read_scope(
        &self,
        headers: &HeaderMap,
        gateway_key: Option<&str>,
    ) -> Result<Option<String>, &'static str> {
        let mut values = headers.get_all(&self.header).iter();
        let Some(value) = values.next() else { return Ok(None); };
        if values.next().is_some() {
            return Err(INVALID_CREDENTIAL);
        }
        let raw = value.to_str().map_err(|_| INVALID_CREDENTIAL)?;
        let canonical = canonical_new_api_key(raw)?;
        // A frequent setup mistake would otherwise put all customers in the
        // gateway's own credential namespace. Reject it rather than degrade.
        if gateway_key
            .and_then(|key| canonical_new_api_key(key).ok())
            .is_some_and(|key| bool::from(canonical.as_bytes().ct_eq(key.as_bytes())))
        {
            return Err("Forwarded client credential must not be the Kiro gateway credential.");
        }
        let mut message = Vec::with_capacity(DOMAIN.len() + canonical.len());
        message.extend_from_slice(DOMAIN);
        message.extend_from_slice(canonical.as_bytes());
        Ok(Some(format!("new-api-key:v1:{}", hex::encode(hmac_sha256(&self.key, &message)))))
    }
}

/// Deliberately support only the unambiguous subset of the inspected New API
/// TokenAuth grammar: raw key, Bearer/bearer, optional sk- prefix, alphanumeric
/// base key. Alternate-auth sentinels and channel-selection suffixes are rejected,
/// not guessed. Key bytes are case-sensitive. No user/body/header-ID fallback.
fn canonical_new_api_key(raw: &str) -> Result<&str, &'static str> {
    if raw.len() > 512 || raw.bytes().any(|b| b.is_ascii_control() || b == b',') {
        return Err(INVALID_CREDENTIAL);
    }
    let raw = raw.trim();
    let key = raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .unwrap_or(raw)
        .trim();
    let key = key.strip_prefix("sk-").unwrap_or(key);
    if !(32..=128).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(INVALID_CREDENTIAL);
    }
    Ok(key)
}

/// HMAC-SHA256 for a fixed 32-byte key, following RFC 2104. No additional crate
/// is required beyond the existing sha2. RFC 4231 test cases pin the construction.
fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Absence never authorizes storage or reads in the gateway-only mode. A channel
/// self-test may use Chat, models, or explicit store=false with fresh full input.
/// A continuation is still a read even when its new response uses store=false.
pub(crate) fn check_request_scope(body: &Value, scope: Option<&str>) -> Result<(), &'static str> {
    if scope.is_some_and(|value| !value.is_empty()) { return Ok(()); }
    let fresh = match body.get("previous_response_id") {
        None | Some(Value::Null) => true,
        Some(Value::String(id)) => id.trim().is_empty(),
        _ => false,
    };
    let references = body.get("input").and_then(Value::as_array).is_some_and(|items| {
        items.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("item_reference"))
    });
    if body.get("store") == Some(&Value::Bool(false)) && fresh && !references {
        Ok(())
    } else {
        Err("Forwarded client Authorization is required for Responses storage or continuation. For a stateless probe use store=false without previous_response_id or item_reference.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = "AbCdEf0123456789AbCdEf0123456789AbCdEf0123456789";
    const B: &str = "BbCdEf0123456789AbCdEf0123456789AbCdEf0123456789";
    fn mode() -> ClientAuthScope {
        ClientAuthScope::new(DEFAULT_CLIENT_AUTH_HEADER, None, &"42".repeat(32)).unwrap()
    }
    fn scope(mode: &ClientAuthScope, value: &str) -> Result<Option<String>, &'static str> {
        let mut headers = HeaderMap::new();
        headers.insert(DEFAULT_CLIENT_AUTH_HEADER, value.parse().unwrap());
        let result = mode.take_scope(&mut headers, None);
        assert!(!headers.contains_key(DEFAULT_CLIENT_AUTH_HEADER));
        result
    }

    #[test]
    fn canonical_forms_keep_scope_but_key_case_and_other_clients_do_not() {
        let mode = mode(); let expected = scope(&mode, A).unwrap();
        for value in [format!("sk-{A}"), format!("Bearer sk-{A}"), format!("bearer {A}"), format!("Bearer   sk-{A} ")] {
            assert_eq!(scope(&mode, &value).unwrap(), expected);
        }
        assert_ne!(scope(&mode, B).unwrap(), expected);
        assert_ne!(scope(&mode, &A.to_ascii_lowercase()).unwrap(), expected);
        assert!(!expected.unwrap().contains(A));
    }

    #[test]
    fn invalid_alternate_or_ambiguous_credentials_fail_without_echo() {
        for value in ["", " ", "midjourney-proxy", "Bearer midjourney-proxy", "{authenticated_tenant}", "{client_header:Authorization}", "Bearer short", "Basic xyz"] {
            assert!(scope(&mode(), value).is_err());
        }
        for value in [format!("Bearer sk-{A}-123"), format!("Bearer sk-{A},Bearer sk-{B}"), format!("BEARER sk-{A}"), "x".repeat(513)] {
            let error = scope(&mode(), &value).unwrap_err();
            assert!(!error.contains(&value));
        }
    }

    #[test]
    fn duplicate_header_is_stripped_and_gateway_auth_preserved() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer sk-{B}").parse().unwrap());
        headers.append(DEFAULT_CLIENT_AUTH_HEADER, A.parse().unwrap());
        headers.append(DEFAULT_CLIENT_AUTH_HEADER, B.parse().unwrap());
        assert!(mode().take_scope(&mut headers, Some(B)).is_err());
        assert_eq!(headers.get_all(DEFAULT_CLIENT_AUTH_HEADER).iter().count(), 0);
        assert_eq!(headers["authorization"], format!("Bearer sk-{B}"));
    }

    #[test]
    fn missing_header_does_not_fall_back_to_arbitrary_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("x-kiro2cc-tenant", A.parse().unwrap());
        headers.insert("x-api-key", A.parse().unwrap());
        assert_eq!(mode().take_scope(&mut headers, None).unwrap(), None);
        headers.insert(DEFAULT_CLIENT_AUTH_HEADER, format!("Bearer sk-{A}").parse().unwrap());
        assert!(mode().take_scope(&mut headers, Some(&format!("sk-{A}"))).is_err());
    }

    #[test]
    fn startup_configuration_fails_closed_and_debug_redacts_secret() {
        for header in ["", "Authorization", "X-Api-Key", "Host", "User-Agent", "x-kiro2cc-bad name"] {
            assert!(ClientAuthScope::new(header, None, &"42".repeat(32)).is_err());
        }
        assert!(ClientAuthScope::new(DEFAULT_CLIENT_AUTH_HEADER, Some("x-tenant"), &"42".repeat(32)).is_err());
        for key in ["".to_owned(), "0".repeat(64), "x".repeat(64), "42".repeat(31)] {
            assert!(ClientAuthScope::new(DEFAULT_CLIENT_AUTH_HEADER, None, &key).is_err());
        }
        assert!(format!("{:?}", mode()).contains("[REDACTED]"));
        assert!(!format!("{:?}", mode()).contains(&"42".repeat(32)));
    }

    #[test]
    fn hmac_is_stable_across_restart_and_changes_with_secret_rotation() {
        assert_eq!(scope(&mode(), A).unwrap(), scope(&mode(), A).unwrap());
        let rotated = ClientAuthScope::new(DEFAULT_CLIENT_AUTH_HEADER, None, &"43".repeat(32)).unwrap();
        assert_ne!(scope(&mode(), A).unwrap(), scope(&rotated, A).unwrap());
    }

    #[test]
    fn rfc4231_vectors() {
        // Keys shorter than 32 bytes are zero padded; RFC 2104 pads them to 64.
        let mut key = [0; 32]; key[..20].fill(0x0b);
        assert_eq!(hex::encode(hmac_sha256(&key, b"Hi There")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        let mut key = [0; 32]; key[..4].copy_from_slice(b"Jefe");
        assert_eq!(hex::encode(hmac_sha256(&key, b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        let mut key = [0; 32]; key[..20].fill(0xaa);
        assert_eq!(hex::encode(hmac_sha256(&key, &[0xdd; 50])), "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe");
    }

    #[test]
    fn no_identity_allows_only_explicit_fresh_stateless_requests() {
        assert!(check_request_scope(&json!({"store":false,"input":"probe"}),None).is_ok());
        for body in [json!({"input":"x"}), json!({"store":true}), json!({"store":null}), json!({"store":false,"previous_response_id":"resp_other"}), json!({"store":false,"input":[{"type":"item_reference","id":"i"}]})] {
            assert!(check_request_scope(&body,None).is_err());
            assert!(check_request_scope(&body,Some("verified-scope")).is_ok());
        }
    }
}
