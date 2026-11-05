# Claude Code integration

Claude Code has a single `ANTHROPIC_BASE_URL` per process. Pointing it at the router lets one
session route requests to different Anthropic-compatible backends by model ID.

## Point Claude Code at the router

Keep the claude.ai login and set only the base URL:

```bash
claude-mix() {
  ANTHROPIC_BASE_URL=http://127.0.0.1:8787 \
  CLAUDE_CODE_ATTRIBUTION_HEADER=0 \
  claude "$@"
}
```

Do **not** set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`: when only `ANTHROPIC_BASE_URL` is
set, the saved subscription login remains the active credential and is forwarded to the
`passthrough` route (for example `anthropic`).

Persist it in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787",
    "CLAUDE_CODE_ATTRIBUTION_HEADER": "0"
  }
}
```

`CLAUDE_CODE_ATTRIBUTION_HEADER=0` keeps the system prompt prefix byte-stable, which Synthetic
recommends to avoid cache busting.

If the router requires a client token:

```bash
export ANTHROPIC_CUSTOM_HEADERS="X-Llm-Router-Token: <token>"
```

With a missing or wrong token the router drops the connection, so Claude Code reports a
connection error instead of a `401`.

## Subagents on Synthetic

Define agents with a Synthetic model in the frontmatter. The main loop keeps using Opus and
picks the agent from its `description`. Three tiers are provided in
[`examples/agents/`](../examples/agents):

| Agent | Model | Use |
|---|---|---|
| `worker-small` | `hf:zai-org/GLM-5.3-Flash` | Exploration, lookups, summaries, trivial edits (~0.1 request each). |
| `worker` (default) | `hf:deepseek-ai/DeepSeek-V4.1-Flash` | Default for scoped code changes and tests. |
| `worker-big` | `hf:moonshotai/Kimi-K3` | Hard refactors, debugging, image input (1 request each). |

```yaml
---
name: worker
description: Default implementation worker (DeepSeek-V4.1-Flash) for well-scoped coding tasks ...
model: hf:deepseek-ai/DeepSeek-V4.1-Flash
tools: Read, Grep, Glob, Bash, Edit, Write
---
```

Copy them to `~/.claude/agents/` or `<project>/.claude/agents/`. Claude Code accepts arbitrary
model IDs in agent frontmatter: it logs an `unrecognized_model` warning and sends the ID
verbatim.

Keep the `tools:` allowlist. Without it a subagent inherits every tool including all MCP
servers, and those schemas are resent on every request.

Synthetic-specific tips:

- Concurrency is one request per model; extra requests queue. The three tiers use different
  models, so they run in parallel while two `worker` agents queue. Watch
  `llm_router_time_to_first_byte_seconds` to spot queueing.
- Requests are scaled by model input price: Kimi-K3 counts as 1 request, GLM-5.3-Flash as
  ~0.1.
- The agents pin `hf:` IDs. Synthetic rotates models, so a pinned ID returns `404` once
  removed; `syn:` aliases track replacements automatically.
- `count_tokens` and `/v2/quotas` calls do not count against limits.
- Synthetic's `count_tokens` returns `500` for some models. Set `count_tokens_model` to a
  model it supports (the example config uses DeepSeek).

## Subagents on NanoGPT

NanoGPT model IDs have no distinctive prefix, so the example route claims `nano:` and strips
it:

```json
{
  "name": "nanogpt",
  "upstream": "https://nano-gpt.com/api",
  "models": ["nano:*"],
  "strip_prefix": "nano:",
  "auth": { "mode": "bearer", "token": "{env:NANOGPT_API_KEY}" }
}
```

In agent frontmatter use `model: nano:<NanoGPT ID>`, for example
`nano:deepseek/deepseek-v4.1-flash`, `nano:z-ai/glm-5.3-flash` or `nano:moonshotai/kimi-k3`.
Suffixes pass through unchanged; IDs are case-sensitive.

Notes:

- The upstream is `https://nano-gpt.com/api` (Messages at `/api/v1/messages`).
- `count_tokens` on NanoGPT is a generic estimate that ignores the model, so `/context` is
  approximate for `nano:` models.
- Non-Claude models are converted to OpenAI chat format upstream; tool use and thinking
  fidelity depend on the model.
- A subscription covers open models only. Add
  `"quota": { "url": "https://nano-gpt.com/api/subscription/v1/usage" }` to export usage
  gauges.

## Verifying the setup

1. `curl -s localhost:8787/readyz` — router is up.
2. Start a session with `ANTHROPIC_BASE_URL` set and ask for a trivial response.
3. Check `llm_router_requests_total` for the expected `route` and `model`.
4. Delegate a small task to a subagent and look for `agent_id` in the access log line.
5. For SSE behavior, confirm tokens appear under `llm_router_tokens_total` after the response
   finishes.

## Caveats

- Anthropic does not support Claude Code on non-Claude models; subagent quality depends
  entirely on the model.
- Because `ANTHROPIC_BASE_URL` points to a non-Anthropic host, Claude Code runs in gateway mode
  for the whole session, including the Opus loop.
- Claude Code does not know the context window of `hf:` / `syn:` / `nano:` IDs. Check with
  `/context`. `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is global and would also affect Opus.
- If a specific beta field causes upstream `400`s, drop it per route with `drop_fields` or
  `drop_tool_fields` rather than changing the client.
- DeepSeek rejects the `\0` escape in tool schema patterns (`400 ... is not a "regex"`);
  kimi-k3 on Bedrock rejects both `\0` and lookaround (empty stream). Set `sanitize` on
  that route:

  ```json
  "sanitize": {
    "tool_schema": { "normalize_nul_escape": true, "strip_lookaround": true },
    "drop_metadata_user_id": true,
    "sanitize_tool_use_ids": true
  }
  ```

  `drop_metadata_user_id` covers the separate DeepSeek `400 Invalid 'user_id'` error: Claude
  Code sends a `user_id` that does not match `^[a-zA-Z0-9_-]+$`. `sanitize_tool_use_ids`
  keeps history usable when ids produced by another backend (`functions.Bash:0`) are replayed
  to Anthropic.
