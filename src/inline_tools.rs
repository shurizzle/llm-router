//! Rewrites mid-conversation tool blocks for upstreams without the inline-tools beta.

use serde_json::Value;

use crate::log;

/// Moves by-value tool definitions from mid-conversation messages into `added` and drops the
/// blocks an upstream without the beta cannot parse.
///
/// `tool_addition` with a `tool_definition` carries a full `tools[]` entry inline; it becomes a
/// regular `tools` entry instead. `tool_removal` has no equivalent once the definition is in the
/// static `tools` array, so it is dropped and the tool stays available. `tool_reference` blocks
/// are kept: the upstream already resolves that variant.
///
/// Returns true when `messages` changed.
pub fn hoist_inline_tools(messages: &mut Value, added: &mut Vec<Value>) -> bool {
    let Some(messages) = messages.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    let mut emptied = Vec::new();
    for (index, message) in messages.iter_mut().enumerate() {
        let Some(content) = message
            .as_object_mut()
            .and_then(|object| object.get_mut("content"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let before = content.len();
        content.retain(|block| keep_block(block, added));
        if content.len() == before {
            continue;
        }
        changed = true;
        if content.is_empty() {
            emptied.push(index);
        }
    }
    for index in emptied.into_iter().rev() {
        messages.remove(index);
    }
    changed
}

/// Reports whether a content block survives the rewrite, collecting a hoisted definition.
fn keep_block(block: &Value, added: &mut Vec<Value>) -> bool {
    let Some(object) = block.as_object() else {
        return true;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("tool_removal") => {
            let name = object
                .get("tool")
                .and_then(|tool| tool.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            log::warn(
                "dropping tool_removal block unsupported by the upstream",
                vec![("tool", Value::String(name.to_string()))],
            );
            false
        }
        Some("tool_addition") => {
            let tool = object.get("tool");
            let by_value = tool
                .and_then(|tool| tool.get("type"))
                .and_then(Value::as_str)
                == Some("tool_definition");
            if !by_value {
                return true;
            }
            match tool.and_then(|tool| tool.get("definition")) {
                Some(definition) if definition.get("name").and_then(Value::as_str).is_some() => {
                    added.push(definition.clone());
                }
                _ => log::warn(
                    "dropping tool_addition without a hoistable tool definition",
                    Vec::new(),
                ),
            }
            false
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hoist(messages: &mut Value) -> (bool, Vec<Value>) {
        let mut added = Vec::new();
        let changed = hoist_inline_tools(messages, &mut added);
        (changed, added)
    }

    #[test]
    fn hoists_tool_definition_and_keeps_text() {
        let definition = json!({"name": "db_query", "input_schema": {"type": "object"}});
        let mut messages = json!([
            {"role": "user", "content": "hi"},
            {"role": "system", "content": [
                {"type": "text", "text": "reminder"},
                {"type": "tool_addition", "tool": {"type": "tool_definition", "definition": definition}}
            ]}
        ]);
        let (changed, added) = hoist(&mut messages);
        assert!(changed);
        assert_eq!(added, vec![definition]);
        assert_eq!(
            messages,
            json!([
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [{"type": "text", "text": "reminder"}]}
            ])
        );
    }

    #[test]
    fn drops_tool_removal() {
        let mut messages = json!([
            {"role": "system", "content": [
                {"type": "tool_removal", "tool": {"type": "tool_reference", "name": "old"}}
            ]}
        ]);
        let (changed, added) = hoist(&mut messages);
        assert!(changed);
        assert!(added.is_empty());
        assert_eq!(messages, json!([]));
    }

    #[test]
    fn keeps_tool_reference() {
        let mut messages = json!([
            {"role": "system", "content": [
                {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "kept"}}
            ]}
        ]);
        let (changed, added) = hoist(&mut messages);
        assert!(!changed);
        assert!(added.is_empty());
        assert_eq!(
            messages,
            json!([
                {"role": "system", "content": [
                    {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "kept"}}
                ]}
            ])
        );
    }

    #[test]
    fn drops_unhoistable_definition() {
        let mut messages = json!([
            {"role": "system", "content": [
                {"type": "tool_addition", "tool": {"type": "tool_definition", "definition": {"type": "mcp_toolset", "mcp_server_name": "calendar"}}}
            ]}
        ]);
        let (changed, added) = hoist(&mut messages);
        assert!(changed);
        assert!(added.is_empty());
        assert_eq!(messages, json!([]));
    }

    #[test]
    fn leaves_other_content_untouched() {
        let original = json!([
            {"role": "user", "content": [{"type": "text", "text": "x"}]},
            {"role": "assistant", "content": "plain"}
        ]);
        let mut messages = original.clone();
        let (changed, added) = hoist(&mut messages);
        assert!(!changed);
        assert!(added.is_empty());
        assert_eq!(messages, original);
    }

    #[test]
    fn non_array_messages_are_untouched() {
        let mut messages = json!({"role": "user"});
        assert!(!hoist_inline_tools(&mut messages, &mut Vec::new()));
        assert_eq!(messages, json!({"role": "user"}));
    }

    #[test]
    fn non_object_blocks_are_kept() {
        let mut messages = json!([
            {"role": "user", "content": ["plain", 7]}
        ]);
        let (changed, added) = hoist(&mut messages);
        assert!(!changed);
        assert!(added.is_empty());
    }
}
