# Stateful Responses integration audit — 2026-10-01

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

1. Superseded companion dependency: Kiro now supports opt-in config-only New API integration via responseStoreClientAuthorizationHeader and an environment-only HMAC key. Only the inspected standard Authorization grammar is canonicalized; special authentication paths fail closed. The gateway must overwrite the internal header from its authenticated credential and disable passthrough fallbacks. No New API source patch is required. See docs/new-api-config-only.md for the precise trust boundary.
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

The continuation store is still in-memory, not durable/shared; restarting or routing a chain to another process loses it. It is reasonable to add an independently tested Redis/SQLite backend later rather than treating durability as already implemented. Scope is per canonical downstream API credential in the new config-only mode, plus the authenticated Kiro API-key ID; it is not per human/user across different tokens. It relies on correct trusted-gateway forwarding and is not an independent verifier of New API credentials. Shared-key usage and limits remain New API's responsibility. The raw compatibility endpoints do not promise every native OpenAI feature (for example encrypted reasoning or arbitrary tool_choice). These are explicit boundaries, not silently enabled features.
