//! Tool-schema normalization for upstreams with a restricted JSON Schema validator.
//!
//! Claude Code sends `pattern` and `format` keywords inside `tools[].input_schema`. Some
//! Anthropic-compatible backends reject part of them and fail the whole request, so the
//! route config can relax them. Adapted from maximhq/bifrost (Apache-2.0),
//! `core/providers/utils/utils.go`.

use serde_json::{Map, Value};

use crate::config::ToolSchemaConfig;

const SCHEMA_MAP_KEYS: [&str; 5] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Normalizes a JSON Schema in place, returning whether anything changed.
pub fn sanitize_schema(schema: &mut Value, cfg: &ToolSchemaConfig) -> bool {
    normalize_value(schema, cfg, false)
}

/// Walks one schema value. `names` marks an object keyed by property or definition
/// names rather than by schema keywords.
fn normalize_value(value: &mut Value, cfg: &ToolSchemaConfig, names: bool) -> bool {
    match value {
        Value::Object(map) => normalize_map(map, cfg, names),
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= normalize_value(item, cfg, false);
            }
            changed
        }
        _ => false,
    }
}

fn normalize_map(map: &mut Map<String, Value>, cfg: &ToolSchemaConfig, names: bool) -> bool {
    let enclosing_type = if names {
        None
    } else {
        map.get("type").cloned()
    };
    let keys: Vec<String> = map.keys().cloned().collect();
    let mut changed = false;
    for key in keys {
        if !names {
            match key.as_str() {
                "pattern" => {
                    let Some(Value::String(pattern)) = map.get(&key) else {
                        continue;
                    };
                    if cfg.drop_pattern {
                        map.remove(&key);
                        changed = true;
                        continue;
                    }
                    if let Some(rewritten) = rewrite_pattern(pattern, cfg) {
                        map.insert(key, Value::String(rewritten));
                        changed = true;
                    }
                    continue;
                }
                "format" => {
                    let Some(Value::String(format)) = map.get(&key) else {
                        continue;
                    };
                    if drops_format(cfg, format, enclosing_type.as_ref()) {
                        map.remove(&key);
                        changed = true;
                    }
                    continue;
                }
                // Instance data, not schema: a "format" key inside is the caller's value.
                "enum" | "const" | "default" | "examples" => continue,
                k if SCHEMA_MAP_KEYS.contains(&k) => {
                    if let Some(nested) = map.get_mut(&key) {
                        changed |= normalize_value(nested, cfg, true);
                    }
                    continue;
                }
                _ => {}
            }
        }
        if let Some(nested) = map.get_mut(&key) {
            changed |= normalize_value(nested, cfg, false);
        }
    }
    changed
}

/// Applies the configured pattern rewrites. Returns None when the pattern is unchanged.
fn rewrite_pattern(pattern: &str, cfg: &ToolSchemaConfig) -> Option<String> {
    let mut current = None;
    if cfg.normalize_nul_escape {
        current = normalize_nul_escape(pattern);
    }
    if cfg.strip_lookaround {
        let input = current.as_deref().unwrap_or(pattern);
        if let Some(stripped) = strip_lookaround(input) {
            current = Some(stripped);
        }
    }
    current
}

/// drops_format reports whether a schema of type `typ` must lose its `format`. Formats on
/// numeric types are kept: no backend tested rejects them, and they are meaningful there.
fn drops_format(cfg: &ToolSchemaConfig, format: &str, typ: Option<&Value>) -> bool {
    let Some(allowed) = cfg.string_formats.as_ref() else {
        return false;
    };
    !allowed.iter().any(|f| f == format) && !is_numeric_type(typ)
}

fn is_numeric_type(typ: Option<&Value>) -> bool {
    match typ {
        Some(Value::String(t)) => t == "integer" || t == "number",
        Some(Value::Array(items)) => {
            let mut numeric = false;
            for item in items {
                match item.as_str() {
                    Some("null") => {}
                    Some("integer") | Some("number") => numeric = true,
                    _ => return false,
                }
            }
            numeric
        }
        _ => false,
    }
}

