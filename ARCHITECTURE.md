# Architecture

`llm-router` is a reverse proxy for the Anthropic Messages API. A client (typically Claude
Code) sends every request to one base URL; the router picks the upstream from the request
`model` field and forwards the request almost verbatim.

There is no protocol translation. Every upstream is expected to speak the Anthropic Messages
API, so the router only:

1. selects a route,
2. replaces or passes through credentials,
3. optionally rewrites the model ID and drops unsupported headers or body fields,
4. streams the upstream response back without buffering.

Detailed references:

- [Configuration](docs/configuration.md)
- [Routing and transformation](docs/routing.md)
- [Observability](docs/observability.md)
- [Deployment](docs/deployment.md)
- [Development](docs/development.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Claude Code integration](docs/claude-code.md)

## Process model

```
                 ┌──────────────────────────── API listener (:8787) ───────────────────────────┐
 claude ────────▶│ Handler{probes, proxy} ──▶ client token ──▶ Router::proxy ──▶ hyper client   │──▶ upstream
                 └──────────────────────────────────────────────────────────────────────────────┘
                 ┌───────────── admin listener (optional, LLM_ROUTER_ADMIN_LISTEN) ─────────────┐
 probes/scraper ▶│ Handler{probes, !proxy}: /livez, /readyz, /metrics, 404 otherwise            │
                 └──────────────────────────────────────────────────────────────────────────────┘
```

- One `tokio` multi-thread runtime.
- One HTTP/1.1 listener for the API, plus an optional second listener for probes and metrics.
- One task per accepted connection, each serving keep-alive requests through `hyper::server::conn::http1`.
- One background task per route with `quota.url`.
- Shared state behind `Arc`: `Metrics`, ready flag, and the router in a `parking_lot::RwLock`
  that a reload (`SIGHUP` or config file change) swaps. `Handler::call` reads the current router per request, so keep-alive
  connections pick up a reload.
- The other shared mutable lock is `Metrics::quota_keys` (`parking_lot::Mutex`), which tracks
  the quota label values currently exported per route so stale keys can be removed.

## Modules

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI parsing, environment overrides, listener setup, quota task spawning, signals, config file watch and reload, graceful drain, `--healthcheck` and `--check-config` probes |
| `src/config.rs` | `serde` deserialization (`deny_unknown_fields`), validation, duration parsing, `glob_match` |
| `src/proxy.rs` | `Router`, `Route`, `Upstream`, request handling, header/body transformation, response streaming, usage sniffing, quota scraping |
| `src/raw.rs` | Order-preserving JSON object with raw values, used for body edits |
| `src/ids.rs` | Deterministic rewriting of Anthropic tool-use ids |
| `src/network.rs` | Egress connector (direct or HTTP CONNECT proxy) and TLS root store |
| `src/schema.rs` | Tool input-schema relaxation: pattern rewrites and format allowlist |
| `src/metrics.rs` | Prometheus registry and metric definitions, `UsageSniffer`, quota JSON flattening |
| `src/log.rs` | One JSON object per line on stdout |

Key types:

- `Router { routes, fallback, token_sum, max_body, metrics, client }` — immutable after startup.
- `Route { cfg, token, disabled, upstream }` — one configured upstream. `disabled` is set at
  startup when the credential template expands to an empty string for `bearer`/`x-api-key`.
- `Upstream { origin, authority, base_path, base_query }` — parsed once from `routes[].upstream`.
- `Handler { router, metrics, ready, probes, proxy_enabled }` — dispatch decisions for a listener.
- `TailBody` — wraps the upstream body stream, feeds the usage sniffer, and finalizes request
  metrics when the body ends or the client disconnects.

## Request lifecycle

For every request on a proxying listener:

1. **Probe dispatch** (`Handler::call`). If the listener serves probes and the method is `GET`:
   `/livez` → `200 ok`, `/readyz` → `200 ok` or `503 not ready` during drain, `/metrics` →
   Prometheus exposition. These endpoints are exempt from the client token.
2. **Admin-only listener**: any other path gets `404`. This listener never proxies.
3. **Client token** (optional). The `X-Llm-Router-Token` header is hashed with SHA-256 and
   compared in constant time against the configured token digest. A missing or wrong token
   increments `llm_router_rejected_requests_total` and aborts the connection with **no HTTP
   response** — the service returns an error, so `hyper` closes the stream without writing
   status or headers. A valid token header is stripped before forwarding.
4. **Route selection.**
   - For `POST` requests whose path starts with `/v1/messages`, the body is read with a limit
     of `max_body_bytes` (default 64 MiB). Oversize → `413`; unreadable body → `400`;
     invalid JSON or non-string `model` → `400`; `null`/missing model → empty string.
   - The model is matched against routes in configuration order. `rewrite` keys match first,
     then `models` globs (`*` matches any sequence, including `/`). No match → `default_route`.
   - Any other request (other methods, other paths) goes to `default_route` and its body is
     streamed through untouched.
