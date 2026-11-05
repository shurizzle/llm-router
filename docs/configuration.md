# Configuration

The router reads one JSONC file: `//` and `/* */` comments and trailing commas are accepted,
plus everything plain JSON allows. Comments are stripped before parsing, so error line and
column numbers still match the file. Unknown fields are rejected, so a typo in a field name
fails startup instead of being ignored.

`config.schema.json` describes the file for editors: point your editor at it to get
completion and validation, or add a `"$schema"` key with its URL. The router accepts
`"$schema"` and ignores it. The file is generated from the Rust types; `cargo test` fails
when it is stale. Regenerate with `UPDATE_SCHEMA=1 cargo test config_schema_is_current`.

The schema cannot express cross-field rules. `default_route` must name an existing route,
`auth.token` is required for `bearer` and `x-api-key`, and quota polling requires a route
credential. Those are checked at startup by `validate()`.

Config path resolution, in order:

1. `--config <path>` flag,
2. `LLM_ROUTER_CONFIG` environment variable,
3. `/etc/llm-router/config.json`.

The chosen file is watched for changes and reloaded on save. Sending `SIGHUP` reloads it too.

## Minimal file

```json
{
  "default_route": "anthropic",
  "routes": [
    {
      "name": "anthropic",
      "upstream": "https://api.anthropic.com",
      "models": ["claude-*"],
      "auth": { "mode": "passthrough" }
    }
  ]
}
```

With this file and `ANTHROPIC_BASE_URL=http://127.0.0.1:8787`, traffic is forwarded to
`api.anthropic.com` with the caller's credentials, unmodified.

## Top-level fields

| Field | Type | Default | Description |
|---|---|---|---|
| `listen` | string | `:8787` | Listen address. `:8787` binds `0.0.0.0:8787`. Overridden by `LLM_ROUTER_LISTEN`. |
| `default_route` | string | — | Required. Route used for unmatched models and for requests that are not `POST /v1/messages*`. |
| `max_body_bytes` | integer | `67108864` (64 MiB) | Request body limit for messages paths. Values `<= 0` are replaced with the default. |
| `routes` | array | — | Required, non-empty. Evaluated in order; the first match wins. |
| `network` | object | disabled | Outbound proxy and extra CA; see below. |

## `routes[]`

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | — | Required, unique. Used in metrics, logs and errors. |
| `upstream` | string | — | Required. Absolute `http` or `https` URL. The request path is appended to its path. |
| `models` | string[] | `[]` | Glob patterns matched against the request model. `*` matches any sequence, including `/`. |
| `rewrite` | object | `{}` | Maps an incoming model ID to the upstream ID. Keys also match the route. |
| `strip_prefix` | string | `""` | Prefix removed from the model ID before forwarding, when present. `rewrite` wins. |
| `auth` | object | — | Credential handling; see below. |
| `drop_headers` | string[] | `[]` | Request headers removed before forwarding (case-insensitive). |
| `set_headers` | object | `{}` | Request headers set before forwarding. Applied after drops. Values are templates (see `auth.token`). |
| `drop_fields` | string[] | `[]` | Top-level body fields removed on `/v1/messages*`. |
| `set_fields` | object | `{}` | Top-level body fields set on `/v1/messages*`, after the drops. Values are templates; see below. |
| `drop_tool_fields` | string[] | `[]` | Fields removed from every entry of the body `tools` array. |
| `sanitize` | object | no-op | Body relaxations for stricter upstreams: `tool_schema`, `drop_metadata_user_id`; see below. |
| `count_tokens_model` | string | `""` | Model used instead of the requested one on `/v1/messages/count_tokens`. |
| `quota` | object | absent | Optional quota endpoint polled with the route credential. |

## `routes[].auth`

| Field | Type | Description |
|---|---|---|
| `mode` | string | `passthrough`, `bearer`, or `x-api-key`. Anything else is a validation error. |
| `token` | string | Credential template, expanded once at startup. Required for `bearer` and `x-api-key`; ignored for `passthrough`. |

Modes:

- `passthrough` — forward the caller's `Authorization` and `X-Api-Key` headers unchanged.
- `bearer` — remove `X-Api-Key` and set `Authorization: Bearer <token>`.
- `x-api-key` — remove `Authorization` and set `X-Api-Key: <token>`.

The `token` value is a template. Each occurrence of a placeholder is replaced:

| Placeholder | Expands to |
|---|---|
| `{env:NAME}` | The environment variable `NAME`, empty if unset. |
| `{file:PATH}` | The file contents, surrounding whitespace trimmed. `~/` expands to `$HOME`; a relative path is resolved against the config file's directory. |

