use std::env;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use url::Url;

pub const DEFAULT_MAX_BODY_BYTES: i64 = 64 << 20;

/// Rendered into `config.schema.json` so editors show the effective default.
#[cfg(test)]
fn schema_default_max_body_bytes() -> i64 {
    DEFAULT_MAX_BODY_BYTES
}

#[cfg(test)]
fn schema_default_quota_interval() -> String {
    "60s".to_string()
}

pub const AUTH_PASSTHROUGH: &str = "passthrough";
pub const AUTH_BEARER: &str = "bearer";
pub const AUTH_X_API_KEY: &str = "x-api-key";

/// Config is the router configuration file.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Editor hint pointing at `config.schema.json`. Accepted and ignored.
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub listen: String,
    pub default_route: String,
    #[cfg_attr(test, schemars(default = "schema_default_max_body_bytes"))]
    pub max_body_bytes: i64,
    pub routes: Vec<RouteConfig>,
    /// Outbound network options shared by every route.
    pub network: NetworkConfig,
}

/// NetworkConfig describes how the router reaches its upstreams.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Outbound HTTP proxy (`http://` only); CONNECT is used for every upstream.
    /// Empty disables the proxy. Supports `{env:NAME}` and `{file:PATH}` templates.
    pub proxy: String,
    /// PEM bundle added to the native root store. Empty disables it.
    /// Supports `{env:NAME}` and `{file:PATH}` templates.
    pub ca_file: String,
    /// `proxy` expanded once at startup.
    #[serde(skip)]
    pub resolved_proxy: String,
    /// `ca_file` expanded once at startup.
    #[serde(skip)]
    pub resolved_ca_file: String,
}

/// RouteConfig describes one upstream and the models it serves.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteConfig {
    pub name: String,
    pub upstream: String,
    /// Glob patterns: "*" matches any sequence, including "/".
    pub models: Vec<String>,
    /// Maps an incoming model ID to the ID sent upstream. Keys also match the route.
    pub rewrite: std::collections::BTreeMap<String, String>,
    /// Removed from the model ID sent upstream (e.g. "nano:"). Rewrite wins.
    pub strip_prefix: String,
    pub auth: AuthConfig,
    /// Headers removed from requests sent upstream (e.g. anthropic-beta).
    pub drop_headers: Vec<String>,
    /// Request headers set before forwarding. Values support `{env:NAME}` and `{file:PATH}`.
    pub set_headers: std::collections::BTreeMap<String, String>,
    /// `set_headers` values expanded once at startup.
    #[serde(skip)]
    pub resolved_set_headers: std::collections::BTreeMap<String, String>,
    /// Top-level request body fields removed on /v1/messages*.
    pub drop_fields: Vec<String>,
    /// Top-level request body fields set on /v1/messages*. Values support the same
    /// templates as `auth.token`; the expansion is parsed as JSON, falling back to a string.
    pub set_fields: std::collections::BTreeMap<String, String>,
    /// `set_fields` values expanded once at startup.
    #[serde(skip)]
    pub resolved_set_fields: std::collections::BTreeMap<String, Value>,
    /// Fields removed from every entry of the request "tools" array.
    pub drop_tool_fields: Vec<String>,
    /// Body sanitization applied on /v1/messages*.
    pub sanitize: SanitizeConfig,
    /// Replaces the model on /v1/messages/count_tokens when set.
    pub count_tokens_model: String,
    /// Optionally polls an upstream quota endpoint and exports it as metrics.
    pub quota: Option<QuotaConfig>,
}

/// SanitizeConfig groups request-body relaxations for upstreams with a stricter validator.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SanitizeConfig {
    /// Relaxation applied to every `tools[].input_schema`.
    pub tool_schema: ToolSchemaConfig,
    /// Removes `metadata.user_id`, which some upstreams validate as `^[a-zA-Z0-9_-]+$`.
    pub drop_metadata_user_id: bool,
    /// Rewrites `tool_use.id` and `tool_result.tool_use_id` to Anthropic's allowed charset.
    pub sanitize_tool_use_ids: bool,
    /// Moves mid-conversation `tool_addition` definitions into the top-level `tools` array,
    /// for upstreams without the `inline-tools` beta. `tool_removal` blocks are dropped.
    pub hoist_inline_tools: bool,
}

