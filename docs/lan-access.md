# Desktop LAN access

In the desktop app, open **Settings → LAN Access** and select **Start LAN access**. Open one of the displayed HTTPS addresses on another device on the same network. The Mac must remain awake with Studio running. Hosting is off on every app launch; **Stop LAN access** closes the listener immediately.

## First connection from an iPad

1. Select **Save certificate…** and AirDrop the `.crt` file to the iPad.
2. Install the downloaded certificate profile under **Settings → General → VPN & Device Management**.
3. Under **General → About → Certificate Trust Settings**, enable full trust for **OpenSCAD Studio LAN**.
4. Open the address shown in Studio using Safari.

This manual trust step is required by [iPadOS](https://support.apple.com/en-us/102390). Other clients also need to trust the exported certificate. Only install a certificate exported from your own Mac. You can remove the profile from the iPad when you no longer use LAN access.

The browser runs an independent web session: its own files, local AI settings and WebAssembly rendering. Desktop projects, API keys, native rendering and MCP are not exposed through LAN hosting. Files can be opened in the browser and exported using the existing web workflow.

## Troubleshooting

- **No address:** connect the Mac to a network with a private IPv4 address.
- **Cannot connect:** check both devices are on the same network, allow incoming connections to Studio if macOS asks, and check for guest Wi-Fi client isolation.
- **Port in use:** stop the other service using port 3443 and retry.
- **Certificate error or rendering unavailable:** complete both the certificate installation and full-trust steps. Ignoring a browser warning does not establish the secure context required for rendering.
- **Changed networks:** stop and restart LAN access to refresh the displayed addresses and server certificate. The trusted root certificate stays the same.
- **Mac asleep or Studio closed:** reconnect after waking the Mac and restarting LAN access.

## Implementation

`apps/ui/src-tauri/src/lan.rs` serves only the bundled `lan-web` directory over HTTPS on IPv4 port 3443. The app config directory holds a per-installation certificate authority; its key is created with owner-only permissions on Unix. Server certificates are generated at startup for current private IPv4 addresses and expire after 90 days. The private key is never returned to the frontend or served over the network. The public root certificate can be exported through the native save dialog.

The host supplies COOP/COEP headers for cross-origin isolation and WebAssembly rendering. It has no filesystem, workspace, AI, render or MCP API. The existing localhost MCP service remains separate. Multiple desktop windows share one server; Settings polls its live state. Closing Settings leaves hosting running; quitting Studio ends it.

Tauri's pre-development and pre-build commands build `apps/web/dist`, and the resource map includes it as `lan-web`. Development serves the built web directory (rerun `pnpm web:build` to refresh it); packaged builds serve the bundled copy. Rust-only CI checks create a placeholder asset directory, while actual desktop builds always run the real web build.