5. **Disabled route check.** A route whose credential is empty answers
   `503` with `x-should-retry: false`.
6. **Body transformation** (messages paths only). Model rewrite, `strip_prefix`,
   `count_tokens_model`, `drop_fields`, `drop_tool_fields`, `sanitize`. If nothing applies, the
   original bytes are forwarded unchanged.
7. **Outbound request.** The path is appended to the route upstream path; the incoming query
   is preserved and combined with any query on the upstream URL. Hop-by-hop headers are
   removed, the client token is removed, credentials are swapped according to `auth.mode`,
   then `drop_headers` and `set_headers` apply (values expanded once at startup). `Host` is set to the upstream authority.
   On exact `/v1/messages`, `Accept-Encoding: identity` is forced so the response can be
   sniffed for usage.
8. **Upstream call.** Transport failures log `upstream error`, increment
   `llm_router_upstream_errors_total`, record request metrics with status `502`, and return
   an Anthropic-shaped JSON error.
9. **Response.** The upstream status is preserved. Response hop-by-hop headers are removed.
   The body is streamed to the client frame by frame.
10. **Finalization.** When the response body reaches EOF (or is dropped because the client
    disconnected), `TailBody` records token usage, duration, time to first byte, the request
    counter, decrements in-flight, and writes the access log line.

Only requests that reached the upstream call produce request metrics. Early rejections
(`400`, `413`, `503` for disabled routes) and probe responses do not.

## Routing

```rust
fn matches(&self, model: &str) -> bool {
    self.cfg.rewrite.contains_key(model)
        || self.cfg.models.iter().any(|p| glob_match(p, model))
}
```

`glob_match` splits the pattern on `*`: the part before the first `*` must prefix the input,
interior parts must appear in order, and the last part must suffix the remainder. A pattern
without `*` is an exact comparison. `"hf:*"` therefore matches `hf:deepseek-ai/DeepSeek-V4.1-Flash`,
including the `/`.

## Body transformation

Transformation is required only when a model rewrite, a field change or sanitization is
configured. When required, the body is parsed into `RawObject` (`src/raw.rs`), an ordered list
of `(key, RawValue)`:

- untouched top-level values keep their original bytes (no float or escape normalization),
- keys keep their original order; a key set by the router keeps its position or is appended,
- `model` is replaced with a JSON-encoded string,
- `drop_fields` removes top-level keys and `set_fields` writes them after the drops,
- `sanitize.drop_metadata_user_id` removes `metadata.user_id`, keeping the rest of the object,
- `sanitize.sanitize_tool_use_ids` rewrites `tool_use.id` / `tool_result.tool_use_id` inside
  `messages` (`src/ids.rs`). Only `messages` is re-serialized, and only when an id changed,
- `sanitize.hoist_inline_tools` moves each `tool_addition` with a `tool_definition` into the
  top-level `tools` array and drops the block (`src/inline_tools.rs`). `tool_removal` blocks are
  dropped, and a message left with no content is removed. See the limit below,
- `drop_tool_fields` parses `tools` into `RawObject` entries and removes the listed keys per
  tool, preserving tool order and each tool's key order,
- `sanitize.tool_schema` parses each tool's `input_schema` and rewrites `pattern`/`format`
  keywords (`src/schema.rs`). Only a changed schema is re-serialized; other tools keep their
  bytes.

If parsing or re-serialization fails, the request is rejected with `400` before reaching the
upstream.

## Streaming and token accounting

`TailBody` owns the upstream body stream, an optional `UsageSniffer`, and the finalization
state. Each frame is forwarded to the client immediately after being inspected.

Usage sniffing is enabled only for exact `POST /v1/messages` responses that are `2xx` and have
no `Content-Encoding` header. Two shapes are supported:

- **SSE** (`Content-Type` starts with `text/event-stream`): complete lines are scanned as they
  arrive; only lines starting with `data:` that contain `"usage"` are parsed. Later events
  override fields they carry, because Anthropic stream usage is cumulative.
- **JSON**: the whole body is accumulated up to 4 MiB and parsed at EOF.

Four counters are exported when non-zero: `input`, `output`, `cache_read`, `cache_creation`.
Values are reported exactly as upstream sends them; no arithmetic is performed.

## Credentials and trust boundaries

- **Client token** (`LLM_ROUTER_CLIENT_TOKEN`) authenticates callers. Comparison is
  constant-time over SHA-256 digests. Failure closes the connection without a response, so
  scanners learn nothing about the service.