impl SanitizeConfig {
    /// Reports whether the policy leaves every request body untouched.
    pub fn is_noop(&self) -> bool {
        self.tool_schema.is_noop()
            && !self.drop_metadata_user_id
            && !self.sanitize_tool_use_ids
            && !self.hoist_inline_tools
    }
}

/// ToolSchemaConfig relaxes tool input schemas for backends with a restricted validator.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSchemaConfig {
    /// Rewrite the `\0` NUL escape to `\x00` in every `pattern`.
    pub normalize_nul_escape: bool,
    /// Remove zero-width lookaround assertions from every `pattern`.
    pub strip_lookaround: bool,
    /// Remove the `pattern` keyword outright. Wins over the two rewrites.
    pub drop_pattern: bool,
    /// String `format` values to keep. Absent keeps every format.
    pub string_formats: Option<Vec<String>>,
}

impl ToolSchemaConfig {
    /// Reports whether the policy leaves every schema untouched.
    pub fn is_noop(&self) -> bool {
        !self.normalize_nul_escape
            && !self.strip_lookaround
            && !self.drop_pattern
            && self.string_formats.is_none()
    }
}

/// QuotaConfig describes an upstream quota endpoint authenticated with the route credential.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    pub url: String,
    /// Duration string, for example `60s` or `1m30s`.
    #[cfg_attr(
        test,
        schemars(with = "String", default = "schema_default_quota_interval")
    )]
    #[serde(deserialize_with = "deserialize_duration")]
    pub interval: Duration,
}

/// AuthConfig controls which credential reaches the upstream.
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub mode: String,
    /// Credential template: literal text with `{env:NAME}` and `{file:PATH}` placeholders.
    pub token: String,
    /// `token` expanded once at startup.
    #[serde(skip)]
    pub resolved: String,
}

/// Parses an expanded `set_fields` value as JSON, falling back to a JSON string.
fn field_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Expands `{env:NAME}` and `{file:PATH}` placeholders in `s`; other text is literal.
/// Relative file paths resolve against `base_dir`; `~/` expands to `$HOME`.
pub fn expand_template(s: &str, base_dir: &Path) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return Ok(out);
        };
        let inner = &after[..close];
        if let Some(name) = inner.strip_prefix("env:") {
            out.push_str(&env::var(name).unwrap_or_default());
        } else if let Some(path) = inner.strip_prefix("file:") {
            let path = if path == "~" || path.starts_with("~/") {
                format!("{}{}", env::var("HOME").unwrap_or_default(), &path[1..])
            } else {
                path.to_string()
            };
            let full = base_dir.join(&path);
            let content =
                std::fs::read_to_string(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            out.push_str(content.trim());
        } else if inner.contains(':') {
            return Err(format!("unknown placeholder {{{inner}}}"));
        } else {
            out.push('{');
            out.push_str(inner);
            out.push('}');
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn deserialize_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

/// Parses a duration string ("60s", "1h30m", "500ms", "0").
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let original = s;
    let mut rest = s;
    let negative = if let Some(r) = rest.strip_prefix('-') {
        rest = r;
        true
    } else if let Some(r) = rest.strip_prefix('+') {
        rest = r;
        false
    } else {
        false
    };
    if rest.is_empty() {
        return Err(format!("time: invalid duration {original:?}"));
    }
    if rest == "0" {
        return Ok(Duration::ZERO);
    }
    let mut total_secs = 0.0f64;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_end == 0 {
            return Err(format!("time: invalid duration {original:?}"));
        }
        let num: f64 = rest[..num_end]
            .parse()
            .map_err(|_| format!("time: invalid duration {original:?}"))?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let mult = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(format!("time: invalid duration {original:?}")),
        };
        total_secs += num * mult;
    }
    let d = Duration::from_secs_f64(total_secs);
    Ok(if negative { Duration::ZERO } else { d })
}

pub fn load_config(path: &str) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut cfg: Config = jsonc_parser::parse_to_serde_value(&text, &Default::default())
        .map_err(|e| format!("parse {path}: {e}"))?;
    let base_dir = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
    cfg.resolve_credentials(base_dir)
        .map_err(|e| format!("invalid config {path}: {e}"))?;
    cfg.validate()
        .map_err(|e| format!("invalid config {path}: {e}"))?;
    Ok(cfg)
}

