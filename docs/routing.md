# Routing and transformation

Routing answers one question: given an incoming request, which upstream receives it, and in
what modified form?

## What is inspected

- `POST` requests whose path starts with `/v1/messages` are treated as messages traffic. The
  router reads the body, extracts the `model` field, and uses it to select a route. The
  `max_body_bytes` limit applies here.
- Every other request — other methods, `/v1/models`, `/v1/messages/batches`, arbitrary paths
  — is sent to `default_route` without inspecting or rewriting the body.

On messages paths, an unreadable body returns `400`, an invalid JSON body returns `400`, and
a `model` that is present but not a string returns `400`. A missing or `null` model becomes
the empty string.

## Matching

Routes are evaluated in configuration order; the first match wins.

A route matches when:

1. the model is a key in the route's `rewrite` map, or
2. the model matches one of the route's `models` glob patterns.

Glob semantics: `*` matches any sequence of characters, including `/`. A pattern without `*`
is an exact comparison. Examples:

| Pattern | Model | Match |
|---|---|---|
| `hf:*` | `hf:deepseek-ai/DeepSeek-V4.1-Flash` | yes |
| `claude-*` | `claude-opus-5` | yes |
| `claude-*` | `hf:claude` | no |
| `*-flash` | `hf:zai-org/GLM-5.3-Flash` | yes |
| `*` | anything | yes |

If no route matches, the request goes to `default_route`. The same fallback handles non-model
traffic.

## Model rewriting

Three mechanisms can change the upstream model ID, in this order:

1. `rewrite[model]` — an exact map lookup. Its keys also act as route matches.
2. `strip_prefix` — when the model starts with the configured prefix and no rewrite matched,
   the prefix is removed. Example: `nano:z-ai/glm-5.3` → `z-ai/glm-5.3`.
3. `count_tokens_model` — only on the exact path `/v1/messages/count_tokens`. When set, it
   replaces the model computed above.

The route still needs a matching `models` glob (or a `rewrite` key) for `strip_prefix` to be
used, because matching happens before rewriting.

On every other path, including `/v1/messages`, `count_tokens_model` is not applied.

## Body field removal and injection

`drop_fields` removes top-level body keys on every `/v1/messages*` request. Typical use:
removing a beta field an upstream rejects (`context_management`, `metadata`, ...).

`set_fields` sets top-level body keys after the drops, so it wins over `drop_fields`. Values
use the same `{env:NAME}` / `{file:PATH}` templates as `auth.token`; the expanded text is
parsed as JSON, falling back to a JSON string:

| Template result | Sent |
|---|---|
| `1`, `1.5`, `true`, `null` | the matching JSON value |
| `"quoted"` | the string `quoted` |
| `hello` | the string `hello` |

`drop_tool_fields` removes keys from every entry of the `tools` array. Typical use:
`defer_loading`, which some Anthropic-compatible backends do not accept.

### Body sanitization

`sanitize.tool_schema` rewrites the `input_schema` of every tool. Use it when an upstream
rejects a JSON Schema keyword:

- `normalize_nul_escape` — `\0` → `\x00`. DeepSeek answers `400 "^[^\0]*$" is not a
  "regex"` without it.
- `strip_lookaround` — removes `(?=...)`, `(?!...)`, `(?<=...)`, `(?<!...)` and a quantifier
  attached to the removed group. kimi-k3 answers `200` with an empty stream without it.
- `drop_pattern` — removes the `pattern` keyword.
- `string_formats` — allowlist of string `format` values to keep.

The walk descends into nested schemas; `enum`, `const`, `default` and `examples` are instance
data and stay untouched. Only a changed tool is re-serialized, so prompts keep a stable byte
sequence across turns.

`sanitize.drop_metadata_user_id` removes only `metadata.user_id`; the rest of `metadata` is
forwarded. Use it when an upstream answers `400 Invalid 'user_id': string does not match
pattern`.

`sanitize.sanitize_tool_use_ids` rewrites `tool_use.id` and `tool_result.tool_use_id` in
`messages` to Anthropic's `^[a-zA-Z0-9_-]+$` charset. The rewrite is a deterministic
`<16 hex digits>_<readable part>`, so a tool call and its result stay paired and the id is at
most 64 characters. Use it when history produced by another backend (`functions.Bash:0`) is
replayed to Anthropic. Conforming ids are untouched.

