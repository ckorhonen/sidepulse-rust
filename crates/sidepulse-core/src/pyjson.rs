//! Helpers that reproduce the Python implementation's JSON and string semantics,
//! so records written by the Rust hook are byte-compatible with the Python ones.

use serde_json::{Map, Value};

pub type Object = Map<String, Value>;

/// `json.dumps(v, separators=(",", ":"), ensure_ascii=False)`.
pub fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

/// `json.dumps(v, ensure_ascii=False, sort_keys=True)` (default `", "` / `": "` separators).
pub fn sorted_default(value: &Value) -> String {
    let mut out = String::new();
    write_sorted(value, &mut out);
    out
}

fn write_sorted(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&compact(&Value::String((*key).clone())));
                out.push_str(": ");
                write_sorted(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_sorted(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&compact(other)),
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `a or b` over optional JSON values.
pub fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) {
        a
    } else {
        b
    }
}

/// Python `str(value)` for a JSON value (`None` -> "None", `True` -> "True").
pub fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => compact(other),
    }
}

/// `str(raw.get(key, default))`.
pub fn get_str_or(obj: &Object, key: &str, default: &str) -> String {
    obj.get(key)
        .map(py_str)
        .unwrap_or_else(|| default.to_string())
}

/// `_string_or_none(value)`: `None` stays `None`, everything else is `str()`-ed.
pub fn string_or_none(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(v) => Some(py_str(v)),
    }
}

/// `_first_string(data, *keys)`: first key whose `str()` value is non-empty.
pub fn first_string(obj: &Object, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| string_or_none(obj.get(*k)))
        .find(|s| !s.is_empty())
}

/// `" ".join(value.split())` for a string value, `None` when empty or not a string.
pub fn clean_label(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => {
            let text = s.split_whitespace().collect::<Vec<_>>().join(" ");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// Python `str.splitlines()` (without keepends).
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_break {
            lines.push(&text[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = iter.peek() {
                    iter.next();
                    next = j + 1;
                }
            }
            start = next;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// First `n` characters (Python `s[:n]`).
pub fn prefix_chars(text: &str, n: usize) -> &str {
    match text.char_indices().nth(n) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

/// Python `str.title()` approximation used for unknown provider labels.
pub fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_cased = false;
    for c in text.chars() {
        if prev_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        prev_cased = c.is_alphabetic();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorted_default_matches_python() {
        let v = json!({"b": 1, "a": {"d": [1, "x"], "c": null}, "é": true});
        assert_eq!(
            sorted_default(&v),
            r#"{"a": {"c": null, "d": [1, "x"]}, "b": 1, "é": true}"#
        );
    }

    #[test]
    fn compact_keeps_insertion_order_and_unicode() {
        let v: Value = serde_json::from_str(r#"{"z":1,"a":"é\n","n":1.50}"#).unwrap();
        assert_eq!(compact(&v), r#"{"z":1,"a":"é\n","n":1.5}"#);
    }

    #[test]
    fn splitlines_handles_crlf_and_trailing() {
        assert_eq!(splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn py_str_matches_python() {
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(12)), "12");
    }
}