impl Config {
    /// Expands every template: route `auth.token`, `set_headers`, `set_fields`, and the
    /// network `proxy` / `ca_file`.
    pub fn resolve_credentials(&mut self, base_dir: &Path) -> Result<(), String> {
        for r in &mut self.routes {
            let route_name = r.name.clone();
            r.auth.resolved = expand_template(&r.auth.token, base_dir)
                .map_err(|e| format!("route {route_name:?}: auth.token: {e}"))?;
            let resolved = r
                .set_headers
                .iter()
                .map(|(name, value)| {
                    expand_template(value, base_dir)
                        .map(|v| (name.clone(), v))
                        .map_err(|e| format!("route {route_name:?}: set_headers[{name:?}]: {e}"))
                })
                .collect::<Result<_, _>>()?;
            r.resolved_set_headers = resolved;
            let fields = r
                .set_fields
                .iter()
                .map(|(name, value)| {
                    expand_template(value, base_dir)
                        .map(|v| (name.clone(), field_value(&v)))
                        .map_err(|e| format!("route {route_name:?}: set_fields[{name:?}]: {e}"))
                })
                .collect::<Result<_, _>>()?;
            r.resolved_set_fields = fields;
        }
        self.network.resolved_proxy = expand_template(&self.network.proxy, base_dir)
            .map_err(|e| format!("network.proxy: {e}"))?;
        let ca_file = expand_template(&self.network.ca_file, base_dir)
            .map_err(|e| format!("network.ca_file: {e}"))?;
        self.network.resolved_ca_file = if ca_file.is_empty() || Path::new(&ca_file).is_absolute() {
            ca_file
        } else {
            base_dir.join(&ca_file).to_string_lossy().into_owned()
        };
        Ok(())
    }

    pub fn validate(&mut self) -> Result<(), String> {
        let mut errs: Vec<String> = Vec::new();
        if self.routes.is_empty() {
            errs.push("no routes defined".to_string());
        }
        if self.max_body_bytes <= 0 {
            self.max_body_bytes = DEFAULT_MAX_BODY_BYTES;
        }
        let default_route = self.default_route.clone();
        let mut names = std::collections::HashSet::new();
        for (i, r) in self.routes.iter_mut().enumerate() {
            if r.name.is_empty() {
                errs.push(format!("routes[{i}]: name is required"));
            } else if names.contains(&r.name) {
                errs.push(format!("routes[{i}]: duplicate name {:?}", r.name));
            }
            names.insert(r.name.clone());

            let upstream_ok = Url::parse(&r.upstream).is_ok_and(|u| {
                (u.scheme() == "http" || u.scheme() == "https") && u.host().is_some()
            });
            if !upstream_ok {
                errs.push(format!(
                    "route {:?}: upstream must be an absolute http(s) URL",
                    r.name
                ));
            }

            match r.auth.mode.as_str() {
                AUTH_PASSTHROUGH => {}
                AUTH_BEARER | AUTH_X_API_KEY => {
                    if r.auth.token.is_empty() {
                        errs.push(format!(
                            "route {:?}: auth.token is required for mode {:?}",
                            r.name, r.auth.mode
                        ));
                    } else if r.auth.resolved.is_empty() && r.name == default_route {
                        errs.push(format!("default route {:?}: auth.token is empty", r.name));
                    }
                }
                _ => errs.push(format!(
                    "route {:?}: unknown auth.mode {:?}",
                    r.name, r.auth.mode
                )),
            }

            if let Some(q) = r.quota.as_mut() {
                if r.auth.mode == AUTH_PASSTHROUGH {
                    errs.push(format!(
                        "route {:?}: quota polling needs a route credential (auth.mode bearer or x-api-key)",
                        r.name
                    ));
                }
                if !Url::parse(&q.url).is_ok_and(|u| u.host().is_some()) {
                    errs.push(format!(
                        "route {:?}: quota.url must be an absolute URL",
                        r.name
                    ));
                }
                if q.interval.is_zero() {
                    q.interval = Duration::from_secs(60);
                }
            }
        }
        if self.default_route.is_empty() {
            errs.push("default_route is required".to_string());
        } else if !names.contains(&self.default_route) {
            errs.push(format!(
                "default_route {:?} does not exist",
                self.default_route
            ));
        }
        if !self.network.resolved_proxy.is_empty() {
            let valid = Url::parse(&self.network.resolved_proxy)
                .is_ok_and(|u| u.scheme() == "http" && u.host().is_some());
            if !valid {
                errs.push("network.proxy must be an absolute http:// URL".to_string());
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("\n"))
        }
    }
}