All other text is literal, so a plain token works too, and the placeholders can be mixed with
literal text. An unreadable file or an unknown placeholder kind is a startup error.
`set_headers` values use the same placeholders and expansion rules.

```json
"auth": { "mode": "bearer", "token": "{file:~/.secrets/synthetic-key}" }
```

A non-default route whose credential expands to an empty string at startup is **disabled**,
not a startup error: matching requests get `503` with `x-should-retry: false`, and quota
polling is skipped. The default route must have a non-empty credential when it uses `bearer`
or `x-api-key`, otherwise validation fails.

## `routes[].set_fields`

Top-level body keys written on every `/v1/messages*` request, after `drop_fields` removes the
ones listed there. Values are templates expanded once at startup, with the same
`{env:NAME}` / `{file:PATH}` placeholders as `auth.token`. The expanded text is parsed as
JSON; when parsing fails it is sent as a JSON string, so `"1"` becomes a number and `"hello"`
a string.

```json
"set_fields": {
  "temperature": "1",
  "max_tokens": "{env:MAX_TOKENS}"
}
```

Only top-level keys are supported: a key is written literally, not interpreted as a path.

## `routes[].sanitize`

Request-body relaxations for upstreams that validate more strictly than Anthropic. Every
option defaults to off; the request body is forwarded byte for byte when all are off.

| Field | Type | Default | Description |
|---|---|---|---|
| `tool_schema` | object | no-op | Relaxation applied to every `tools[].input_schema`; see below. |
| `drop_metadata_user_id` | bool | `false` | Removes `metadata.user_id`. Some upstreams validate it as `^[a-zA-Z0-9_-]+$` and answer `400 Invalid 'user_id'`. |
| `sanitize_tool_use_ids` | bool | `false` | Rewrites `tool_use.id` and `tool_result.tool_use_id` to Anthropic's `^[a-zA-Z0-9_-]+$` charset, deterministically. |
| `hoist_inline_tools` | bool | `false` | Moves mid-conversation tool definitions into the top-level `tools` array; see below. |

### `routes[].sanitize.hoist_inline_tools`

Claude Code with `ENABLE_TOOL_SEARCH` sends mid-conversation tool definitions when the
`inline-tools` beta is active, as a `role: "system"` message:

```json
{"role": "system", "content": [{"type": "tool_addition", "tool": {
  "type": "tool_definition",
  "definition": {"name": "db_query", "input_schema": {"type": "object"}}}}]}
```

An upstream that does not implement the beta rejects the block with `422` (or `400`) and
`unknown variant 'tool_definition'`. With `hoist_inline_tools`, each by-value definition is
appended to `tools` (duplicates by `name` are skipped), the block is removed, and the usual
`drop_tool_fields` / `tool_schema` handling then applies to the added entry.

Two limits: `tools` is static per request, so a hoisted tool is available from the start
rather than from the block's position; and a `tool_removal` block has no equivalent, so it is
dropped and the tool stays available. Each dropped `tool_removal` logs a `WARN` line named
`dropping tool_removal block unsupported by the upstream` with the `tool` field. `tool_addition` blocks that reference a tool by name
(`tool_reference`) are left untouched. An `mcp_toolset` definition cannot be hoisted and its
block is dropped.

### `routes[].sanitize.tool_schema`

Some Anthropic-compatible backends validate tool schemas with a restricted regex engine.
They reject the request when a `pattern` uses an escape or group they do not support, so
these options relax the schema on this route only.

| Field | Type | Default | Description |
|---|---|---|---|
| `normalize_nul_escape` | bool | `false` | Rewrite the `\0` escape to `\x00` in every `pattern`. |
| `strip_lookaround` | bool | `false` | Remove `(?=...)`, `(?!...)`, `(?<=...)` and `(?<!...)` from every `pattern`, plus a quantifier attached to the removed group. |
| `drop_pattern` | bool | `false` | Remove the `pattern` keyword. Wins over the two rewrites above. |
| `string_formats` | string[] | absent | Allowlist of `format` values kept on string schemas. Absent keeps every format; `[]` drops every string format. |

The walk covers the whole schema: nested `properties`, `items`, `anyOf`/`oneOf`/`allOf`,
`$defs`/`definitions`, `patternProperties`, `dependentSchemas` and schema-valued
`additionalProperties`. Keys named `pattern` or `format` inside a property-name map, and
values under `enum`, `const`, `default` and `examples`, are instance data and stay untouched.
Formats on numeric schemas (`integer`/`number`, optionally with `null`) are always kept.

`strip_lookaround` is a relaxation: it can only widen what the pattern matches. Rewrites are
deterministic, so repeated requests produce byte-identical tool schemas.

