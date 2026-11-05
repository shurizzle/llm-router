//! Deterministic rewriting of tool-use ids for Anthropic's `^[a-zA-Z0-9_-]+$` requirement.
//!
//! Ported from maximhq/bifrost (Apache-2.0), `core/providers/utils/utils.go`
//! (`SanitizeAnthropicToolUseID`). Useful when a conversation produced by another backend is
//! replayed to Anthropic: ids such as `functions.Bash:0` are rejected with `400`.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Anthropic caps tool identifiers at 64 characters.
pub const MAX_ID_LEN: usize = 64;

fn is_conforming(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Returns the rewritten id, or None when the id already conforms.
///
/// The output is deterministic, so a `tool_use` and its matching `tool_result` map to the
/// same value.
pub fn sanitize_tool_use_id(id: &str) -> Option<String> {
    if is_conforming(id) {
        return None;
    }
    // 64-bit prefix of the digest, as 16 lowercase hex digits.
    let digest = Sha256::digest(id.as_bytes());
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let hash = format!("{:016x}", u64::from_be_bytes(head));

    let semantic = semantic_part(id);
    if semantic.is_empty() {
        return Some(hash);
    }
    let max_semantic = MAX_ID_LEN - hash.len() - 1;
    let mut semantic = semantic;
    if semantic.len() > max_semantic {
        semantic.truncate(max_semantic);
        semantic.truncate(semantic.trim_end_matches('_').len());
    }
    if semantic.is_empty() {
        return Some(hash);
    }
    Some(format!("{hash}_{semantic}"))
}

/// Keeps the allowed characters, collapsing each run of disallowed ones into a single `_`,
/// then trims `_` from both ends.
fn semantic_part(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut separator = false;
    for c in id.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            if separator && !out.is_empty() {
                out.push('_');
            }
            separator = false;
            out.push(c);
        } else {
            separator = true;
        }
    }
    out
}

/// Rewrites `tool_use.id` and `tool_result.tool_use_id` inside a `messages` array.
/// Returns whether anything changed.
pub fn rewrite_message_tool_ids(messages: &mut Value) -> bool {
    let Some(list) = messages.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for message in list {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content {
            let Some(object) = block.as_object_mut() else {
                continue;
            };
            let field = match object.get("type").and_then(Value::as_str) {
                Some("tool_use") => "id",
                Some("tool_result") => "tool_use_id",
                _ => continue,
            };
            let id = match object.get(field) {
                Some(Value::String(id)) => id.clone(),
                _ => continue,
            };
            if let Some(rewritten) = sanitize_tool_use_id(&id) {
                object.insert(field.to_string(), Value::String(rewritten));
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hash_of(id: &str) -> String {
        let digest = Sha256::digest(id.as_bytes());
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        format!("{:016x}", u64::from_be_bytes(head))
    }

    #[test]
    fn conforming_ids_are_untouched() {
        for id in ["toolu_01AbC-2", "abc", "a_b-c", "0"] {
            assert_eq!(sanitize_tool_use_id(id), None, "{id}");
        }
    }

    #[test]
    fn disallowed_characters_are_rewritten() {
        assert_eq!(
            sanitize_tool_use_id("functions.Bash:0").unwrap(),
            format!("{}_functions_Bash_0", hash_of("functions.Bash:0"))
        );
        assert_eq!(
            sanitize_tool_use_id("call/../x").unwrap(),
            format!("{}_call_x", hash_of("call/../x"))
        );
        assert_eq!(
            sanitize_tool_use_id("ümlaut").unwrap(),
            format!("{}_mlaut", hash_of("ümlaut"))
        );
    }

    #[test]
    fn empty_id_becomes_the_hash() {
        assert_eq!(sanitize_tool_use_id("").unwrap(), hash_of(""));
    }

    #[test]
    fn result_is_deterministic_and_bounded() {
        let long = "functions.".repeat(40);
        let first = sanitize_tool_use_id(&long).unwrap();
        assert_eq!(first, sanitize_tool_use_id(&long).unwrap());
        assert!(first.len() <= MAX_ID_LEN, "{first}");
        assert!(first.starts_with(&hash_of(&long)));
        assert!(!first.ends_with('_'));
    }

    #[test]
    fn rewrites_only_tool_blocks() {
        let mut messages = json!([
            { "role": "user", "content": "plain" },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "hi" },
                    { "type": "tool_use", "id": "functions.Bash:0", "name": "Bash", "input": {} }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "functions.Bash:0", "content": "ok" },
                    { "type": "tool_result", "tool_use_id": "already_ok", "content": "ok" }
                ]
            }
        ]);
        assert!(rewrite_message_tool_ids(&mut messages));
        let rewritten = sanitize_tool_use_id("functions.Bash:0").unwrap();
        assert_eq!(messages[1]["content"][0]["text"], "hi");
        assert_eq!(messages[1]["content"][1]["id"], rewritten);
        assert_eq!(messages[2]["content"][0]["tool_use_id"], rewritten);
        assert_eq!(messages[2]["content"][1]["tool_use_id"], "already_ok");
    }

    #[test]
    fn no_change_reports_false() {
        let mut messages = json!([{ "role": "user", "content": "just text" }]);
        assert!(!rewrite_message_tool_ids(&mut messages));
        let mut not_an_array = json!({ "content": [] });
        assert!(!rewrite_message_tool_ids(&mut not_an_array));
        let mut odd_blocks = json!([{
            "role": "user",
            "content": ["plain", { "type": "text" }, { "type": "tool_use", "id": 1 }]
        }]);
        assert!(!rewrite_message_tool_ids(&mut odd_blocks));
    }
}