/// Reports whether `s` matches `pattern`, where "*" matches any sequence of characters.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let Some(mut rest) = s.strip_prefix(parts[0]) else {
        return false;
    };
    for p in &parts[1..parts.len() - 1] {
        let Some((_, after)) = rest.split_once(p) else {
            return false;
        };
        rest = after;
    }
    rest.ends_with(parts[parts.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_syntax_is_accepted() {
        let src = r#"{
  // line comment
  "a": "http://x/*not a comment*/", /* block
  comment */
  "b": [1, 2, ],
  "c": {"d": true,},
  "e": "trailing comma in string,}",
}"#;
        let value: Value =
            jsonc_parser::parse_to_serde_value(src, &Default::default()).expect("parse");
        assert_eq!(
            value,
            serde_json::json!({
                "a": "http://x/*not a comment*/",
                "b": [1, 2],
                "c": {"d": true},
                "e": "trailing comma in string,}",
            })
        );
    }

    #[test]
    fn load_config_accepts_jsonc() {
        let dir = std::env::temp_dir().join(format!("llm-router-jsonc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
  // minimal routed config
  "default_route": "a",
  "routes": [
    { "name": "a", "upstream": "http://127.0.0.1:9", "auth": {"mode": "passthrough"}, },
  ],
}"#,
        )
        .expect("write config");
        let cfg = load_config(path.to_str().expect("utf-8 path")).expect("load jsonc config");
        assert_eq!(cfg.routes[0].name, "a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn glob() {
        let cases = [
            ("hf:*", "hf:moonshotai/Kimi-K3", true),
            ("claude-*", "claude-opus-5", true),
            ("claude-*", "hf:claude", false),
            ("*", "anything", true),
            ("exact", "exact", true),
            ("exact", "exactly", false),
            ("a*b*c", "a-x-b-y-c", true),
            ("a*a", "a", false),
            ("*-flash", "hf:x/GLM-5.3-flash", true),
        ];
        for (pattern, s, want) in cases {
            assert_eq!(
                glob_match(pattern, s),
                want,
                "glob_match({pattern:?}, {s:?})"
            );
        }
    }

    #[test]
    fn go_durations() {
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert!(parse_duration("60").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn validate_reports_all_errors() {
        // An env var that is guaranteed to be absent.
        // SAFETY: tests in this module do not touch this variable concurrently.
        unsafe {
            std::env::remove_var("EMPTY_KEY");
        }
        let mut cfg = Config {
            default_route: "missing".to_string(),
            routes: vec![
                RouteConfig {
                    name: "a".to_string(),
                    upstream: "ftp://x".to_string(),
                    auth: AuthConfig {
                        mode: AUTH_BEARER.to_string(),
                        token: "{env:EMPTY_KEY}".to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                RouteConfig {
                    name: "a".to_string(),
                    upstream: "https://x".to_string(),
                    auth: AuthConfig {
                        mode: "nope".to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        cfg.resolve_credentials(Path::new(".")).expect("resolve");
        let err = cfg.validate().expect_err("expected error");
        for want in [
            "duplicate name",
            "absolute http(s)",
            "unknown auth.mode",
            "default_route",
        ] {
            assert!(err.contains(want), "error {err:?} missing {want:?}");
        }

        let mut cfg = Config {
            default_route: "a".to_string(),
            routes: vec![
                RouteConfig {
                    name: "a".to_string(),
                    upstream: "https://x".to_string(),
                    auth: AuthConfig {
                        mode: AUTH_BEARER.to_string(),
                        token: "{env:EMPTY_KEY}".to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                RouteConfig {
                    name: "b".to_string(),
                    upstream: "https://x".to_string(),
                    auth: AuthConfig {
                        mode: AUTH_X_API_KEY.to_string(),
                        token: "{env:EMPTY_KEY}".to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        cfg.resolve_credentials(Path::new(".")).expect("resolve");
        let err = cfg.validate().expect_err("empty default route credential");
        assert!(
            err.contains("default route \"a\": auth.token is empty"),
            "{err}"
        );
        cfg.routes[0].auth = AuthConfig {
            mode: AUTH_PASSTHROUGH.to_string(),
            ..Default::default()
        };
        cfg.validate()
            .expect("empty credential on a non-default route must not fail validation");
    }

    #[test]
    fn template_expansion() {
        let dir = std::env::temp_dir().join(format!("llm-router-tpl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("key"), "  secret\n").expect("write key");
        std::fs::write(dir.join("home-key"), "tilde").expect("write home-key");
        let base = Path::new(".");
        // SAFETY: this variable is only used by this test.
        unsafe {
            std::env::set_var("LLM_ROUTER_TPL_TEST", "from-env");
        }

        assert_eq!(expand_template("literal", base).unwrap(), "literal");
        assert_eq!(expand_template("", base).unwrap(), "");
        assert_eq!(expand_template("Bearer abc", base).unwrap(), "Bearer abc");
        assert_eq!(
            expand_template("Bearer {env:LLM_ROUTER_TPL_TEST}", base).unwrap(),
            "Bearer from-env"
        );
        assert_eq!(expand_template("{env:SURELY_UNSET_VAR}", base).unwrap(), "");
        assert_eq!(
            expand_template(&format!("Bearer {{file:{}/key}}", dir.display()), base).unwrap(),
            "Bearer secret"
        );
        let rel = expand_template("{file:key}", &dir).unwrap();
        assert_eq!(rel, "secret");
        assert_eq!(
            expand_template("a{env:SURELY_UNSET_VAR}b", base).unwrap(),
            "ab"
        );
        assert_eq!(expand_template("{plain}", base).unwrap(), "{plain}");
        assert_eq!(expand_template("{unclosed", base).unwrap(), "{unclosed");
        assert!(expand_template(&format!("{{file:{}/nope}}", dir.display()), base).is_err());
        assert!(expand_template("{yaml:x}", base).is_err());

        let home = std::path::PathBuf::from(env::var("HOME").expect("HOME"));
        std::fs::write(home.join(".llm-router-tpl-test"), "home").expect("write home file");
        assert_eq!(
            expand_template("{file:~/.llm-router-tpl-test}", base).unwrap(),
            "home"
        );
        std::fs::remove_file(home.join(".llm-router-tpl-test")).ok();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_set_header_templates() {
        let dir = std::env::temp_dir().join(format!("llm-router-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("key"), "  secret\n").expect("write key");
        // SAFETY: this variable is only used by this test.
        unsafe {
            std::env::set_var("LLM_ROUTER_HDR_TEST", "from-env");
        }
        let mut cfg = Config {
            routes: vec![RouteConfig {
                name: "a".to_string(),
                set_headers: [
                    ("x-env".to_string(), "{env:LLM_ROUTER_HDR_TEST}".to_string()),
                    ("x-literal".to_string(), "plain".to_string()),
                ]
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.resolve_credentials(&dir).expect("resolve");
        assert_eq!(cfg.routes[0].resolved_set_headers["x-env"], "from-env");
        assert_eq!(cfg.routes[0].resolved_set_headers["x-literal"], "plain");

        cfg.routes[0].set_headers.insert(
            "x-file".to_string(),
            format!("Bearer {{file:{}/key}}", dir.display()),
        );
        cfg.resolve_credentials(&dir).expect("resolve file");
        assert_eq!(
            cfg.routes[0].resolved_set_headers["x-file"],
            "Bearer secret"
        );

        cfg.routes[0]
            .set_headers
            .insert("x-bad".to_string(), "{yaml:x}".to_string());
        let err = cfg
            .resolve_credentials(&dir)
            .expect_err("unknown placeholder");
        assert!(err.contains("set_headers[\"x-bad\"]"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sanitize_config_parsing() {
        let cfg: Config = serde_json::from_str(
            r#"{"routes":[{"name":"a","upstream":"http://x","sanitize":{"tool_schema":{"normalize_nul_escape":true,"string_formats":["uuid"]},"drop_metadata_user_id":true,"sanitize_tool_use_ids":true}}]}"#,
        )
        .expect("parse");
        let sanitize = &cfg.routes[0].sanitize;
        assert!(sanitize.drop_metadata_user_id);
        assert!(sanitize.sanitize_tool_use_ids);
        assert!(sanitize.tool_schema.normalize_nul_escape);
        assert!(!sanitize.is_noop());
        assert_eq!(
            sanitize.tool_schema.string_formats.clone().unwrap(),
            vec!["uuid".to_string()]
        );

        let err = serde_json::from_str::<Config>(
            r#"{"routes":[{"name":"a","upstream":"http://x","sanitize":{"bogus":true}}]}"#,
        )
        .expect_err("unknown sanitize field");
        assert!(err.to_string().contains("bogus"), "{err}");

        let err = serde_json::from_str::<Config>(
            r#"{"routes":[{"name":"a","upstream":"http://x","sanitize":{"tool_schema":{"bogus":true}}}]}"#,
        )
        .expect_err("unknown tool_schema field");
        assert!(err.to_string().contains("bogus"), "{err}");

        let cfg: Config =
            serde_json::from_str(r#"{"routes":[{"name":"a","upstream":"http://x"}]}"#)
                .expect("parse");
        assert!(cfg.routes[0].sanitize.is_noop());

        let cfg: Config = serde_json::from_str(
            r#"{"$schema":"https://example.test/config.schema.json","routes":[]}"#,
        )
        .expect("$schema is accepted");
        assert!(cfg.schema.is_some());
    }

    #[test]
    fn set_fields_expand_templates_and_parse_json() {
        let dir = std::env::temp_dir().join(format!("llm-router-sf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("max"), "42").expect("write file");
        // SAFETY: this variable is only used by this test.
        unsafe {
            std::env::set_var("LLM_ROUTER_SF_TEST", "\"quoted\"");
        }

        let mut cfg = Config {
            default_route: "a".to_string(),
            routes: vec![RouteConfig {
                name: "a".to_string(),
                upstream: "http://x".to_string(),
                auth: AuthConfig {
                    mode: AUTH_PASSTHROUGH.to_string(),
                    ..Default::default()
                },
                set_fields: std::collections::BTreeMap::from([
                    (
                        "file".to_string(),
                        format!("{{file:{}/max}}", dir.display()),
                    ),
                    ("env".to_string(), "{env:LLM_ROUTER_SF_TEST}".to_string()),
                    ("number".to_string(), "1.5".to_string()),
                    ("text".to_string(), "hello".to_string()),
                    ("flag".to_string(), "true".to_string()),
                ]),
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.resolve_credentials(&dir).expect("resolve");
        let fields = &cfg.routes[0].resolved_set_fields;
        assert_eq!(fields["file"], 42);
        assert_eq!(fields["env"], "quoted");
        assert_eq!(fields["number"], 1.5);
        assert_eq!(fields["text"], "hello");
        assert_eq!(fields["flag"], true);
        cfg.validate().expect("valid config");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn network_proxy_validation_and_templates() {
        let mut cfg = Config {
            default_route: "a".to_string(),
            routes: vec![RouteConfig {
                name: "a".to_string(),
                upstream: "http://x".to_string(),
                auth: AuthConfig {
                    mode: AUTH_PASSTHROUGH.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.network.proxy = "ftp://proxy:21".to_string();
        cfg.resolve_credentials(Path::new(".")).expect("resolve");
        let err = cfg.validate().expect_err("non-http proxy");
        assert!(err.contains("network.proxy"), "{err}");

        // SAFETY: these variables are only used by this test.
        unsafe {
            std::env::set_var("LLM_ROUTER_PROXY_TEST", "http://proxy.local:3128");
            std::env::set_var("LLM_ROUTER_CA_TEST", "/tmp/extra-ca.pem");
        }
        cfg.network.proxy = "{env:LLM_ROUTER_PROXY_TEST}".to_string();
        cfg.network.ca_file = "{env:LLM_ROUTER_CA_TEST}".to_string();
        cfg.resolve_credentials(Path::new(".")).expect("resolve");
        assert_eq!(cfg.network.resolved_proxy, "http://proxy.local:3128");
        assert_eq!(cfg.network.resolved_ca_file, "/tmp/extra-ca.pem");
        cfg.validate().expect("valid proxy");
    }

    #[test]
    fn go_durations_accept_signs_and_reject_junk() {
        assert_eq!(parse_duration("+5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("-5s").unwrap(), Duration::ZERO);
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("-").is_err());
        assert_eq!(parse_duration("1h2m3s").unwrap(), Duration::from_secs(3723));
    }

    #[test]
    fn quota_interval_deserializes_from_a_duration_string() {
        let cfg: Config = serde_json::from_str(
            r#"{"routes":[{"name":"a","quota":{"url":"http://x/q","interval":"1m30s"}}]}"#,
        )
        .expect("config");
        assert_eq!(
            cfg.routes[0].quota.as_ref().unwrap().interval,
            Duration::from_secs(90)
        );
        assert!(
            serde_json::from_str::<Config>(
                r#"{"routes":[{"name":"a","quota":{"url":"http://x","interval":"soon"}}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn glob_requires_literal_tail() {
        assert!(!glob_match("a*b*c", "a-x"));
        assert!(!glob_match("hf:*", "other"));
        assert!(glob_match("a*b*c", "a-b-c"));
    }

    #[test]
    fn validate_reports_name_token_quota_and_default_route_errors() {
        let mut cfg = Config {
            default_route: "ok".to_string(),
            routes: vec![
                RouteConfig {
                    name: String::new(),
                    upstream: "http://x".to_string(),
                    ..Default::default()
                },
                RouteConfig {
                    name: "keyed".to_string(),
                    upstream: "http://x".to_string(),
                    auth: AuthConfig {
                        mode: AUTH_BEARER.to_string(),
                        token: String::new(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                RouteConfig {
                    name: "quota".to_string(),
                    upstream: "http://x".to_string(),
                    auth: AuthConfig {
                        mode: AUTH_PASSTHROUGH.to_string(),
                        ..Default::default()
                    },
                    quota: Some(QuotaConfig {
                        url: "not a url".to_string(),
                        interval: Duration::ZERO,
                    }),
                    ..Default::default()
                },
                RouteConfig {
                    name: "ok".to_string(),
                    upstream: "http://x".to_string(),
                    auth: AuthConfig {
                        mode: AUTH_PASSTHROUGH.to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        cfg.resolve_credentials(Path::new(".")).expect("resolve");
        let err = cfg.validate().expect_err("expected errors");
        for want in [
            "name is required",
            "auth.token is required",
            "quota polling needs a route credential",
            "quota.url must be an absolute URL",
        ] {
            assert!(err.contains(want), "error {err:?} missing {want:?}");
        }

        let mut cfg = Config {
            default_route: String::new(),
            routes: vec![RouteConfig {
                name: "ok".to_string(),
                upstream: "http://x".to_string(),
                auth: AuthConfig {
                    mode: AUTH_PASSTHROUGH.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.resolve_credentials(Path::new(".")).expect("resolve");
        let err = cfg.validate().expect_err("missing default route");
        assert!(err.contains("default_route is required"), "{err}");
    }

    #[test]
    fn relative_ca_file_resolves_against_config_dir() {
        let mut cfg = Config::default();
        cfg.network.ca_file = "extra-ca.pem".to_string();
        cfg.resolve_credentials(Path::new("/etc/llm-router"))
            .expect("resolve");
        assert_eq!(cfg.network.resolved_ca_file, "/etc/llm-router/extra-ca.pem");
    }

    /// Keeps `config.schema.json` in sync with the Rust definitions.
    /// Regenerate with `UPDATE_SCHEMA=1 cargo test config_schema_is_current`.
    #[test]
    fn config_schema_is_current() {
        let schema = schemars::schema_for!(Config);
        let rendered = format!(
            "{}\n",
            serde_json::to_string_pretty(&schema).expect("render schema")
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.schema.json");
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(&path, &rendered).expect("write config.schema.json");
        }
        let committed = std::fs::read_to_string(&path).expect("read config.schema.json");
        assert_eq!(
            committed, rendered,
            "config.schema.json is stale; run UPDATE_SCHEMA=1 cargo test config_schema_is_current"
        );
    }
}
