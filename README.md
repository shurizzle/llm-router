# llm-router

A reverse proxy that lets one Claude Code session talk to several Anthropic-compatible
services. Claude Code accepts only one `ANTHROPIC_BASE_URL` per process, so this proxy reads
the `model` field of each request and forwards it to the backend that serves that model.

The router does not translate between API formats. Every backend must already speak the
Anthropic Messages API. The proxy selects a route, swaps or forwards credentials, optionally
rewrites the model name and removes headers or body fields the backend rejects, then relays
the response as it arrives.

```
Claude Code
  ANTHROPIC_BASE_URL=http://127.0.0.1:8787
          │
          ▼
     llm-router ──▶ claude-*        → first-party API        (caller credentials)
          ├───────▶ hf:* / syn:*    → open-model provider    (route credential)
          ├───────▶ nano:*          → second provider        (route credential, prefix removed)
          └───────▶ anything else   → default_route
```

## Features

- Model routing by glob pattern (`claude-*`, `hf:*`), exact alias (`rewrite`) or prefix
  stripping (`nano:`).
- Three credential modes per route: forward the caller's headers, send a bearer token, send
  an `x-api-key` token. A route without a credential disables itself instead of blocking
  startup.
- Per-route header removal and injection, top-level body field removal, and per-tool field
  removal.
- Unbuffered SSE relay; upstream status codes and error bodies reach the client unchanged.
- Prometheus metrics for requests, latency, time to first byte, in-flight requests, token
  usage (including cache counters) and upstream quota values.
- Liveness and readiness endpoints, with readiness failing before listeners close on
  `SIGTERM`.
- Optional shared client token. Requests without it are dropped without a response.
- Small dependency set, single binary, distroless container image.

## Build and run

The project requires Rust 1.99.

```bash
cargo build --release
SYNTHETIC_API_KEY=... NANOGPT_API_KEY=... ./target/release/llm-router --config config.example.json
```

The service listens on `:8787` by default. Check it with:

```bash
curl -s localhost:8787/readyz
```

Configuration and credentials are read at startup. The config file is watched: saving it, or
sending `SIGHUP`, reloads the config file, credentials, client token and egress client without
dropping listeners; `listen`, the admin address and the drain delay are startup-only. A failed
reload is logged as `reload failed` and the old configuration keeps serving. Validate a file
with `--check-config` before reloading or restarting.

### Container

```bash
docker build -t llm-router:local .
docker run -d --name llm-router \
  -p 127.0.0.1:8787:8787 \
  -e SYNTHETIC_API_KEY \
  -v "$PWD/config.json:/etc/llm-router/config.json:ro" \
  llm-router:local
```

Or start it from this directory with Compose:

```bash
export SYNTHETIC_API_KEY=... NANOGPT_API_KEY=...
docker compose up -d --build
```

The project does not publish a registry image. Build and tag the image yourself if you need
to distribute it.

## Use with Claude Code

Keep the existing claude.ai login. Set the base URL and leave `ANTHROPIC_API_KEY` and
`ANTHROPIC_AUTH_TOKEN` unset, so the stored subscription credential stays active and is
forwarded to the route that passes credentials through.

```bash
claude-mix() {
  ANTHROPIC_BASE_URL=http://127.0.0.1:8787 \
  CLAUDE_CODE_ATTRIBUTION_HEADER=0 \
  claude "$@"
}
```

