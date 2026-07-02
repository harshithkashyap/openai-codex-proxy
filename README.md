# openai-codex-proxy

Local OpenAI-compatible proxy for the ChatGPT/Codex backend.

This is an experimental localhost proxy. It does **not** expose ChatGPT OAuth tokens to downstream tools. Downstream clients point at a local OpenAI-compatible base URL, and the proxy reads Codex-compatible ChatGPT auth locally to add upstream authorization.

## Motivation

This project exists to make ChatGPT/Codex subscription access usable from local clients that already work well on Linux and other desktop environments.

The official Codex app is a purpose-built desktop surface for Codex, with first-party desktop support documented for macOS and Windows. On Linux, the practical first-party options are the CLI, web, and editor-style surfaces. Those are useful, but they do not provide the same native desktop experience for managing long-running work, switching between local and remote/cloud sessions, or continuing work from companion apps and other devices.

Instead of rebuilding a full desktop app, this proxy lets existing Linux-friendly clients speak familiar OpenAI-compatible or Anthropic-compatible HTTP APIs while the proxy handles ChatGPT/Codex authentication and upstream request shape locally. The goal is to reuse mature configurable clients rather than recreate every Codex app feature.

This is not an Anthropic subscription proxy and it does not call Anthropic's backend. Anthropic-compatible support exists only so clients such as Claude Desktop, Claude Code-style tools, and other Claude/Anthropic API clients can be pointed at this local server and use ChatGPT/Codex-backed requests. The same idea applies to OpenAI-compatible clients, Copilot-style desktop/editor tools, and any other local app that can be configured with a custom base URL.

This project should become less important if first-party Codex desktop support covers the same Linux and cross-platform workflows directly. Until then, it is a pragmatic bridge for using ChatGPT subscription-backed Codex capabilities from clients that are already available on the user's machine.

## Use cases

- Use OpenAI-compatible local tools against the ChatGPT/Codex backend with a normal `Base URL` and local API key.
- Point clients that support OpenAI's Responses API at `/v1/responses` while preserving Codex session/thread metadata and usage headers.
- Run narrower Chat Completions clients through `/v1/chat/completions` when they only need text, image parts, function tools, reasoning controls, verbosity, streaming, and service-tier options.
- Use Anthropic-compatible clients against `/v1/messages` through Claude-style model aliases that map back to Codex models.
- Test Codex Fast mode behavior locally by configuring a default `service_tier` without changing each request body.

## Implemented

- Browser-based ChatGPT login by default
- Device-code ChatGPT login fallback with `--device-auth`
- Codex-compatible auth storage
  - defaults to `$CODEX_HOME/auth.json`
  - falls back to `~/.codex/auth.json`
  - can be overridden with `CODEX_PROXY_AUTH_FILE`
- On-demand access token refresh
  - refreshes within 5 minutes of JWT expiry
  - fallback refresh if `last_refresh` is older than 8 days
- Refresh preserves unknown Codex auth fields and keeps `tokens.account_id` aligned with refreshed token claims
- Local proxy config for generated downstream API keys
- Local HTTP server
- Linux system tray controller with ChatGPT login/logout, connected status, generated local API key, copyable client settings, bundled project icons, proxy status, release version, start/stop actions, and log opening
- `GET /health`
- `GET /v1/models` using a configurable advertised model list, with Codex-style reasoning/verbosity/Fast service-tier metadata for GPT-5/Codex models
- `POST /v1/responses` streaming proxy to `https://chatgpt.com/backend-api/codex/responses`
- `POST /v1/responses/compact` pass-through proxy to `https://chatgpt.com/backend-api/codex/responses/compact`
- Selected Codex/OpenAI pass-through headers, including session/thread metadata, attestation, tracing, compression, rate-limit, and usage-window headers
- Restricted ChatGPT Cloudflare infrastructure cookie store for upstream requests only
- Narrow `POST /v1/chat/completions` compatibility shim, including streaming, text/image content parts, function tool calls, function tool outputs, best-effort mapping for `reasoning`, `reasoning_effort`, `reasoningSummary`, verbosity options, and `service_tier`
- Narrow Anthropic Messages compatibility shim at `POST /v1/messages`, plus `POST /v1/messages/count_tokens`
- Anthropic-shaped model discovery from `GET /v1/models` when the request includes `x-codex-proxy-format: anthropic` or an `anthropic-version` header
- Sanitized JSONL compatibility trace log at `/tmp/openai-codex-proxy.log`

