# Comidea Codex Image Bridge for macOS - Execution Plan

## 1. Goal

Build a temporary macOS bridge that restores generated images directly in the
Codex Desktop chat while keeping the existing Windows Rust application
unchanged.

The macOS delivery is one source file:

```text
mac-bridge/codex-image-bridge.mjs
```

Target computers install the official Node.js 24 runtime, then run this file.
The first release does not use Electron, an app bundle, DMG, PKG, SEA, a
background service, or an Apple Developer certificate.

## 2. Fixed Scope

- macOS only; Windows remains owned by `CodexImageFix.exe`.
- Node.js 24, native ESM, standard library only.
- Local browser UI for API URL, API Key, image switch, diagnostics, and launch.
- API Key is process-only and is never written to disk.
- The bridge launches the official Codex app with `CODEX_CLI_PATH` pointing to
  itself and proxies the official bundled CLI.
- The official app bundle and Codex session files are never modified.
- `gpt-image-2` is the Codex image tool backend, not a chat-model selector item.

## 3. Runtime Flow

1. Run `node codex-image-bridge.mjs`.
2. The bridge listens on a random `127.0.0.1` port and opens the browser UI.
3. Enter an OpenAI-compatible `/v1` base URL and API Key.
4. Test the service and click **Launch Codex**.
5. The bridge asks an already running Codex app to quit, then starts the
   official app executable with process-only environment variables.
6. Codex Desktop starts this file with `app-server` arguments.
7. The file starts the official bundled CLI with temporary `-c` overrides and
   transparently proxies JSONL messages.
8. Generated image data is read from the session, validated, stored locally,
   and emitted to Desktop as `imageGeneration` items with `savedPath`.

The browser UI process may be closed after Codex has launched. Each Codex
launch must go through the bridge so the new process receives the API Key.

## 4. Provider Injection

The CLI process receives temporary overrides equivalent to:

```toml
model_provider = "comidea"

[model_providers.comidea]
name = "OpenAI"
base_url = "https://example.com/v1"
env_key = "COMIDEA_API_KEY"
wire_api = "responses"
requires_openai_auth = true

[features]
image_generation = true
```

The API Key is supplied only as `COMIDEA_API_KEY` in the child environment. It
must not appear in CLI arguments, configuration files, HTTP responses,
diagnostics, or logs. Existing ChatGPT authentication remains available to the
official CLI because current Codex image-tool availability is authentication
gated.

## 5. Image Recovery Contract

- Record wall-clock milliseconds and the current session byte offset before
  forwarding `turn/start`.
- Locate sessions by explicit path, remembered thread mapping, read-only
  SQLite, then bounded directory scanning as a fallback.
- Open SQLite read-only and enable `PRAGMA query_only=ON`.
- Parse JSONL incrementally from byte offsets; never repeatedly parse old
  multi-megabyte Base64 lines.
- Accept both `response_item/image_generation_call` and
  `event_msg/image_generation_end`.
- Do not require `status == completed`; require a valid result instead.
- Strictly validate PNG, JPEG, and WebP structure.
- Validate and hash an existing `saved_path`; if Base64 is also present, reuse
  the file only when both SHA256 values match.
- Store by SHA256, using a private temporary file, file `fsync`, atomic rename,
  and directory `fsync`.
- Deduplicate by image ID and SHA256.
- Never modify the source session.
- Never log Base64, prompts, API Keys, or authorization headers.

## 6. Desktop Protocol Contract

For live turns, inject paired `item/started` and `item/completed` messages
before the official `turn/completed` message. For history methods, supplement
responses to `thread/read`, `thread/resume`, `thread/fork`, and
`thread/rollback`.

Official `imageGeneration` items are materialized immediately when possible.
Their `result` field is cleared before forwarding so Desktop receives only the
absolute local `savedPath`, not Base64.

## 7. Local UI Security

- Bind only to `127.0.0.1` on a random operating-system-assigned port.
- Use a random bootstrap token, an `HttpOnly; SameSite=Strict` cookie, and a
  separate CSRF token.
- Validate Host, Origin, method, JSON content type, and request size.
- Use a nonce-based CSP and no remote scripts, fonts, images, analytics, or CDN.
- Do not return the API Key from any endpoint or repopulate it after refresh.
- Keep navigation synchronous so rapid menu clicks cannot display stale pages.

## 8. Deliverables

- [x] `mac-bridge/codex-image-bridge.mjs`
- [x] `mac-bridge/test.mjs`
- [x] `mac-bridge/package.json`
- [x] `mac-bridge/README.md`
- [x] Isolated macOS Node 24 CI workflow
- [x] Root README split between Windows Rust and macOS Node usage

## 9. Automated Acceptance

- [x] Old images before the recorded offset are excluded.
- [x] Both image event shapes and non-completed statuses are accepted.
- [x] Truncated JSONL remains pending until completed.
- [x] Corrupt PNG, JPEG, and WebP are rejected.
- [x] Oversized Base64 is rejected before decoding.
- [x] `saved_path` hash matches are reused; mismatches are not.
- [x] SHA256 storage and protocol injections are deduplicated.
- [x] API Key never appears in child arguments or diagnostic JSON.
- [x] Local UI rejects invalid Host, Origin, CSRF, and content type.
- [x] Existing Windows Rust formatting, tests, and build remain healthy.

## 10. Required Real-Mac Acceptance

Windows development and CI can validate all portable logic. Final release
still requires an Apple Silicon Mac with Node.js 24 and the current Codex
Desktop app to verify:

- [ ] bundled app and CLI paths;
- [ ] `CODEX_CLI_PATH` handoff;
- [ ] environment inheritance after a full Codex restart;
- [ ] custom provider authentication and `gpt-image-2` availability;
- [ ] a newly generated image appearing directly in the chat;
- [ ] browser UI behavior in Safari and Chrome.

Until these checks pass, the macOS build is a test candidate rather than a
finished mass-deployment release.