/// Rewrites `\0` to `\x00`, leaving legacy octal escapes (`\012`) and escaped backslashes
/// (`\\0`) alone. Returns None when nothing changed.
fn normalize_nul_escape(pattern: &str) -> Option<String> {
    if !pattern.contains(r"\0") {
        return None;
    }
    let bytes = pattern.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        let escape = bytes[i] == b'\\' && i + 1 < bytes.len();
        if !escape {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // `\012` is a legacy octal escape and stays; `\08` is a NUL escape plus a literal 8.
        let octal = i + 2 < bytes.len() && (b'0'..=b'7').contains(&bytes[i + 2]);
        if bytes[i + 1] == b'0' && !octal {
            out.extend_from_slice(br"\x00");
            changed = true;
        } else {
            out.push(bytes[i]);
            out.push(bytes[i + 1]);
        }
        i += 2;
    }
    changed.then(|| String::from_utf8(out).expect("NUL rewrite preserves UTF-8"))
}

/// Removes zero-width lookaround assertions, `(?=...)`, `(?!...)`, `(?<=...)` and
/// `(?<!...)`, plus a quantifier attached to the removed group.
///
/// This is a relaxation: it can only widen what the pattern matches. Non-capturing
/// `(?:...)`, named `(?<name>...)` groups and inline flags are left alone. An unbalanced
/// pattern is returned unchanged. Returns None when nothing changed.
fn strip_lookaround(pattern: &str) -> Option<String> {
    if !pattern.contains("(?") {
        return None;
    }
    let bytes = pattern.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let end = (i + 1 + char_len(bytes[i + 1])).min(bytes.len());
            out.extend_from_slice(&bytes[i..end]);
            i = end;
        } else if c == b'[' {
            let end = class_end(bytes, i);
            out.extend_from_slice(&bytes[i..end]);
            i = end;
        } else if c == b'(' && is_lookaround_start(bytes, i) {
            let end = group_end(bytes, i)?;
            changed = true;
            i = quantifier_end(bytes, end);
        } else {
            let end = (i + char_len(c)).min(bytes.len());
            out.extend_from_slice(&bytes[i..end]);
            i = end;
        }
    }
    changed.then(|| String::from_utf8(out).expect("lookaround removal preserves UTF-8"))
}

fn is_lookaround_start(bytes: &[u8], i: usize) -> bool {
    let rest = &bytes[i..];
    rest.starts_with(b"(?=")
        || rest.starts_with(b"(?!")
        || rest.starts_with(b"(?<=")
        || rest.starts_with(b"(?<!")
}

/// Returns the index just past the character class opening at `start`. A `]` in first
/// position (after an optional `^`) is a literal.
fn class_end(bytes: &[u8], start: usize) -> usize {
    let mut j = start + 1;
    if j < bytes.len() && bytes[j] == b'^' {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b']' {
        j += 1;
    }
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b']' => return j + 1,
            _ => j += 1,
        }
    }
    bytes.len()
}

/// Returns the index just past the `)` closing the group opening at `start`, honouring
/// escapes, character classes and nesting.
fn group_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = start;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'[' => j = class_end(bytes, j),
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => j += 1,
        }
    }
    None
}

/// Returns the index just past a quantifier starting at `start` (`*`, `+`, `?`, or a
/// well-formed `{n}`, `{n,}`, `{n,m}`), plus an optional lazy `?`. A malformed brace is
/// left in place.
fn quantifier_end(bytes: &[u8], start: usize) -> usize {
    let mut i = match bytes.get(start) {
        Some(b'*') | Some(b'+') | Some(b'?') => start + 1,
        Some(b'{') => {
            let mut j = start + 1;
            if !skip_digits(bytes, &mut j) {
                return start;
            }
            if j < bytes.len() && bytes[j] == b',' {
                j += 1;
                skip_digits(bytes, &mut j);
            }
            if bytes.get(j) != Some(&b'}') {
                return start;
            }
            j + 1
        }
        _ => return start,
    };
    if bytes.get(i) == Some(&b'?') {
        i += 1;
    }
    i
}

