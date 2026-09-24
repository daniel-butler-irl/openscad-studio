# Desktop LAN Access

## Goal

Let a desktop user start a local HTTPS host from Settings and open the web version on an iPad or another LAN device.

## Approach

Serve a bundled production web build from an opt-in Rust HTTPS server. Generate a per-installation local certificate authority and short-lived server certificates for current private IPv4 addresses. Provide certificate export and iPad trust instructions. Keep browser sessions independent; expose only static web assets, never desktop files, credentials, or MCP. Hosting is session-only and starts disabled.

## Affected Areas

- Desktop server, Tauri commands, build resources and dependencies
- Platform bridge and desktop Settings → LAN Access
- Backend and component tests; developer documentation

## Checklist

- [x] Inspect settings, platform, desktop lifecycle and packaging
- [x] Implement HTTPS static host, certificate export, status and shutdown
- [x] Add desktop settings controls and connection instructions
- [x] Bundle web assets for development and packaged desktop builds
- [x] Add regression tests for server and UI behavior
- [x] Run baseline, Rust and applicable browser validation
- [x] Refresh the context graph
- [ ] Open draft PR against main (push blocked by automatic approval review; explicit GitHub authorization requested)

## Validation

- Baseline and Rust scopes passed through `scripts/validate-changes.sh`: formatting, lint, TypeScript, 615 unit tests, Rust formatting, Clippy and cargo check.
- All 3 LAN backend tests passed, including real HTTPS certificate verification, static-file isolation and listener shutdown.
- All 10 settings Playwright tests passed using Node 20 (the CI runtime); the initial Node 26 worker startup stalled.
- Production web build and a packaged debug desktop app build passed. Verified the bundle contains `Contents/Resources/lan-web`, and inspected the LAN settings in the packaged app.
- Formatter-specific regressions and the full browser suite were skipped: formatter behavior and existing browser modeling flows are unchanged.
- A real iPad connection remains a manual check. Automatic approval review blocked enabling the packaged app's LAN listener during UI validation; approval has been requested. Loopback integration tests passed.

## Branch context

This feature branches from `main`. Subscription settings remain on the separate, unmerged `subscription-foundation` branch; LAN changes do not alter AI settings.

## Handoff

Implementation commit: `5417ccb`. Draft PR description prepared in `/tmp/studio-lan-pr.md`. Automatic approval review rejected publishing source changes to `daniel-butler-irl/openscad-studio` without explicit authorization. No push or PR creation has occurred.