## Compatibility limits

The proxy intentionally keeps `/v1/responses` and `/v1/responses/compact` as close to pass-through as possible. The compatibility limits below are proxy/API facade gaps, not claims that Codex CLI lacks these features.

- Structured outputs, audio content parts, legacy `functions` fields, and other advanced `/v1/chat/completions` translation. Use `/v1/responses` for those request shapes.
- `/v1/chat/completions` stream usage chunks from `stream_options` are not synthesized.
- Anthropic compatibility is intentionally narrow. It supports message text, image blocks, tool use/tool results, streaming, non-streaming aggregation, and a rough local token count estimate, not every Claude API field or exact usage accounting.
- Some clients hardcode thinking/reasoning controls from their own model catalog. `/v1/models` advertises Codex-compatible reasoning metadata, but a client may still hide its UI control for custom providers.
- Full OpenAI response-shape normalization for every endpoint.
- `/v1/embeddings`, `/v1/images`, `/v1/audio`, etc.
- OS keychain storage in this proxy. Use Codex itself if your Codex auth is keychain-backed.
- Codex app-server, MCP, subagent orchestration, and local tool execution surfaces. Those are agent runtime features, not lightweight HTTP proxy behavior.

## Usage

Release archives and AppImages only provide an executable. Installing, copying, or opening the executable does not install a daemon, register a background service, or configure a desktop client. The proxy runs only while `openai-codex-proxy serve` or `openai-codex-proxy tray` is running. In `serve` mode it logs to the terminal and stops on Ctrl-C. In `tray` mode it stops when you choose Quit from the tray menu or close the app process.

The local API key is a secret for downstream clients that connect to this localhost proxy. It is not your ChatGPT token. In `serve` mode you provide it with `--local-api-key` or `CODEX_PROXY_LOCAL_API_KEY`. In Linux tray mode the app generates and saves one automatically if you did not provide one.

### Install a release binary

Linux and macOS release archives contain the executable plus `README.md` and `LICENSE`:

```bash
tar -xzf openai-codex-proxy-0.1.0-linux-x64.tar.gz
sudo install -m 0755 openai-codex-proxy-0.1.0-linux-x64/openai-codex-proxy /usr/local/bin/openai-codex-proxy
```

Linux AppImages do not need installation:

```bash
chmod +x openai-codex-proxy-0.1.0-linux-x64.AppImage
./openai-codex-proxy-0.1.0-linux-x64.AppImage
./openai-codex-proxy-0.1.0-linux-x64.AppImage status
./openai-codex-proxy-0.1.0-linux-x64.AppImage serve --local-api-key local-dev-secret
```

Opening a release AppImage with no arguments starts the Linux tray controller. Passing any CLI argument keeps the normal command-line behavior.

You can also put the AppImage on your PATH:

```bash
mkdir -p ~/.local/bin
mv openai-codex-proxy-0.1.0-linux-x64.AppImage ~/.local/bin/openai-codex-proxy
```

Windows release archives contain `openai-codex-proxy.exe`; run it from PowerShell or add its directory to `PATH`.

Stable release asset names include the release version and intentionally omit commit hashes. Automatic `main` prereleases use the package version plus a SemVer prerelease-style build segment, such as `openai-codex-proxy-0.1.0-main.8.1-linux-x64.AppImage`.

| Platform | Asset |
| --- | --- |
| Linux x64 AppImage | `openai-codex-proxy-0.1.0-linux-x64.AppImage` |
| Linux ARM64 AppImage | `openai-codex-proxy-0.1.0-linux-arm64.AppImage` |
| Linux x64 archive | `openai-codex-proxy-0.1.0-linux-x64.tar.gz` |
| Linux ARM64 archive | `openai-codex-proxy-0.1.0-linux-arm64.tar.gz` |
| macOS Intel archive | `openai-codex-proxy-0.1.0-macos-x64.tar.gz` |
| macOS Apple Silicon archive | `openai-codex-proxy-0.1.0-macos-arm64.tar.gz` |
| Windows x64 archive | `openai-codex-proxy-0.1.0-windows-x64.zip` |
| Windows ARM64 archive | `openai-codex-proxy-0.1.0-windows-arm64.zip` |
| Checksums | `SHA256SUMS` |