fn skip_digits(bytes: &[u8], j: &mut usize) -> bool {
    let begin = *j;
    while bytes.get(*j).is_some_and(u8::is_ascii_digit) {
        *j += 1;
    }
    *j > begin
}

fn char_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(nul: bool, lookaround: bool) -> ToolSchemaConfig {
        ToolSchemaConfig {
            normalize_nul_escape: nul,
            strip_lookaround: lookaround,
            ..Default::default()
        }
    }

    #[test]
    fn nul_escapes() {
        let cases = [
            (r"^[^\0]*$", Some(r"^[^\x00]*$")),
            (r"\\0", None),
            (r"\012", None),
            (r"\08", Some(r"\x008")),
            (r"\x00", None),
            (r"^[a-z]+$", None),
            (r"tail\", None),
            ("é\\0", Some(r"é\x00")),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_nul_escape(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn lookaround_removal() {
        let cases = [
            (
                r"^(?!\.\.?(?:\/|$))[A-Za-z0-9_\-.~:@+]{1,200}$",
                Some(r"^[A-Za-z0-9_\-.~:@+]{1,200}$"),
            ),
            (
                r"^(?!\.\.?(?:\/|$))[A-Za-z0-9_\-.~:@+]{1,200}(?:\/(?!\.\.?(?:\/|$))[A-Za-z0-9_\-.~:@+]{1,200}){0,14}$",
                Some(r"^[A-Za-z0-9_\-.~:@+]{1,200}(?:\/[A-Za-z0-9_\-.~:@+]{1,200}){0,14}$"),
            ),
            (r"^(?=.*\d).{8,}$", Some(r"^.{8,}$")),
            (r"(?<=\$)\d+", Some(r"\d+")),
            (r"(?<!x)y", Some(r"y")),
            (r"(?!(a|b)c)d", Some(r"d")),
            (r"(?!\))x", Some(r"x")),
            (r"(?![)])x", Some(r"x")),
            (r"(?![]a])x", Some(r"x")),
            (r"(?=b)+a", Some(r"a")),
            (r"a(?=b)+", Some(r"a")),
            (r"(?!x){2,3}?y", Some(r"y")),
            (r"(?!x){2,}y", Some(r"y")),
            (r"(?!x){2}y", Some(r"y")),
            (r"(?!x){2,y", Some(r"{2,y")),
            (r"(?!abc", None),
            (r"^(?:a|b)$", None),
            (r"(?<year>\d{4})", None),
            (r"(?i)abc", None),
            (r"[(?!]x", None),
            (r"\(\?!x", None),
            (r"^[a-z]+$", None),
            ("", None),
            (r"(?!x)[abc", Some("[abc")),
            (r"(?!x){a", Some("{a")),
        ];
        for (input, want) in cases {
            assert_eq!(strip_lookaround(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn combined_rewrites_in_order() {
        let cfg = policy(true, true);
        assert_eq!(
            rewrite_pattern(r"^(?!\0)[^\0]*$", &cfg).as_deref(),
            Some(r"^[^\x00]*$")
        );
        assert_eq!(rewrite_pattern(r"^[a-z]+$", &cfg), None);
    }

    #[test]
    fn walks_nested_schemas() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": r"^[^\0]*$",
                    "format": "uri"
                }
            },
            "items": { "pattern": r"(?!x)y" },
            "anyOf": [{ "pattern": r"(?=a)b" }],
            "$defs": {
                "nested": { "type": "string", "format": "date-time" }
            },
            "additionalProperties": { "pattern": r"^(?!x)z$" }
        });
        let cfg = ToolSchemaConfig {
            normalize_nul_escape: true,
            strip_lookaround: true,
            string_formats: Some(vec!["date-time".to_string()]),
            ..Default::default()
        };
        assert!(sanitize_schema(&mut schema, &cfg));
        assert_eq!(schema["properties"]["id"]["pattern"], r"^[^\x00]*$");
        assert!(schema["properties"]["id"].get("format").is_none());
        assert_eq!(schema["items"]["pattern"], "y");
        assert_eq!(schema["anyOf"][0]["pattern"], "b");
        assert_eq!(schema["$defs"]["nested"]["format"], "date-time");
        assert_eq!(schema["additionalProperties"]["pattern"], "^z$");
    }

    #[test]
    fn property_and_instance_names_are_not_keywords() {
        let cfg = ToolSchemaConfig {
            drop_pattern: true,
            string_formats: Some(vec![]),
            ..Default::default()
        };
        let mut schema = json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "format": { "type": "number" }
            },
            "enum": [{ "format": "uuid", "pattern": "x" }],
            "default": { "format": "uuid" }
        });
        assert!(!sanitize_schema(&mut schema, &cfg));
        assert!(schema["properties"].get("pattern").is_some());
        assert!(schema["properties"].get("format").is_some());
        assert_eq!(schema["enum"][0]["format"], "uuid");
    }

    #[test]
    fn numeric_formats_are_kept() {
        let mut schema = json!({
            "type": "integer",
            "format": "int64",
            "properties": {
                "ratio": { "type": ["number", "null"], "format": "double" }
            }
        });
        let cfg = ToolSchemaConfig {
            string_formats: Some(vec![]),
            ..Default::default()
        };
        assert!(!sanitize_schema(&mut schema, &cfg));
        assert_eq!(schema["format"], "int64");
        assert_eq!(schema["properties"]["ratio"]["format"], "double");
    }

    #[test]
    fn drop_pattern_removes_the_keyword() {
        let mut schema = json!({
            "type": "object",
            "properties": { "id": { "type": "string", "pattern": "^a$" } }
        });
        let cfg = ToolSchemaConfig {
            drop_pattern: true,
            normalize_nul_escape: true,
            ..Default::default()
        };
        assert!(sanitize_schema(&mut schema, &cfg));
        assert!(schema["properties"]["id"].get("pattern").is_none());
    }

    #[test]
    fn noop_policy_touches_nothing() {
        let mut schema = json!({ "pattern": r"^[^\0]*$" });
        let before = schema.clone();
        assert!(!sanitize_schema(&mut schema, &ToolSchemaConfig::default()));
        assert_eq!(schema, before);
    }

    #[test]
    fn non_string_keyword_values_are_ignored() {
        let mut schema = json!({ "pattern": 5, "format": 5 });
        let cfg = ToolSchemaConfig {
            drop_pattern: true,
            string_formats: Some(vec![]),
            ..Default::default()
        };
        assert!(!sanitize_schema(&mut schema, &cfg));
        assert_eq!(schema, json!({ "pattern": 5, "format": 5 }));
    }

    #[test]
    fn numeric_type_detection() {
        assert!(is_numeric_type(Some(&json!("integer"))));
        assert!(is_numeric_type(Some(&json!("number"))));
        assert!(is_numeric_type(Some(&json!(["integer", "null"]))));
        assert!(!is_numeric_type(Some(&json!(["integer", "string"]))));
        assert!(!is_numeric_type(Some(&json!(true))));
        assert!(!is_numeric_type(None));
    }

    #[test]
    fn format_without_policy_is_kept() {
        let cfg = ToolSchemaConfig {
            string_formats: None,
            ..Default::default()
        };
        assert!(!drops_format(&cfg, "uuid", None));
    }

    #[test]
    fn char_len_by_utf8_prefix() {
        assert_eq!(char_len(0x41), 1);
        assert_eq!(char_len(0xc3), 2);
        assert_eq!(char_len(0xe2), 3);
        assert_eq!(char_len(0xf0), 4);
        assert_eq!(char_len(0x80), 1);
    }
}