Or persist the same settings in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787",
    "CLAUDE_CODE_ATTRIBUTION_HEADER": "0"
  }
}
```

`CLAUDE_CODE_ATTRIBUTION_HEADER=0` prevents a per-request header from changing the system
prompt bytes, which keeps provider-side prompt caches effective.

If `LLM_ROUTER_CLIENT_TOKEN` is set, every API request must carry the token:

```bash
export ANTHROPIC_CUSTOM_HEADERS="X-Llm-Router-Token: <token>"
```

A missing or invalid token makes the router close the connection without replying. Claude
Code reports a connection error rather than an authentication error.

### Delegating to other models

Claude Code sends the model ID from an agent's frontmatter verbatim, even when it does not
recognize it. That makes subagents the practical way to use a second provider: the main loop
stays on a first-party model, while delegated work runs on an open model.

```yaml
---
name: worker
description: Implementation worker for focused coding tasks.
model: hf:deepseek-ai/DeepSeek-V4.1-Flash
tools: Read, Grep, Glob, Bash, Edit, Write
---
```

Keep the `tools` allowlist. Without it the subagent inherits every available tool, including
all MCP servers, and those schemas are sent on every request.

Ready-made agent files are in [`examples/agents/`](examples/agents). See
[docs/claude-code.md](docs/claude-code.md) for provider-specific notes and limits.

## Configuration

The file is JSON. Unknown fields are errors, so a misspelled key stops startup. Resolution
order for the path: `--config`, then `LLM_ROUTER_CONFIG`, then
`/etc/llm-router/config.json`.

`config.schema.json` gives editors completion and validation for the file. It is generated
from the Rust types and checked for drift by `cargo test`.

```json
{
  "listen": ":8787",
  "default_route": "anthropic",
  "routes": [
    {
      "name": "open-models",
      "upstream": "https://api.synthetic.new/anthropic",
      "models": ["hf:*", "syn:*"],
      "auth": { "mode": "bearer", "token": "{env:SYNTHETIC_API_KEY}" },
      "drop_headers": ["anthropic-beta"]
    },
    {
      "name": "anthropic",
      "upstream": "https://api.anthropic.com",
      "models": ["claude-*"],
      "auth": { "mode": "passthrough" }
    }
  ]
}
```

Route fields:

| Field | Purpose |
|---|---|
| `name` | Unique label used in metrics and logs. |
| `upstream` | Absolute `http(s)` base URL. The request path is appended to it. |
| `models` | Glob patterns. `*` matches any sequence, including `/`. |
| `rewrite` | Exact model aliases. Keys also count as route matches. |
| `strip_prefix` | Prefix removed before forwarding. `rewrite` takes precedence. |
| `auth.mode` | `passthrough`, `bearer` or `x-api-key`. |
| `auth.token` | Credential template. `{env:NAME}` and `{file:PATH}` placeholders are expanded once at startup; other text is literal. |
| `drop_headers` / `set_headers` | Headers removed or set on the outbound request. `set_headers` values support the same `{env:NAME}` / `{file:PATH}` placeholders as `auth.token`. |
| `drop_fields` / `set_fields` | Top-level body keys removed, or set (templates allowed). |
| `drop_tool_fields` | Body keys removed inside each tool. |
| `sanitize` | Requests relaxed for strict upstreams: tool schema rewrites, `metadata.user_id`, tool-use ids and inline tool definitions. |
| `count_tokens_model` | Substitute model for `/v1/messages/count_tokens`. |
| `quota` | Optional URL and interval for quota scraping. |

Top-level fields: `listen`, `default_route`, `max_body_bytes` (default 64 MiB), `routes`,
`network` (`proxy`, `ca_file`). Routes are checked in order and the first match wins.

Environment variables:

| Variable | Effect |
|---|---|
| `LLM_ROUTER_CONFIG` | Config path. |
| `LLM_ROUTER_LISTEN` | Overrides `listen`. |
| `LLM_ROUTER_ADMIN_LISTEN` | Moves `/livez`, `/readyz`, `/metrics` to a second listener. |
| `LLM_ROUTER_CLIENT_TOKEN` | Requires `X-Llm-Router-Token` on API requests. |
| `LLM_ROUTER_DRAIN_DELAY` | Delay between readiness failure and listener close, default `5s`. |

Full reference, validation rules and examples: [docs/configuration.md](docs/configuration.md).
Routing and rewriting details: [docs/routing.md](docs/routing.md).

## Endpoints

| Path | Behavior |
|---|---|
| `GET /livez` | Returns `200` while the process runs. |
| `GET /readyz` | Returns `200` after startup, `503` during drain. |
| `GET /metrics` | Prometheus text exposition. |
| Everything else | Proxied to the selected route. |

With `LLM_ROUTER_ADMIN_LISTEN` set, those three paths are served only on the admin listener.
The API listener then requires the client token for every request, including probes.

## Metrics

| Metric | Labels |
|---|---|
| `llm_router_requests_total` | `route`, `model`, `code` |
| `llm_router_request_duration_seconds` | `route`, `model` |
| `llm_router_time_to_first_byte_seconds` | `route`, `model` |
| `llm_router_inflight_requests` | `route`, `model` |
| `llm_router_tokens_total` | `route`, `model`, `type` |
| `llm_router_upstream_errors_total` | `route` |
| `llm_router_upstream_quota` | `route`, `key` |
| `llm_router_quota_scrapes_total` | `route`, `result` |
| `llm_router_rejected_requests_total` | — |

`model` is the ID the client sent, before rewriting, or `none` when the request has no
model. Token counters are populated from `usage` blocks on `2xx` `/v1/messages` responses.
Provider accounting differs: do not add `input_tokens` and `cache_creation_input_tokens` for
Synthetic, and NanoGPT's `input_tokens` excludes cache reads.

Detailed semantics, bucket sizes and PromQL examples:
[docs/observability.md](docs/observability.md).

## Security

Without the client token, anything able to reach the port can spend your provider
credentials.

- Generate a long random value, for example `openssl rand -hex 32`.
- Set `LLM_ROUTER_CLIENT_TOKEN`.
- Set `LLM_ROUTER_ADMIN_LISTEN` so probes and scraping live on a separate, unauthenticated
  port.
- Expose only the API port through your gateway and rate-limit unauthenticated traffic there.

Token comparison uses SHA-256 digests and constant-time equality. The token header is removed
before forwarding. Routes in `bearer` or `x-api-key` mode replace the caller's credentials, so
the subscription credential never reaches them.

Deployment examples, Kubernetes manifests and shutdown behavior:
[docs/deployment.md](docs/deployment.md).

## Documentation

- [Architecture](ARCHITECTURE.md)
- [Configuration](docs/configuration.md)
- [Routing and transformation](docs/routing.md)
- [Observability](docs/observability.md)
- [Deployment](docs/deployment.md)
- [Development](docs/development.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Claude Code integration](docs/claude-code.md)

## Limitations

- Claude Code runs in gateway mode for the whole session when the base URL is not
  Anthropic's. Some first-party-only behavior may be disabled for the main loop too.
- Claude Code does not know the context window of `hf:`, `syn:` or `nano:` models.
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS` applies globally and would also affect first-party models.
- Delegated quality depends on the model. Anthropic does not support Claude Code against
  non-Anthropic models.
- The router serves HTTP/1.1 and keeps no state. Run `cargo test` for the covered behavior.

## License

MIT. See [LICENSE](LICENSE).