See [Configuration](configuration.md#routessanitize) for the field reference.

Transformation is applied only when at least one of these is true:

- a model rewrite applies,
- `drop_fields` or `set_fields` is non-empty,
- `drop_tool_fields` is non-empty,
- `sanitize` is not all-default.

Otherwise the original request bytes are forwarded unchanged. When transformation runs, steps
apply in this order: model rewrite, `drop_fields`, `set_fields`, `metadata.user_id`, tool ids,
tool schema. Untouched top-level values keep their original bytes and every key keeps its
original position; a key set by the router is appended when it was absent. Tool order is
preserved.

If transformation fails (for example `tools` is not an array of objects), the request is
rejected with `400` and never reaches the upstream.

## Header processing

Order matters:

1. Hop-by-hop headers are removed: `Connection`, `Proxy-Connection`, `Keep-Alive`,
   `Proxy-Authenticate`, `Proxy-Authorization`, `Te`, `Trailer`, `Transfer-Encoding`,
   `Upgrade`, plus any header named in the incoming `Connection` header.
2. `X-Llm-Router-Token` is removed (it must never reach an upstream).
3. Authentication is rewritten according to `auth.mode`:
   - `passthrough`: headers are left as the client sent them;
   - `bearer`: `X-Api-Key` removed, `Authorization: Bearer <token>` set;
   - `x-api-key`: `Authorization` removed, `X-Api-Key: <token>` set.
4. Each `drop_headers` entry is removed.
5. Each `set_headers` entry is set (values already expanded at startup).
6. `Host` is set to the upstream authority.

Additional message-path behavior:

- `Content-Length` is set to the transformed body length.
- On exact `/v1/messages`, `Accept-Encoding: identity` is forced. This is required for usage
  sniffing and avoids compressing an SSE stream.

Other client headers (`anthropic-beta`, `x-claude-code-agent-id`, custom headers) are
forwarded unless dropped or replaced.

## Paths and queries

The request path is appended to the upstream path:

```
upstream = https://api.example.com/anthropic
request  = POST /v1/messages?beta=true
forward  = POST https://api.example.com/anthropic/v1/messages?beta=true
```

A query string configured on the upstream URL is preserved and combined with the incoming
query using `&`.

## Examples

### Route by prefix

```json
{
  "name": "synthetic",
  "upstream": "https://api.synthetic.new/anthropic",
  "models": ["hf:*", "syn:*"],
  "auth": { "mode": "bearer", "token": "{env:SYNTHETIC_API_KEY}" }
}
```

`model: "hf:moonshotai/Kimi-K3"` goes to Synthetic with its own key.

### Alias a short model name

```json
{
  "name": "synthetic",
  "upstream": "https://api.synthetic.new/anthropic",
  "models": ["hf:*"],
  "rewrite": { "kimi": "hf:moonshotai/Kimi-K3" },
  "auth": { "mode": "bearer", "token": "{env:SYNTHETIC_API_KEY}" }
}
```

`model: "kimi"` matches through the rewrite key and is sent as
`hf:moonshotai/Kimi-K3`.

### Upstreams without a distinctive prefix

```json
{
  "name": "nanogpt",
  "upstream": "https://nano-gpt.com/api",
  "models": ["nano:*"],
  "strip_prefix": "nano:",
  "auth": { "mode": "bearer", "token": "{env:NANOGPT_API_KEY}" }
}
```

`nano:deepseek/deepseek-v4.1-flash:thinking` is sent as
`deepseek/deepseek-v4.1-flash:thinking`.

### Count tokens on a model that supports it

```json
{
  "name": "synthetic",
  "upstream": "https://api.synthetic.new/anthropic",
  "models": ["hf:*"],
  "count_tokens_model": "hf:deepseek-ai/DeepSeek-V4.1-Flash",
  "auth": { "mode": "bearer", "token": "{env:SYNTHETIC_API_KEY}" }
}
```

`POST /v1/messages/count_tokens` is forwarded with the DeepSeek model even if the request
asked for a different one. Real messages keep the requested model.

### Passthrough for first-party models

```json
{
  "name": "anthropic",
  "upstream": "https://api.anthropic.com",
  "models": ["claude-*"],
  "auth": { "mode": "passthrough" }
}
```

The subscription OAuth token reaches `api.anthropic.com`; routes that replace credentials
never see it.

## Disabled routes

A route with `auth.mode` `bearer` or `x-api-key` whose `token` template expands to an empty
string at startup is disabled:

- startup continues (unless it is the default route),
- a warning is logged,
- quota polling is skipped,
- matching requests receive `503`, an Anthropic-shaped error body, and
  `x-should-retry: false`.

This lets one config file be valid with any subset of provider keys present.
