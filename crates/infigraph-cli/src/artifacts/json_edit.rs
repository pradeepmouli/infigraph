//! Edit a user's JSON config file so that only the keys install owns change
//! (#171).
//!
//! Install used to parse the target into a `serde_json::Value`, merge its keys
//! in, and write back `to_string_pretty` of the whole value. `serde_json::Map`
//! is a `BTreeMap`, so every object in the file came back sorted; floats were
//! re-printed from `f64` (`124.35806450000003` became `...04`); and a JSONC file
//! with comments could not be touched at all. A `JsonDoc` keeps the parsed
//! concrete syntax tree beside the value: callers still mutate `value` exactly
//! as before, and `render` applies only the *difference* between the value as
//! read and the value as left back onto that tree. Everything install did not
//! touch -- key order, number text, whitespace, comments -- is reproduced byte
//! for byte.

use jsonc_parser::cst::{CstArray, CstInputValue, CstObject, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::{Map, Value};

pub(crate) struct JsonDoc {
    /// `None` for a file that does not exist yet: there is nothing to
    /// preserve, so `render` pretty-prints.
    tree: Option<CstRootNode>,
    original: Value,
    pub(crate) value: Value,
}

impl JsonDoc {
    /// A document for a target that does not exist yet.
    pub(crate) fn new_file() -> Self {
        Self {
            tree: None,
            original: Value::Object(Map::new()),
            value: Value::Object(Map::new()),
        }
    }

    /// Parse an existing file's text. JSONC (comments, trailing commas) is
    /// accepted, since `render` writes them back untouched. An empty file is
    /// an empty object. `Err` carries the parse error for the caller's skip
    /// message.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let tree = CstRootNode::parse(text, &ParseOptions::default()).map_err(|e| e.to_string())?;
        let original = tree
            .to_serde_value()
            .unwrap_or_else(|| Value::Object(Map::new()));
        Ok(Self {
            tree: Some(tree),
            value: original.clone(),
            original,
        })
    }

    /// `parse`, or `new_file` when there is no file.
    pub(crate) fn parse_or_new(text: Option<&str>) -> Result<Self, String> {
        text.map_or_else(|| Ok(Self::new_file()), Self::parse)
    }

    /// The file's new text: the original with only the changed values edited,
    /// or a pretty-printed new file.
    pub(crate) fn render(self) -> String {
        let Some(tree) = self.tree else {
            return serde_json::to_string_pretty(&self.value).expect("a Value always serializes");
        };
        if self.value != self.original {
            match (&self.original, &self.value, tree.object_value()) {
                (Value::Object(old), Value::Object(new), Some(object)) => {
                    edit_object(&object, old, new)
                }
                _ => tree.set_value(to_input(&self.value)),
            }
        }
        tree.to_string()
    }
}

fn edit_object(object: &CstObject, old: &Map<String, Value>, new: &Map<String, Value>) {
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        if let Some(prop) = object.get(key) {
            prop.remove();
        }
    }
    for (key, new_value) in new {
        let Some(old_value) = old.get(key) else {
            object.append(key, to_input(new_value));
            continue;
        };
        if old_value == new_value {
            continue;
        }
        let Some(prop) = object.get(key) else {
            continue;
        };
        match (old_value, new_value) {
            (Value::Object(old), Value::Object(new)) => match prop.object_value() {
                Some(child) => edit_object(&child, old, new),
                None => prop.set_value(to_input(new_value)),
            },
            (Value::Array(old), Value::Array(new)) => match prop.array_value() {
                Some(child) => edit_array(&child, old, new),
                None => prop.set_value(to_input(new_value)),
            },
            _ => prop.set_value(to_input(new_value)),
        }
    }
}

