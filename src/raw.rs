//! A JSON object that keeps key order and the original bytes of each value.

use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// Order-preserving JSON object. Replacing a key keeps its position; a new key is appended.
#[derive(Debug, Default, Clone)]
pub struct RawObject(Vec<(String, Box<RawValue>)>);

impl RawObject {
    pub fn get(&self, key: &str) -> Option<&RawValue> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref())
    }

    pub fn insert(&mut self, key: String, value: Box<RawValue>) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Box<RawValue>> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(index).1)
    }
}

impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RawObjectVisitor;

        impl<'de> Visitor<'de> for RawObjectVisitor {
            type Value = RawObject;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawObject, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(key) = map.next_key::<String>()? {
                    entries.push((key, map.next_value::<Box<RawValue>>()?));
                }
                Ok(RawObject(entries))
            }
        }

        deserializer.deserialize_map(RawObjectVisitor)
    }
}

impl Serialize for RawObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(input: &str) -> String {
        let object: RawObject = serde_json::from_str(input).expect("parse");
        serde_json::to_string(&object).expect("serialize")
    }

    #[test]
    fn preserves_key_order_and_value_bytes() {
        assert_eq!(roundtrip(r#"{"b":1,"a":2.50}"#), r#"{"b":1,"a":2.50}"#);
        assert_eq!(
            roundtrip(r#"{"model":"x","tools":[], "metadata":null}"#),
            r#"{"model":"x","tools":[],"metadata":null}"#
        );
    }

    #[test]
    fn accepts_empty_object() {
        assert_eq!(roundtrip("{}"), "{}");
    }

    #[test]
    fn insert_keeps_position_and_appends_new_keys() {
        let mut object: RawObject = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        object.insert(
            "a".to_string(),
            RawValue::from_string("9".to_string()).unwrap(),
        );
        object.insert(
            "c".to_string(),
            RawValue::from_string("3".to_string()).unwrap(),
        );
        assert_eq!(
            serde_json::to_string(&object).unwrap(),
            r#"{"a":9,"b":2,"c":3}"#
        );
    }

    #[test]
    fn remove_drops_only_the_key() {
        let mut object: RawObject = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert!(object.remove("a").is_some());
        assert!(object.remove("missing").is_none());
        assert_eq!(serde_json::to_string(&object).unwrap(), r#"{"b":2}"#);
        assert!(object.get("b").is_some());
    }

    #[test]
    fn rejects_non_objects() {
        assert!(serde_json::from_str::<RawObject>("[]").is_err());
        assert!(serde_json::from_str::<RawObject>("null").is_err());
    }
}