- **Route credentials** come from `auth.token`, a template expanded once at startup
  (`{env:NAME}`, `{file:PATH}`). `passthrough` forwards the caller's
  credentials untouched; `bearer` and `x-api-key` replace them, so the subscription OAuth
  token never reaches a route that replaces authentication.
- **Disabled routes** exist to let one configuration serve any subset of providers: a
  credential template that expands to empty disables a non-default route instead of failing
  startup. The default route must have a credential at startup or configuration validation
  fails.
- Probe and metrics endpoints are exempt from the client token only when served on the same
  listener; with `LLM_ROUTER_ADMIN_LISTEN` the API listener answers nothing without a token.

## Quota polling

For every route with `quota.url`, a background task scrapes immediately and then once per
`interval` (default `60s`):

- request uses the route credentials and a 10-second timeout,
- response must be JSON with status `200`,
- numeric leaves are flattened with dotted keys; array indices become key segments; booleans
  map to `0`/`1`; RFC 3339 strings map to Unix seconds,
- each scrape replaces the route's exported gauge series,
- outcomes increment `llm_router_quota_scrapes_total{result="success"|"error"}`.

## Shutdown

On `SIGTERM` or `Ctrl-C`:

1. `/readyz` starts returning `503` while the process is still serving.
2. The process sleeps `LLM_ROUTER_DRAIN_DELAY` (default `5s`) so load balancers remove it.
3. Listeners stop accepting; the admin listener is stopped together with the API listener.
4. In-flight connections get up to 60 seconds to finish, then their tasks are aborted.
5. Quota tasks are aborted.

## Reload

On `SIGHUP`, or when the watched config file changes on disk:


1. The config file is loaded, templates expanded and validated.
2. A new egress client is built from `network`, so proxy and CA changes apply.
3. `Router::new` builds the new routing table; the `RwLock` is replaced.
4. Quota tasks are aborted and respawned from the new routes.
5. `reloaded` is logged. On any error the previous router stays in place and `reload failed`
   is logged with the error.

Listeners, `LLM_ROUTER_ADMIN_LISTEN` and `LLM_ROUTER_DRAIN_DELAY` are startup-only. Every
request after the swap uses the new router, including requests on existing keep-alive
connections; requests already in flight finish on the old router. The client token is re-read
from the environment during reload.

## Error responses

Errors generated by the router use the Anthropic error envelope:

```json
{"type":"error","error":{"type":"api_error","message":"llm-router: ..."}}
```

| Status | Trigger | Extra headers |
|---|---|---|
| `400` | Unreadable body, invalid JSON, non-string model, body transformation failure | — |
| `413` | Body larger than `max_body_bytes` | — |
| `502` | Upstream URI build failure or transport error | — |
| `503` | Route disabled because its credential is empty | `x-should-retry: false` |

Upstream error responses are passed through byte for byte: Claude Code matches on their
wording when deciding whether to retry.

## Implementation notes and limits

- Only process-level runtime metrics are exported, and only on Linux; there are no
  language-runtime collectors.
- The upstream client uses HTTP/1.1 only (`enable_http1`).
- The API listener has no idle timeout. The header read timeout is 10 seconds.
- Only `GET` is recognized on `/livez`, `/readyz` and `/metrics`; a `HEAD` request is proxied
  like any other path.
- Quota responses are collected in full and the first 1 MiB is parsed.
- Access log objects have keys in sorted order (JSON object semantics), not insertion order.
- A rewritten `input_schema` loses its original key order and number spelling, because it is
  parsed into `serde_json::Value`. The output is deterministic, so prompt cache keys stay
  stable across turns.
- `sanitize` applies to every request on its route; there is no per-model gating inside
  one route.
- `sanitize.hoist_inline_tools` makes every hoisted tool available from the start of the
  request, because `tools` is static per call. A `tool_removal` has no equivalent and is
  dropped: a withdrawn tool stays available for the rest of that request.
- `messages` and `metadata` are parsed into `serde_json::Value` when they are rewritten, so
  their nested key order follows `serde_json` (sorted) rather than the client's order. The
  output is deterministic, so prompt cache keys stay stable across turns.
- `network.proxy` tunnels every upstream, including plain `http://` ones, through CONNECT.
  Only `http://` proxies are supported; an `https://` proxy is rejected at startup.
- A custom `ca_file` is loaded into the root store, but no test performs a TLS handshake
  against it; only the loading and error paths are covered.
- Reload replaces the whole router and client; in-flight upstream connections are not
  migrated, and a reload does not reset metrics. SIGHUP is unix-only; on other platforms only
  Ctrl-C and the config file watch trigger a reload. The watch (default Cargo feature `watch`)
  covers the config file only: `{file:...}` templates and `network.ca_file` are re-read on
  reload but their own changes do not trigger one.
