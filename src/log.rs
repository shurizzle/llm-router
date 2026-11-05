use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

/// Writes one JSON log line to stdout, shaped like Go's slog JSON handler.
pub fn json(level: &str, msg: &str, fields: Vec<(&str, Value)>) {
    let mut map = Map::new();
    map.insert(
        "time".to_string(),
        Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, false)),
    );
    map.insert("level".to_string(), Value::String(level.to_string()));
    map.insert("msg".to_string(), Value::String(msg.to_string()));
    for (k, v) in fields {
        map.insert(k.to_string(), v);
    }
    println!("{}", Value::Object(map));
}

pub fn info(msg: &str, fields: Vec<(&str, Value)>) {
    json("INFO", msg, fields);
}

pub fn warn(msg: &str, fields: Vec<(&str, Value)>) {
    json("WARN", msg, fields);
}

pub fn error(msg: &str, fields: Vec<(&str, Value)>) {
    json("ERROR", msg, fields);
}
