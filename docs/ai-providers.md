# AI providers

OpenSCAD Studio supports direct API keys, local OpenAI-compatible servers, and desktop subscription sign-in.

## Connect a subscription account

In the desktop app, open **Settings → AI** and choose **Sign in** under ChatGPT subscription or Grok subscription. Finish authorization in the provider browser page; device-code sign-in also displays a code to enter there. The settings card updates when the native login completes. Choose a model in the chat composer after its account catalog loads.

The standalone web app does not offer subscription sign-in. Use an API key there, or configure a local OpenAI-compatible server. API-key and local-server settings remain separate from subscription accounts.

Codex and Grok subscription requests use the account's available models and do not silently fall back to a paid API-key endpoint. Model entries retain the provider's supported request format and known tool, image, and reasoning capabilities. Unsupported or unknown request formats are not sent. The native transport sends Codex Responses requests with response storage disabled.

Codex conversations continue statelessly from Studio's local chat history. Encrypted reasoning metadata is replayed only for the same Codex account generation; switching or signing out invalidates that continuation.

Use **Disconnect** in AI Settings to remove the desktop session. This clears the cached model list and cancels active requests for that provider. Sign-in state is independent of Codex or Grok CLI login state; Studio does not read CLI credentials.

## Credential handling

The desktop stores OAuth refresh credentials in the operating system credential manager. Access tokens, refresh tokens, authorization headers, and raw provider error bodies remain in the Rust process. The webview receives sanitized account status, model metadata, and ordered response bytes. Provider request hosts and routes are selected by native code, and cancellation is scoped to the window and account generation that started the request.

API keys use the existing browser/webview storage path. They are not moved into the subscription credential store by this feature.

## Protocol references and attribution

The implementation uses public protocol behavior as reference and contains no copied source code or tests from these projects:

- [OpenCode](https://github.com/anomalyco/opencode), pinned at `0f549842ee746e400b1f72516b0b2e292e267e2c`, is MIT-licensed. Its OAuth/provider behavior was reviewed as protocol evidence; no implementation was copied or adapted.
- [Zed](https://github.com/zed-industries/zed), pinned at `baee1ca6de0feb9b87d4e5288cf40b02bdf4c370`, contains GPL-3.0-or-later code in `crates/openai_subscribed`. It was used only to verify protocol details; no source or tests were copied or adapted into this GPL-2.0 project.

Provider endpoints and model availability can change. Catalog entries are filtered or marked unknown when Studio cannot establish a supported request format rather than guessing how to send a request.