### Start the proxy

If you are already signed in with Codex and have a file-backed `~/.codex/auth.json`, you can serve directly:

```bash
openai-codex-proxy status
openai-codex-proxy serve --addr 127.0.0.1:8787 --local-api-key local-dev-secret
```

If you need to create or refresh file-backed ChatGPT auth from this proxy:

```bash
openai-codex-proxy login
```

For headless or remote hosts where browser callback login cannot reach the local callback server:

```bash
openai-codex-proxy login --device-auth
```

Configure a client as:

```text
Base URL: http://127.0.0.1:8787/v1
API key: local-dev-secret
```

For tools that do not support a custom API key header but send `Authorization: Bearer ...`, use the same local API key as the bearer token.

### Linux system tray

For a lightweight desktop workflow on Linux, run the same executable in tray mode:

```bash
openai-codex-proxy tray
```

Opening a release AppImage without arguments also starts tray mode.

The tray menu shows the current proxy status, ChatGPT connected status, release/build identifier, base URL, and the latest lifecycle message. Use **Log in to ChatGPT** to start browser OAuth, then **Start Proxy** to run the local server in the same process. When you are signed in, the menu shows a connected state and enables **Log out of ChatGPT**. Logging out removes the local file-backed ChatGPT auth and stops the proxy first if it is running.

The tray status icon and menu action icons are bundled with the project instead of relying on desktop-theme icon names. AppImage launcher metadata also uses the bundled project icon.

Use **Copy Base URL**, **Copy API Key**, or **Copy Client Settings** to configure clients. The default copied settings are:

```text
Base URL: http://127.0.0.1:8787/v1
API key: <generated local proxy key>
Authorization: Bearer <generated local proxy key>
```

The tray stores the generated local proxy API key in the user config directory, normally `~/.config/openai-codex-proxy/config.json`, with owner-only file permissions on Unix. Override that path with `CODEX_PROXY_CONFIG_FILE`. The tray does not display ChatGPT OAuth tokens or the local API key in the menu; it only copies the local API key when requested. Clipboard copy uses `wl-copy`, `xclip`, or `xsel`, so install one of those utilities if copy actions report that no clipboard command is available.

The tray starts stopped; it does not autostart the proxy, install a daemon, or persist a background service after Quit.

GNOME and some other desktops require AppIndicator/KStatusNotifier support to display tray icons. If your desktop does not expose a StatusNotifier/AppIndicator host, use `serve` mode or enable the desktop's tray support.

To run the proxy in the background, wrap the same foreground command with your process manager. For example, a user-level systemd service at `~/.config/systemd/user/openai-codex-proxy.service` can run the proxy after you create `~/.config/openai-codex-proxy/env` containing `CODEX_PROXY_LOCAL_API_KEY=local-dev-secret`:

```ini
[Unit]
Description=OpenAI Codex Proxy
After=network-online.target

[Service]
ExecStart=%h/.local/bin/openai-codex-proxy serve --addr 127.0.0.1:8787
EnvironmentFile=%h/.config/openai-codex-proxy/env
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable it with:

```bash
systemctl --user daemon-reload
systemctl --user enable --now openai-codex-proxy.service
```

### Run from source

During development, replace `openai-codex-proxy` with `cargo run --`:

```bash
cargo run -- status
cargo run -- serve --addr 127.0.0.1:8787 --local-api-key local-dev-secret
cargo run -- tray --local-api-key local-dev-secret
```

### Anthropic-compatible clients

Anthropic-style clients should use the same local API key and point at the proxy's `/v1` base URL:

```text
Base URL: http://127.0.0.1:8787/v1
API key: local-dev-secret
```

`POST /v1/messages` accepts Claude-style requests and maps advertised Claude-style aliases back to Codex model IDs before sending them upstream:

| Advertised alias | Upstream Codex model |
| --- | --- |
| `claude-opus-4-8` | `gpt-5.5` |
| `claude-opus-4-7` | `gpt-5.4` |
| `claude-haiku-4-5-20251001` | `gpt-5.4-mini` |
| `claude-sonnet-5` | `gpt-5.3-codex-spark` |

Clients that discover models through `GET /v1/models` can request Anthropic-shaped model metadata by sending either:

```text
x-codex-proxy-format: anthropic
```

or a normal Anthropic SDK version header:

```text
anthropic-version: 2023-06-01
```

Advertise a different model list with:

```bash
CODEX_PROXY_MODELS=gpt-5.5,gpt-5.4 openai-codex-proxy serve --local-api-key local-dev-secret
```

`/v1/models` is an advertised compatibility catalog for clients. It does not query a live ChatGPT entitlement catalog; unsupported models will fail at the upstream Codex backend if your account cannot use them.

Enable Codex Fast mode by setting the service tier to `fast` or `priority`:

```bash
CODEX_PROXY_SERVICE_TIER=fast openai-codex-proxy serve --local-api-key local-dev-secret
```

The proxy normalizes `fast` to Codex's backend request value, `priority`. Leave `CODEX_PROXY_SERVICE_TIER` unset for standard mode, or set it to `default`, `standard`, `off`, `none`, or `false` to omit `service_tier`. When this default is configured, `/v1/responses` JSON bodies without an explicit `service_tier` are lightly rewritten for GPT-5/Codex models; otherwise `/v1/responses` remains raw pass-through. Explicit request `service_tier` values still win.

Request logs are emitted at `INFO` by default. For more detail:

```bash
RUST_LOG=openai_codex_proxy=debug,tower_http=debug openai-codex-proxy serve --local-api-key local-dev-secret
```

Compatibility traces are also appended to `/tmp/openai-codex-proxy.log` by default:

```bash
tail -f /tmp/openai-codex-proxy.log
```

These JSONL logs include endpoint status, sanitized chat request shape, translated Responses shape, upstream status, and one concise stream summary per response. They do not include auth headers or raw prompt/tool output text. Override the path with `CODEX_PROXY_LOG_FILE=/tmp/my-proxy.log`, or disable file logging with `CODEX_PROXY_LOG_FILE=off`.

## Test

```bash
curl http://127.0.0.1:8787/health \
  -H 'Authorization: Bearer local-dev-secret'

curl http://127.0.0.1:8787/v1/models \
  -H 'Authorization: Bearer local-dev-secret'
```

Responses endpoint example:

```bash
curl -N http://127.0.0.1:8787/v1/responses \
  -H 'Authorization: Bearer local-dev-secret' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-5.5",
    "input": [{"role":"user","content":"Say hello in one sentence."}],
    "stream": true,
    "store": false
  }'
```

## Release automation

GitHub Actions runs CI on pushes, pull requests, and manual dispatch. CI checks formatting, Clippy, tests, and a debug build on Linux, macOS, and Windows.

When CI completes successfully on `main`, the release workflow automatically creates a prerelease tag named `main-<ci-run>-<attempt>-<short-sha>`, creates a GitHub prerelease with generated release notes, and uploads binaries for that exact commit. This is the continuous release channel for testing current `main` builds.

Publishing a tag that starts with `v` runs the stable release path:

```bash
git tag v0.1.0
git push origin v0.1.0
```

You can also rerun the release workflow manually for an existing tag from the GitHub Actions UI.

The release workflow validates the source, builds release binaries for Linux, macOS, and Windows on x64 and ARM64 runners, and uploads archives to the GitHub release. Linux releases also include x64 and ARM64 AppImages. Opening a release AppImage without arguments launches the tray controller; CLI subcommands still work by passing arguments. Each archive includes the binary, `README.md`, and `LICENSE`. Stable asset filenames include the semver release number, while automatic `main` prerelease filenames use the Cargo package version plus `main.<ci-run>.<attempt>` and omit commit hashes.

Release assets include one combined `SHA256SUMS` file instead of separate checksum files for every binary. The workflow also generates GitHub artifact attestations for each binary archive, each AppImage, and `SHA256SUMS`; verify them with:

```bash
gh attestation verify openai-codex-proxy-0.1.0-linux-x64.tar.gz \
  -R harshithkashyap/openai-codex-proxy
```

## Security notes

- Keep the proxy bound to `127.0.0.1`.
- `--local-api-key` is required by default. `--allow-no-local-api-key` is only for trusted loopback-only testing.
- Do not expose this service on a LAN or public interface.
- Do not log or share `auth.json`.