/// Keep the old elements that match `new` in order, remove the rest, then
/// append whatever of `new` is left. Both of install's array edits have this
/// shape -- merge filters out its owned entries and appends its own,
/// uninstall only filters -- so a user's entries are never rewritten.
fn edit_array(array: &CstArray, old: &[Value], new: &[Value]) {
    let mut matched = 0;
    let mut doomed = Vec::new();
    for (element, old_value) in array.elements().into_iter().zip(old) {
        if new.get(matched) == Some(old_value) {
            matched += 1;
        } else {
            doomed.push(element);
        }
    }
    for element in doomed.into_iter().rev() {
        element.remove();
    }
    for value in &new[matched..] {
        array.append(to_input(value));
    }
}

fn to_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), to_input(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(text: &str, change: impl FnOnce(&mut Value)) -> String {
        let mut doc = JsonDoc::parse(text).expect("fixture parses");
        change(&mut doc.value);
        doc.render()
    }

    const USER_SETTINGS: &str = r#"{
  "env": {
    "FOO": "1"
  },
  "lastCost": 124.35806450000003,
  "agentPushNotifEnabled": true
}"#;

    #[test]
    fn an_unchanged_document_renders_byte_for_byte() {
        let text = "{\n  // keep me\n  \"b\": 1,\n  \"a\": [1, 2,],\n}\n";
        assert_eq!(edit(text, |_| {}), text);
    }

    #[test]
    fn adding_a_key_keeps_every_other_key_in_place_and_every_number_as_written() {
        let out = edit(USER_SETTINGS, |v| {
            v["mcpServers"] = json!({"infigraph": {"command": "infigraph-mcp"}});
        });
        assert!(
            out.starts_with(
                "{\n  \"env\": {\n    \"FOO\": \"1\"\n  },\n  \"lastCost\": 124.35806450000003,\n  \"agentPushNotifEnabled\": true,"
            ),
            "the user's keys moved or were re-printed:\n{out}"
        );
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["mcpServers"]["infigraph"]["command"],
            "infigraph-mcp"
        );
    }

    #[test]
    fn comments_survive_an_edit() {
        let text = "{\n  // user's note\n  \"keep\": 1,\n  \"owned\": 2\n}";
        let out = edit(text, |v| {
            v.as_object_mut().unwrap().remove("owned");
        });
        assert!(out.contains("// user's note"), "{out}");
        assert!(out.contains("\"keep\": 1"), "{out}");
        assert!(!out.contains("owned"), "{out}");
    }

    #[test]
    fn a_nested_change_edits_only_that_leaf() {
        let text = "{\n  \"z\": {\"b\": 1, \"a\": 2},\n  \"y\": 3\n}";
        let out = edit(text, |v| v["z"]["a"] = json!(20));
        assert_eq!(out, "{\n  \"z\": {\"b\": 1, \"a\": 20},\n  \"y\": 3\n}");
    }

    #[test]
    fn filtering_and_appending_an_array_leaves_the_users_entries_as_written() {
        let text =
            "{\"allow\": [\n  \"Bash(ls:*)\",\n  \"mcp__infigraph__old\",\n  \"Read(  x  )\"\n]}";
        let out = edit(text, |v| {
            v["allow"] = json!(["Bash(ls:*)", "Read(  x  )", "mcp__infigraph__search"]);
        });
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["allow"],
            json!(["Bash(ls:*)", "Read(  x  )", "mcp__infigraph__search"])
        );
        assert!(
            out.starts_with("{\"allow\": [\n  \"Bash(ls:*)\",\n  \"Read(  x  )\""),
            "{out}"
        );
    }

    #[test]
    fn a_new_file_is_pretty_printed() {
        let mut doc = JsonDoc::new_file();
        doc.value["a"] = json!(1);
        assert_eq!(doc.render(), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn an_empty_file_is_an_empty_object() {
        let doc = JsonDoc::parse("").unwrap();
        assert_eq!(doc.value, json!({}));
    }

    #[test]
    fn malformed_json_is_reported_not_rebuilt() {
        assert!(JsonDoc::parse("{\"a\": ").is_err());
    }
}
