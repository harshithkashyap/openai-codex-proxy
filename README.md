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
- Local HTTP server
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

If you are already signed in with Codex and have a file-backed `~/.codex/auth.json`, you can serve directly:

```bash
cargo run -- status
cargo run -- serve --addr 127.0.0.1:8787 --local-api-key local-dev-secret
```

If you need to create or refresh file-backed ChatGPT auth from this proxy:

```bash
cargo run -- login
```

For headless or remote hosts where browser callback login cannot reach the local callback server:

```bash
cargo run -- login --device-auth
```

Configure a client as:

```text
Base URL: http://127.0.0.1:8787/v1
API key: local-dev-secret
```

For tools that do not support a custom API key header but send `Authorization: Bearer ...`, use the same local API key as the bearer token.

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
CODEX_PROXY_MODELS=gpt-5.5,gpt-5.4 cargo run -- serve --local-api-key local-dev-secret
```

`/v1/models` is an advertised compatibility catalog for clients. It does not query a live ChatGPT entitlement catalog; unsupported models will fail at the upstream Codex backend if your account cannot use them.

Enable Codex Fast mode by setting the service tier to `fast` or `priority`:

```bash
CODEX_PROXY_SERVICE_TIER=fast cargo run -- serve --local-api-key local-dev-secret
```

The proxy normalizes `fast` to Codex's backend request value, `priority`. Leave `CODEX_PROXY_SERVICE_TIER` unset for standard mode, or set it to `default`, `standard`, `off`, `none`, or `false` to omit `service_tier`. When this default is configured, `/v1/responses` JSON bodies without an explicit `service_tier` are lightly rewritten for GPT-5/Codex models; otherwise `/v1/responses` remains raw pass-through. Explicit request `service_tier` values still win.

Request logs are emitted at `INFO` by default. For more detail:

```bash
RUST_LOG=openai_codex_proxy=debug,tower_http=debug cargo run -- serve --local-api-key local-dev-secret
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

## Security notes

- Keep the proxy bound to `127.0.0.1`.
- `--local-api-key` is required by default. `--allow-no-local-api-key` is only for trusted loopback-only testing.
- Do not expose this service on a LAN or public interface.
- Do not log or share `auth.json`.
