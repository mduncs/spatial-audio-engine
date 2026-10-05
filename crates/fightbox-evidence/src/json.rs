//! Deterministic JSON string construction without a serialization framework.
//!
//! These helpers escape the characters JSON requires and leave all other UTF-8
//! untouched, so identical input always produces identical bytes. They are
//! intentionally narrow: they serve evidence/manifest/listening output only,
//! and never accept unstructured caller JSON.

/// Escape a string's contents for placement inside a JSON string literal. The
/// surrounding quotes are added by [`json_string`].
fn escape_into(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                // Deterministic \u00XX escape for every remaining control char.
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// A JSON string literal (with surrounding quotes) for `value`.
#[must_use]
pub fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    escape_into(&mut out, value);
    out.push('"');
    out
}

/// Join string slices into a JSON array of string literals, e.g. `["a","b"]`.
#[must_use]
pub fn json_string_array<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for item in items {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&json_string(item));
    }
    out.push(']');
    out
}

/// A JSON boolean literal.
#[must_use]
pub fn json_bool(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// A finite `f32` as a JSON number, or `null` for NaN/Inf. Rust's `{}` float
/// formatting yields the shortest round-tripping representation, which is
/// deterministic across runs for a given value.
#[must_use]
pub fn json_f32(value: f32) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "null".into()
    }
}

/// An optional string as a JSON string literal or `null`.
#[must_use]
pub fn json_opt_string(value: Option<&str>) -> String {
    match value {
        Some(v) => json_string(v),
        None => "null".into(),
    }
}

/// An optional finite `f32` as a JSON number or `null`.
#[must_use]
pub fn json_opt_f32(value: Option<f32>) -> String {
    match value {
        Some(v) => json_f32(v),
        None => "null".into(),
    }
}

/// A tiny ordered JSON object builder. Keys are appended in caller order, so
/// determinism is the caller's responsibility (record types below use a fixed
/// field order). Values are inserted as already-serialized JSON fragments.
pub struct JsonObject {
    out: String,
    need_comma: bool,
}

impl JsonObject {
    #[must_use]
    pub fn new() -> Self {
        Self {
            out: String::from("{"),
            need_comma: false,
        }
    }

    fn raw(&mut self, key: &str, value: &str) {
        if self.need_comma {
            self.out.push(',');
        }
        self.need_comma = true;
        self.out.push_str(&json_string(key));
        self.out.push(':');
        self.out.push_str(value);
    }

    /// Insert an already-serialized JSON fragment (object, array, number, etc.).
    pub fn raw_value(&mut self, key: &str, value: &str) {
        self.raw(key, value);
    }

    /// Insert a JSON string.
    pub fn str(&mut self, key: &str, value: &str) {
        self.raw(key, &json_string(value));
    }

    /// Insert an optional JSON string (`null` when absent).
    pub fn opt_str(&mut self, key: &str, value: Option<&str>) {
        self.raw(key, &json_opt_string(value));
    }

    /// Insert a JSON number from a `u32`.
    pub fn num_u32(&mut self, key: &str, value: u32) {
        self.raw(key, &format!("{value}"));
    }

    /// Insert a JSON number from a `usize`.
    pub fn num_usize(&mut self, key: &str, value: usize) {
        self.raw(key, &format!("{value}"));
    }

    /// Insert a JSON number from a `u64`.
    pub fn num_u64(&mut self, key: &str, value: u64) {
        self.raw(key, &format!("{value}"));
    }

    /// Insert a finite `f32` JSON number (NaN/Inf become `null`).
    pub fn num_f32(&mut self, key: &str, value: f32) {
        self.raw(key, &json_f32(value));
    }

    /// Insert an optional finite `f32` JSON number.
    pub fn opt_f32(&mut self, key: &str, value: Option<f32>) {
        self.raw(key, &json_opt_f32(value));
    }

    /// Insert a JSON boolean.
    pub fn boolean(&mut self, key: &str, value: bool) {
        self.raw(key, json_bool(value));
    }

    #[must_use]
    pub fn finish(mut self) -> String {
        self.out.push('}');
        self.out
    }
}

impl Default for JsonObject {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_required_characters() {
        assert_eq!(json_string("a\"b\\c\n"), r#""a\"b\\c\n""#);
        assert_eq!(json_string("\u{08}\u{0c}"), r#""\b\f""#);
        assert_eq!(json_string("\u{01}"), r#""\u0001""#);
    }

    #[test]
    fn arrays_and_bools_are_stable() {
        assert_eq!(json_string_array(["x", "y"].into_iter()), r#"["x","y"]"#);
        assert_eq!(json_bool(true), "true");
    }

    #[test]
    fn non_finite_floats_become_null() {
        assert_eq!(json_f32(f32::NAN), "null");
        assert_eq!(json_f32(f32::INFINITY), "null");
        assert_eq!(json_opt_f32(None), "null");
        assert_eq!(json_f32(1.5), "1.5");
    }

    #[test]
    fn object_builder_emits_fixed_order() {
        let mut o = JsonObject::new();
        o.str("a", "1");
        o.num_u32("b", 2);
        o.num_u64("d", 4);
        o.boolean("c", true);
        assert_eq!(o.finish(), r#"{"a":"1","b":2,"d":4,"c":true}"#);
    }
}
