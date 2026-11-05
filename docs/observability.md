# Observability

The router exposes Prometheus metrics, writes one JSON access log line per proxied request,
and logs lifecycle events (startup, disabled routes, drain) as JSON.

## Endpoints

| Path | Method | Description |
|---|---|---|
| `/livez` | GET | `200 ok` while the process serves HTTP. |
| `/readyz` | GET | `200 ok` once listeners are up; `503` from `SIGTERM` until shutdown. Does not probe upstreams. |
| `/metrics` | GET | Prometheus text exposition. |

With `LLM_ROUTER_ADMIN_LISTEN` set these endpoints move to that listener, and the API listener
answers nothing without the client token. Without it, they are served on the API listener and
are exempt from the client token.

## Metrics

| Metric | Type | Labels | Notes |
|---|---|---|---|
| `llm_router_requests_total` | counter | `route`, `model`, `code` | One per proxied request, after the response body completes. `code` is the upstream status; `502` for transport errors. |
| `llm_router_request_duration_seconds` | histogram | `route`, `model` | Full duration, including the streamed response body. |
| `llm_router_time_to_first_byte_seconds` | histogram | `route`, `model` | Time until upstream response headers; includes upstream queueing. |
| `llm_router_inflight_requests` | gauge | `route`, `model` | Requests currently being proxied. |
| `llm_router_tokens_total` | counter | `route`, `model`, `type` | `type` is `input`, `output`, `cache_read` or `cache_creation`. |
| `llm_router_upstream_errors_total` | counter | `route` | Transport-level failures reaching the upstream. |
| `llm_router_upstream_quota` | gauge | `route`, `key` | Numeric leaves of the quota response, flattened. |
| `llm_router_quota_scrapes_total` | counter | `route`, `result` | `result` is `success` or `error`. |
| `llm_router_rejected_requests_total` | counter | — | Requests dropped for a missing or wrong client token. |

Histogram buckets:

- duration: `0.25 × 2ⁿ` for `n = 0..12` (`0.25` … `1024` seconds), plus `+Inf`.
- time to first byte: `0.05 × 2ⁿ` for `n = 0..13` (`0.05` … `409.6` seconds), plus `+Inf`.

On Linux the process collector adds `process_*` series. There are no language-runtime
collectors.

### Label semantics

- `route` is the route name from the config.
- `model` is the model ID exactly as the client sent it, before rewrite. Requests with no
  model (non-messages traffic, or a missing field) use `none`.
- `code` is the response status sent to the client for proxy outcomes. Router-generated
  early rejections (`400`, `413`, `503` disabled route) are not counted in
  `llm_router_requests_total`.

### Token accounting

Usage is parsed from `usage` blocks of `2xx` `/v1/messages` responses. Both SSE and JSON
bodies are supported:

- SSE: every complete `data:` line containing `"usage"` is parsed as it streams; the latest
  event that carries a field wins, because Anthropic stream usage is cumulative.
- JSON: the body is buffered up to 4 MiB and parsed when the response ends.

Only non-zero values increment counters. Values are exported exactly as the upstream reports
them; the router never adds or subtracts them.

Provider differences worth knowing:

- Synthetic reports `input_tokens` equal to `cache_creation_input_tokens` (the uncached part
  is counted in both). Do not sum the two for Synthetic.
- NanoGPT's `input_tokens` excludes `cache_read_input_tokens`.

Sniffing is disabled when the response is non-`2xx` or carries a `Content-Encoding` header,
which is why the router forces `Accept-Encoding: identity` on exact `/v1/messages`.

## Quota metrics

For each route with `quota.url`, the response JSON is flattened:

- objects → dotted keys (`subscription.limit`),
- arrays → numeric index segments (`items.0.value`),
- numbers → exported as floats,
- booleans → `0` or `1`,
- strings parsed as RFC 3339 → Unix seconds (fractional seconds accepted),
- other strings and `null` → ignored.

Each successful scrape replaces the route's gauge series, so removed keys disappear.

Synthetic's `/v2/quotas` currently exposes keys such as `subscription.limit`,
`subscription.requests`, `rollingFiveHourLimit.remaining`, `rollingFiveHourLimit.limited`,
`weeklyTokenLimit.percentRemaining` and `weeklyTokenLimit.nextRegenAt`. The schema is
undocumented and may change.

## PromQL examples

```promql
# Cache hit ratio per route
sum by (route) (rate(llm_router_tokens_total{type="cache_read"}[1h]))
  / sum by (route) (rate(llm_router_tokens_total{type=~"input|cache_read|cache_creation"}[1h]))

# Remaining 5-hour request budget for a Synthetic route
llm_router_upstream_quota{route="synthetic",key="rollingFiveHourLimit.remaining"}

# p90 time to first byte per model (proxy queueing and upstream latency)
histogram_quantile(0.9,
  sum by (le, model) (rate(llm_router_time_to_first_byte_seconds_bucket[15m])))

# Requests per status per route
sum by (route, code) (rate(llm_router_requests_total[5m]))

# Requests rejected for a missing or wrong client token
rate(llm_router_rejected_requests_total[5m])
```

## Access log

One line per proxied request on stdout:

```json
{"time":"2026-11-05T15:46:42.421756000+00:00","level":"INFO","msg":"request",
 "method":"POST","path":"/v1/messages","route":"synthetic",
 "model":"hf:moonshotai/Kimi-K3","status":200,"ttfb_ms":812,"duration_ms":4321,
 "input_tokens":2100,"output_tokens":450,"cache_read_tokens":18000,
 "cache_creation_tokens":0,"agent_id":"a1b2c3"}
```

Fields:

| Field | Meaning |
|---|---|
| `method`, `path` | Incoming request method and path (query excluded). |
| `route` | Selected route name. |
| `model` | Raw model from the request body; empty for non-messages traffic. |
| `status` | Status sent to the client. |
| `ttfb_ms` | Milliseconds until upstream response headers. |
| `duration_ms` | Milliseconds until the response body completed. |
| `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens` | Sniffed usage, zeros when unavailable. |
| `agent_id` | `X-Claude-Code-Agent-Id` value, set by Claude Code on subagent requests; empty otherwise. |

Lifecycle events use the same JSON shape with different `msg` values:

- `listening` (`addr`, `admin_addr`, `default_route`, `routes`, `client_token`),
- `route disabled: credential is empty` (`route`),
- `request rejected: route disabled` (`route`, `model`),
- `upstream error` (`route`, `path`, `err`),
- `quota poll failed` (`route`, `err`),
- `draining` (`delay`), `stopped`, `fatal` (`err`).

The client token is never logged; `client_token` in the startup line is a boolean.

## Scrape configuration

The exposition uses the Prometheus text format (`text/plain; version=0.0.4`). Scrape
`/metrics` on the API port when no admin listener is configured, or on
`LLM_ROUTER_ADMIN_LISTEN` when it is. With the admin listener set, the API port requires the
client token, which scrapers do not have.
