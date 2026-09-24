# Subscription AI Providers

## Goal

Add first-class Codex and Grok subscription connections to OpenSCAD Studio while preserving the existing shared frontend AI conversation, tools, edit validation, checkpoints, and rendering flow. Subscription credentials remain in the desktop OS keychain and are never exposed to frontend JavaScript.

## Approach

Use separate API-key and subscription provider types. A discriminated connection model resolves either a direct SDK API-key model or a desktop-owned subscription transport. The Tauri boundary exposes sanitized account status and lifecycle operations plus scoped streaming requests; provider origins and authorization remain fixed in Rust. Web reports subscriptions as unsupported. Request IDs and account generations bind cancellation to the originating window and authenticated account; stateless continuation metadata stays in the local conversation and is scoped to the matching provider/account generation.

## Affected Areas

- `apps/ui/src/stores/apiKeyStore.ts` and model selection/provider catalogs
- `apps/ui/src/platform/` and `apps/ui/src-tauri/src/subscriptions/`
- `apps/ui/src/services/aiService.ts`, chat settings/components, and tests
- Rust dependencies, command registration, and desktop credential/session handling
- `AGENTS.md`, `CLAUDE.md`, provider setup documentation, and attribution notices

## Keychain prompt follow-up

Repeated status polling and concurrent session restoration could read the same OS credential repeatedly. Native auth now hydrates each provider’s stored credential once per app session, shares concurrent reads, caches read failures until an explicit reconnect, and updates or invalidates the cache on sign-in, refresh, and sign-out. The settings panel keeps its fast status poll only while a login challenge is pending. If replacing a refresh credential fails while the current access token remains usable, Settings displays a sanitized reconnect warning.

## Checklist

- [x] Establish typed API/subscription provider and connection contracts; record and review bridge interfaces.
- [x] Implement secure native login, account status, refresh, signout, and cancellation for Codex and Grok.
- [x] Implement fixed-origin provider transports with ordered streaming, cancellation, and account/request generation guards.
  - [x] Add authenticated Codex/Grok model catalogs, backend routing metadata, capability parsing, and account-generation cache.
  - [x] Enforce Codex Responses `store:false` plus Studio instructions and provider-specific fixed headers.
  - [x] Add cancellable native per-window request channel, signout/window cleanup, one pre-response refresh retry, and sanitized status errors.
  - [x] Add native HTTP/channel stream tests for pre-header cancellation and signout invalidation during streaming.
  - [x] Add SDK-compatible fetch adapter and fragmented Responses tool-call/result regression.
- [x] Integrate provider catalogs, desktop settings, model selection, chat streaming, multimodal capabilities, and account-scoped stateless continuation.
- [x] Add focused Rust and frontend tests for session isolation, lifecycle races, protocol errors, and stream cancellation.
  - [x] Cover concurrent status/session hydration, cached keychain failures and reconnect, sign-out during a blocked read, and failed reconnect with preserved access.
  - [x] Cover fast-poll stop on successful/failed login and the signed-in reconnect warning.
- [x] Update architecture and user setup documentation, including upstream attribution and license notices.
- [x] Run baseline and Rust validation, web and desktop builds, diff checks, and report live acceptance limits.
  - Baseline: `scripts/validate-changes.sh --scope baseline` passed (86 suites, 621 tests).
  - Rust: `scripts/validate-changes.sh --scope rust` passed; `cargo test --lib` passed (36 tests).
  - Web production build and Tauri debug app bundle completed successfully.
  - Live provider sign-in and restart persistence remain for the user’s manual acceptance; no credentials were accessed during automated validation.
- [x] Refresh graft after the implementation.
- [x] Open draft PR [#1](https://github.com/daniel-butler-irl/openscad-studio/pull/1) targeting `main`.
- [x] Check preview status; GitHub currently reports zero registered Actions workflows, so no PR preview URL was produced.

The latest macOS debug bundle is `apps/ui/src-tauri/target/debug/bundle/macos/OpenSCAD Studio.app`. It was built but not launched or installed during final validation. Automated tests use injected credential stores; no OS Keychain probe was run.
