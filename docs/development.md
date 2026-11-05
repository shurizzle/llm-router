# Development

## Requirements

- Rust 1.99 (edition 2024).
- Docker, only for image builds.
- Provider credentials, only for manual end-to-end runs.

## Commands

```bash
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo run -- --config config.example.json
./target/debug/llm-router --healthcheck --config config.example.json
./target/debug/llm-router --check-config --config config.example.json
cargo build --release
```

CI runs the same formatting, lint and test commands on every push and pull request. Release
tags additionally build and push the multi-arch image.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI, listeners, connection serving, signals, reload, drain, probes |
| `src/config.rs` | Config structs, validation, glob matching, duration parser |
| `src/proxy.rs` | Routing, transformation, forwarding, streaming wrapper, quota scraping |
| `src/metrics.rs` | Prometheus registry, usage sniffer, quota flattening |
| `src/log.rs` | JSON logging helpers |
| `src/raw.rs` | Order-preserving JSON object with raw values |
| `src/ids.rs` | Deterministic tool-use id rewriting |
| `src/network.rs` | Egress connector (direct or CONNECT proxy) and TLS roots |
| `src/schema.rs` | Tool input-schema relaxation: pattern rewrites and format allowlist |
| `tests/cli.rs` | Process-level tests: spawns the binary against temporary config files |
| `docs/` | User and operator documentation |

## Tests

`cargo test` runs 65 tests:

- `config`: glob semantics, duration parsing, validation errors, `sanitize` parsing, and a
  drift check that regenerates `config.schema.json` from the Rust types.
- `metrics`: SSE usage across chunk boundaries, latest-field-wins behavior, JSON usage split
  across chunks, quota flattening.
- `proxy`: routing and credential swap, `count_tokens_model`, `strip_prefix`, upstream error
  passthrough, client-token rejection, unbuffered streaming, disabled routes, admin listener
  separation, metrics end to end, quota polling, `sanitize.tool_schema` rewriting,
  `drop_metadata_user_id`, byte passthrough when nothing applies.
- `schema`: NUL-escape and lookaround rewrite tables, nested schema walk, instance-data
  exclusions, format allowlist.
- `raw`: key-order and raw-value preservation, insert/remove semantics.
- `ids`: id rewrite table, determinism, length bound, `messages` walk.
- `network`: proxy URL parsing and Basic auth, direct mode, CA loading and error paths, and an
  end-to-end request through an in-test CONNECT proxy.
- `tests/cli.rs`: config rejection, `--check-config`, `LLM_ROUTER_LISTEN` override, admin
  listener probes, `--healthcheck` against a running and a stopped server, `SIGTERM` drain,
  `SIGHUP` reload against two real upstreams including a failed reload, and a config file
  change reload with no signal.

The `config`, `metrics` and `proxy` modules start real HTTP servers on ephemeral loopback
ports: a mock upstream and the router handler. They exercise real sockets, headers and
streaming, not mocked calls. `tests/cli.rs` spawns the built binary, so it covers argument
parsing, config loading, environment overrides and listener setup end to end. Tests kill the
child process with `SIGKILL`; graceful shutdown is not covered.

Tests that mutate environment variables use `unsafe { std::env::set_var }` because edition
2024 marks them unsafe. Use a distinct variable per test: Rust tests run in parallel, and two
tests sharing one env var race.

`config.schema.json` is generated with `schemars` from the `config.rs` types, under
`#[cfg_attr(test, ...)]` so the dependency stays a dev-dependency. After changing a config
struct, regenerate it with `UPDATE_SCHEMA=1 cargo test config_schema_is_current` and commit
the result. The test fails when the committed file is stale.

## Dependencies

| Crate | Why |
|---|---|
| `tokio` | Async runtime, signals, timers, channels |
| `clap` | Command-line flags, help and version |
| `hyper`, `hyper-util` | HTTP/1.1 server and client; `client-proxy` for CONNECT tunnels |
| `hyper-rustls`, `rustls`, `rustls-native-certs`, `rustls-pemfile` | HTTPS upstreams: native roots plus an optional extra CA bundle |
| `tower-service` | Service trait implemented by the custom egress connector |
| `base64` | Basic auth on the proxy CONNECT request |
| `http-body-util` | Body combinators and boxing |
| `prometheus` | Metric types and text exposition; process collector on Linux |
| `serde`, `serde_json` | Config parsing and raw-value body transformation |
| `chrono` | RFC 3339 parsing for quota timestamps |
| `url` | Upstream URL parsing and normalization |
| `sha2`, `subtle` | Constant-time client-token comparison |
| `parking_lot` | Mutex for the quota key bookkeeping |
| `bytes` | Body buffers |

Add a dependency only when the standard library or an existing crate cannot do the job.

## Changing behavior

Public behavior is the project's contract. Before changing headers, status codes, error
bodies, metric names or labels, config field names, log fields, or endpoints:

1. Confirm the change is intended; record any new implementation limit in `ARCHITECTURE.md`
   under "Implementation notes and limits".
2. Update the affected document in `docs/` and, if user-facing, `README.md`.
3. Add or extend a test that pins the new behavior.
4. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.

## Adding a config option

1. Add the field to the matching struct in `src/config.rs` with a `serde` default, so existing
   files keep working. Unknown fields are rejected, so the name is part of the public API.
2. Validate it in `Config::validate` and add the error to the joined list.
3. Apply it in `Router::new`, `Route::transform_body`, or the header pipeline in
   `src/proxy.rs`.
4. Document it in `docs/configuration.md` and, when routing-related, `docs/routing.md`.
5. Add a test: validation error for bad input, and observable forwarding behavior for good
   input.

## Debugging a request

1. Check the access log line: `route`, `status`, `ttfb_ms`, token fields.
2. Compare with `llm_router_requests_total` and `llm_router_upstream_errors_total`.
3. For a transformation problem, run the mock-upstream tests or point a route at a local echo
   server and inspect the received path, headers and body.
4. For TLS or DNS problems, `/readyz` stays healthy: it deliberately does not probe upstreams,
   so the first request surfaces the error as `502` plus an `upstream error` log line.

## Release

Releases are built from semver tags (`vX.Y.Z`, or `vX.Y.Z-rc.N` for pre-releases). The workflow
tests, builds and pushes a multi-arch image, then creates the GitHub release.

```bash
git tag -a v0.2.1 -m v0.2.1 && git push origin v0.2.1
```

Do not tag or push without explicit approval.
