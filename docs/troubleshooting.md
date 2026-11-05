# Troubleshooting

Start with the access log line for the failing request and the related metric series. Most
problems are visible in `route`, `status`, `ttfb_ms`, and the token fields.

## Quick table

| Symptom | Likely cause | Fix |
|---|---|---|
| Client reports a connection error, no HTTP status at all | Client token missing or wrong | Send `X-Llm-Router-Token` with the configured value |
| `503` with `x-should-retry: false` | Route disabled: its credential is empty | Fix the variable or secret file and restart |
| `502` with `llm-router: upstream "..."` | DNS, TLS, connection refused, upstream unreachable | Check network, CA certificates, upstream URL |
| `413` | Body larger than `max_body_bytes` (default 64 MiB) | Raise the limit if the upstream accepts it |
| `400 llm-router: invalid JSON body` | Client sent malformed JSON or a non-string `model` | Fix the client request |
| `400 llm-router: transform body` | `drop_tool_fields` applied to a `tools` value that is not an array of objects, or an unparsable `input_schema` | Inspect the request; drop the option or fix the payload |
| Upstream `400 ... is not a "regex"`, or `200` with an empty stream | Upstream rejects a tool schema pattern (`\0`, lookaround) or format | Set `sanitize.tool_schema` on the route (`normalize_nul_escape`, `strip_lookaround`, `drop_pattern`, `string_formats`) |
| Upstream `400 Invalid 'user_id': string does not match pattern` | Upstream validates `metadata.user_id` | Set `sanitize.drop_metadata_user_id: true` on that route, or `strip_metadata_user_id` in the client |
| Upstream `400` on replayed history mentioning `tool_use.id` | History comes from a backend whose ids do not match `^[a-zA-Z0-9_-]+$` | Set `sanitize.sanitize_tool_use_ids: true` on the route that reaches Anthropic |
| `502 ... proxy CONNECT` or all upstreams unreachable | Proxy down, wrong host/port, or missing credentials | Check `network.proxy`; only `http://` URLs are supported, Basic auth comes from the URL userinfo |
| Startup: `network.ca_file ...` | Missing, unreadable, or certificate-free PEM bundle | Fix the path (relative paths resolve against the config file) or the file contents |
| Startup exits with `invalid config ...` | Validation error, joined by newlines | Read every listed error; all must be fixed |
| `/readyz` returns `503` | Process is draining, or listeners are not up yet | Expected during shutdown; wait or restart |
| Token metrics stay at zero | Non-`2xx` response, compressed response, or path is not exact `/v1/messages` | Check `status` and `Content-Encoding`; usage is only sniffed on exact `/v1/messages` 2xx |
| Quota metric missing | Scrape failed, route disabled, or key removed upstream | Check `llm_router_quota_scrapes_total{result="error"}` and the `quota poll failed` log |
| High `ttfb_ms`, normal `duration_ms` | Upstream queueing (e.g. per-model concurrency limits) | Read provider limits; use distinct models for parallel work |
| Config changes do nothing | No hot reload triggered, or the reload failed | Check for a `reloaded` line after saving; send `SIGHUP` if the `watch` feature is disabled. A `reload failed` line means the old config is still active. Validate with `--check-config` |
| Startup: `listen ... Address already in use` | Another process owns the port | Stop it or change `listen` |

## Connection reset instead of a response

When `LLM_ROUTER_CLIENT_TOKEN` is set, a missing or wrong `X-Llm-Router-Token` makes the
router abort the connection without writing a response. `llm_router_rejected_requests_total`
increments; there is no access log line for rejected requests and nothing reaches the
upstream. This is deliberate: scanners and brute-forcers get no fingerprint.

Claude Code shows this as a connection error. Set:

```bash
export ANTHROPIC_CUSTOM_HEADERS="X-Llm-Router-Token: <token>"
```

With `LLM_ROUTER_ADMIN_LISTEN` set, the same applies to `GET /livez` and `GET /metrics` on the
API port. Use the admin port for probes and scraping.

## Disabled route

```
llm-router: route "nanogpt" is disabled: auth.token is empty
```

The route has `auth.mode` `bearer` or `x-api-key` and its `token` template expanded to an empty
string at startup. Credentials are read once at startup; setting the variable or secret file
later has no effect until restart. The default route must have a credential, so validation also rejects an empty one at
startup.

## Upstream failures

`502` means the router could not obtain a response:

- DNS failure for the upstream host,
- TLS verification failure (missing CA store, clock skew, interception),
- connection refused or timed out,
- the upstream URL is not reachable from the router's network namespace.

Each failure logs `upstream error` with `route`, `path` and `err`, and increments
`llm_router_upstream_errors_total{route}`. `/readyz` deliberately stays healthy: it does not
probe upstreams, to avoid cascading failures.

## Body limit

The limit applies only to messages paths. `413` returns an Anthropic-shaped error. Bodies on
other paths are streamed without inspection and without this limit.

## Token accounting looks wrong

- Usage is only read on exact `POST /v1/messages` with a `2xx` response and no
  `Content-Encoding`. The router forces `Accept-Encoding: identity` there, but a proxy in
  front that compresses responses breaks sniffing.
- `/v1/messages/count_tokens` is not sniffed.
- For SSE, the latest event carrying a field wins. Some providers resend the final input and
  cache split in `message_delta`; others do not.
- Synthetic reports `input_tokens` equal to `cache_creation_input_tokens`; do not add them.
  NanoGPT's `input_tokens` excludes `cache_read_input_tokens`.
- `count_tokens` accuracy depends on the model used upstream. If an upstream returns `500`
  for token counting on some models, set `count_tokens_model` to a model it supports.

## Quota polling

- Quota requires a route credential (`bearer` or `x-api-key`); `passthrough` is rejected at
  validation time.
- Each scrape has a 10-second timeout; failures increment
  `llm_router_quota_scrapes_total{result="error"}` and log the upstream error text.
- A successful scrape replaces all gauge series for the route, so keys that disappear from the
  response are removed rather than left stale.
- Multiple replicas each poll independently.

## Startup validation errors

Example:

```
invalid config /etc/llm-router/config.json: route "a": upstream must be an absolute http(s) URL
default_route "missing" does not exist
```

All errors are reported together. Common causes:

- unknown field name (`parse ...: unknown field ...`),
- `interval` written as a number instead of a duration string,
- duplicate route names,
- `auth.mode` typo,
- `token` omitted for `bearer` / `x-api-key`,
- quota configured on a `passthrough` route,
- default route credential expands to an empty string,
- unreadable `{file:...}` path or unknown placeholder kind.

## Graceful shutdown

On `SIGTERM`:

1. `/readyz` returns `503` immediately.
2. The process sleeps `LLM_ROUTER_DRAIN_DELAY` (default `5s`).
3. Listeners close; in-flight requests get up to 60 seconds.
4. The process exits.

If Kubernetes kills the pod before long streamed requests finish, raise
`terminationGracePeriodSeconds` above the drain delay plus the expected request duration.

## Getting more detail

The router has no verbose mode: every lifecycle event and every proxied request is already
logged as JSON. For a non-routable problem, reproduce with a local echo upstream and compare
the request the router sends (path, headers, body) with the client request.