```json
"sanitize": {
  "tool_schema": {
    "normalize_nul_escape": true,
    "strip_lookaround": true,
    "string_formats": ["date", "date-time", "uri", "uuid"]
  },
  "drop_metadata_user_id": true
}
```

`drop_metadata_user_id` removes only that key: the rest of `metadata` is forwarded, and a
`metadata` value that is not an object is left untouched.

## `routes[].quota`

| Field | Type | Default | Description |
|---|---|---|---|
| `url` | string | — | Absolute URL polled with the route credential. |
| `interval` | string | `"60s"` | Poll interval. Accepts duration strings: `500ms`, `30s`, `1m30s`, `1h`. |

Quota polling requires a route credential: configuring it on a `passthrough` route is a
validation error.

## `network`

| Field | Type | Default | Description |
|---|---|---|---|
| `proxy` | string | `""` | Outbound proxy URL, `http://` only, optionally with `user:password@`. Every upstream is reached with HTTP CONNECT, including plain `http://`. Empty disables it. |
| `ca_file` | string | `""` | PEM bundle added to the native root store. A relative path resolves against the config file's directory. Empty disables it. |

Both fields expand `{env:NAME}` and `{file:PATH}` templates at startup.

```json
"network": {
  "proxy": "{env:HTTPS_PROXY}",
  "ca_file": "./certs/internal-ca.pem"
}
```

A proxy URL that is not `http://` or has no host fails validation. A missing or unreadable
`ca_file`, or one that contains no certificate, fails at startup. The same settings apply to
quota polling.

## Environment variables

| Variable | Description |
|---|---|
| `LLM_ROUTER_CONFIG` | Config file path. The `--config` flag wins. |
| `LLM_ROUTER_LISTEN` | Overrides `listen`. |
| `LLM_ROUTER_ADMIN_LISTEN` | Serve `/livez`, `/readyz` and `/metrics` on this address instead of the API listener. The API listener then answers nothing without a client token. |
| `LLM_ROUTER_CLIENT_TOKEN` | When set, every API request must carry `X-Llm-Router-Token: <token>`. Missing or wrong token closes the connection without a response. |
| `LLM_ROUTER_DRAIN_DELAY` | Delay between failing `/readyz` and closing listeners on `SIGTERM`. Default `5s`. |

Credential templates in `routes[].auth.token` are expanded once at startup, so changes to the
environment or the secret file need a restart.

## Validation

Startup fails with every validation error joined by newlines, prefixed with
`invalid config <path>:`. Checks:

- `routes` is not empty;
- each route has a non-empty, unique `name`;
- `upstream` parses as an absolute `http` or `https` URL with a host;
- `auth.mode` is one of the three known modes;
- `token` is non-empty for `bearer` / `x-api-key`;
- the default route's credential expands to a non-empty string;
- quota requires a credential mode and an absolute URL;
- `default_route` is set and names an existing route.

## Example: multiple providers

```json
{
  "listen": ":8787",
  "default_route": "anthropic",
  "routes": [
    {
      "name": "synthetic",
      "upstream": "https://api.synthetic.new/anthropic",
      "models": ["hf:*", "syn:*"],
      "auth": { "mode": "bearer", "token": "{env:SYNTHETIC_API_KEY}" },
      "drop_headers": ["anthropic-beta"],
      "count_tokens_model": "hf:deepseek-ai/DeepSeek-V4.1-Flash",
      "quota": { "url": "https://api.synthetic.new/v2/quotas", "interval": "60s" }
    },
    {
      "name": "nanogpt",
      "upstream": "https://nano-gpt.com/api",
      "models": ["nano:*"],
      "strip_prefix": "nano:",
      "auth": { "mode": "bearer", "token": "{env:NANOGPT_API_KEY}" }
    },
    {
      "name": "deepseek",
      "upstream": "https://api.deepseek.com/anthropic",
      "models": ["deepseek-*"],
      "auth": { "mode": "bearer", "token": "{env:DEEPSEEK_API_KEY}" },
      "drop_headers": ["x-anthropic-billing-header", "anthropic-beta"],
      "sanitize": {
        "tool_schema": { "normalize_nul_escape": true },
        "drop_metadata_user_id": true,
        "hoist_inline_tools": true
      }
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

Set only the keys of the providers you use. Unset providers are disabled; the file stays
valid.

## Notes

- `upstream` paths are joined with the request path, avoiding a double slash:
  `https://host/anthropic` + `/v1/messages` → `https://host/anthropic/v1/messages`.
- A query string on `upstream` is preserved and combined with the incoming query using `&`.
- The request body is re-encoded only when a rewrite or a drop actually applies; otherwise
  upstream receives the original bytes.
- Durations are strings, not numbers. `"interval": 60` is a config error.
