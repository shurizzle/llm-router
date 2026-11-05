# AGENTS.md

Repository guidance for AI coding agents working on `llm-router`.

## What this project is

A small reverse proxy that routes Anthropic Messages API requests to different
Anthropic-compatible backends based on the request `model` field. It performs no protocol
translation: it selects an upstream, swaps credentials, optionally rewrites the model ID and
drops headers or body fields, then streams the response back unbuffered.

**Public behavior is the primary requirement.** A change that alters routing, forwarded
headers, body transformation, status codes, error bodies, metric names or labels, log fields,
or endpoints is a breaking change for clients and dashboards.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI flags, listeners, HTTP/1 serving, signals, SIGHUP reload, graceful drain |
| `src/config.rs` | Config deserialization, validation, glob matching, duration parsing |
| `src/proxy.rs` | Router, per-route transformation, proxying, usage sniffing, quota polling |
| `src/raw.rs` | Order-preserving JSON object with raw values |
| `src/ids.rs` | Deterministic tool-use id rewriting |
| `src/network.rs` | Egress connector (direct or CONNECT proxy) and TLS roots |
| `src/schema.rs` | Tool input-schema relaxation applied before forwarding |
| `src/metrics.rs` | Prometheus registry, usage sniffer, quota JSON flattening |
| `src/log.rs` | JSON log lines on stdout |
| `docs/` | Detailed documentation |
| `ARCHITECTURE.md` | Internal design and request lifecycle |
| `config.example.json` | Reference configuration |
| `config.schema.json` | Generated JSON Schema for the configuration; drift-checked by `cargo test` |
| `examples/agents/` | Claude Code subagent definitions |

## Commands

```bash
cargo build                 # debug build
cargo test                  # unit + integration tests
cargo fmt --check           # CI formatting gate
cargo clippy --all-targets -- -D warnings
cargo run -- --config config.example.json
./target/debug/llm-router --healthcheck --config <path>
```

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`
on every push and pull request; release tags also build and push the image.

## Rules

- Write code, comments, commit messages, and documentation in English.
- Keep dependencies minimal. `hyper`, `hyper-rustls`, `prometheus`, `serde`, `tokio` and
  `parking_lot` cover the current needs; justify anything new.
- Preserve public behavior: metric names, labels and help text, JSON error shape, headers,
  environment variable names, and config field names are a contract.
- When behavior changes, update `README.md`, the relevant file in `docs/`, and add or adjust
  tests in the same change.
- Configuration parsing rejects unknown fields on purpose. Do not relax that.
- New non-trivial logic (routing, transformation, parsing, token accounting) needs a test.
- Do not commit, amend, push, tag, or rewrite history unless the user explicitly asks.
- Do not write client or customer names into configuration files, documentation, or tests.

## Implementation limits

Known implementation limits are documented in `ARCHITECTURE.md` ("Implementation notes and
limits"). Do not change them silently.
