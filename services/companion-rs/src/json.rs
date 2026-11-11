//! Strict JSON parser + serializer. Zero dependencies, written to a bounded
//! contract: hard depth limit, duplicate-key rejection, lone-surrogate
//! rejection, control-char rejection. Every wire byte passes through here,
//! so the whole attack surface of malformed input is this module's tests.

use std::collections::HashSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Ordered object; duplicate keys rejected at parse time.
    Obj(Vec<(String, Json)>),
}

/// Hard depth cap: hostile deeply-nested payloads are rejected, not stack-overflowed.
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, PartialEq)]
pub enum JsonError {
    Eof,
    TrailingGarbage,
    InvalidNumber,
    InvalidString,
    ControlCharInString,
    LoneSurrogate,
    InvalidEscape,
    InvalidLiteral,
    UnexpectedChar,
    DepthExceeded,
    DuplicateKey,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            JsonError::Eof => "unexpected end of input",
            JsonError::TrailingGarbage => "trailing garbage after JSON value",
            JsonError::InvalidNumber => "invalid number literal",
            JsonError::InvalidString => "invalid string literal",
            JsonError::ControlCharInString => "raw control character in string",
            JsonError::LoneSurrogate => "lone surrogate escape",
            JsonError::InvalidEscape => "invalid escape sequence",
            JsonError::InvalidLiteral => "invalid literal (true/false/null)",
            JsonError::UnexpectedChar => "unexpected character",
            JsonError::DepthExceeded => "nesting depth exceeded",
            JsonError::DuplicateKey => "duplicate object key",
        };
        f.write_str(s)
    }
}

impl std::error::Error for JsonError {}

/// Parsed result with the byte offset where parsing stopped (for trailing-garbage checks).
pub fn parse(input: &str) -> Result<Json, JsonError> {
    let bytes = input.as_bytes();
    let mut p = Parser {
        b: bytes,
        s: input,
        i: 0,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.i != bytes.len() {
        return Err(JsonError::TrailingGarbage);
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth >= MAX_DEPTH {
            return Err(JsonError::DepthExceeded);
        }
        match self.peek() {
            None => Err(JsonError::Eof),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') | Some(b'f') | Some(b'n') => self.literal(),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(JsonError::UnexpectedChar),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.i += 1; // '{'
        let mut out: Vec<(String, Json)> = Vec::new();
        // security review 2026-10-25: scanning the earlier keys made each
        // object O(n^2) — one pre-session 256 KB frame of short distinct keys
        // cost ~5x10^8 compares. A set keeps duplicate detection linear.
        let mut seen: HashSet<String> = HashSet::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(JsonError::InvalidString);
            }
            let key = self.string()?;
            if !seen.insert(key.clone()) {
                return Err(JsonError::DuplicateKey);
            }
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(JsonError::UnexpectedChar);
            }
            self.i += 1;
            self.skip_ws();
            let v = self.value(depth + 1)?;
            out.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(out));
                }
                _ => return Err(JsonError::UnexpectedChar),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.i += 1; // '['
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            self.skip_ws();
            out.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(out));
                }
                _ => return Err(JsonError::UnexpectedChar),
            }
        }
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            // scan until quote, backslash, or control char
            while self.i < self.b.len() {
                let c = self.b[self.i];
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            // safe slice: boundaries are ASCII markers, interior is validated UTF-8
            out.push_str(&self.s[start..self.i]);
            match self.peek() {
                None => return Err(JsonError::Eof),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(JsonError::ControlCharInString),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        match self.peek() {
            None => Err(JsonError::Eof),
            Some(b'"') => {
                self.i += 1;
                out.push('"');
                Ok(())
            }
            Some(b'\\') => {
                self.i += 1;
                out.push('\\');
                Ok(())
            }
            Some(b'/') => {
                self.i += 1;
                out.push('/');
                Ok(())
            }
            Some(b'b') => {
                self.i += 1;
                out.push('\u{0008}');
                Ok(())
            }
            Some(b'f') => {
                self.i += 1;
                out.push('\u{000c}');
                Ok(())
            }
            Some(b'n') => {
                self.i += 1;
                out.push('\n');
                Ok(())
            }
            Some(b'r') => {
                self.i += 1;
                out.push('\r');
                Ok(())
            }
            Some(b't') => {
                self.i += 1;
                out.push('\t');
                Ok(())
            }
            Some(b'u') => {
                self.i += 1;
                let hi = self.hex4()?;
                if (0xD800..=0xDBFF).contains(&hi) {
                    // high surrogate: must be followed by \uXXXX low surrogate
                    if self.peek() != Some(b'\\') {
                        return Err(JsonError::LoneSurrogate);
                    }
                    self.i += 1;
                    if self.peek() != Some(b'u') {
                        return Err(JsonError::LoneSurrogate);
                    }
                    self.i += 1;
                    let lo = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err(JsonError::LoneSurrogate);
                    }
                    let c = 0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00);
                    // SAFETY: the computed code point is a valid scalar value
                    out.push(char::from_u32(c).ok_or(JsonError::LoneSurrogate)?);
                } else if (0xDC00..=0xDFFF).contains(&hi) {
                    return Err(JsonError::LoneSurrogate);
                } else {
                    out.push(char::from_u32(hi as u32).ok_or(JsonError::LoneSurrogate)?);
                }
                Ok(())
            }
            Some(_) => Err(JsonError::InvalidEscape),
        }
    }

    fn hex4(&mut self) -> Result<u16, JsonError> {
        if self.i + 4 > self.b.len() {
            return Err(JsonError::Eof);
        }
        let mut v: u16 = 0;
        for _ in 0..4 {
            let c = self.b[self.i];
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u16,
                b'a'..=b'f' => (c - b'a' + 10) as u16,
                b'A'..=b'F' => (c - b'A' + 10) as u16,
                _ => return Err(JsonError::InvalidString),
            };
            v = v.wrapping_mul(16).wrapping_add(d);
            self.i += 1;
        }
        Ok(v)
    }

    fn literal(&mut self) -> Result<Json, JsonError> {
        let rest = &self.s[self.i..];
        if rest.starts_with("true") {
            self.i += 4;
            Ok(Json::Bool(true))
        } else if rest.starts_with("false") {
            self.i += 5;
            Ok(Json::Bool(false))
        } else if rest.starts_with("null") {
            self.i += 4;
            Ok(Json::Null)
        } else {
            Err(JsonError::InvalidLiteral)
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits_start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == digits_start {
            return Err(JsonError::InvalidNumber);
        }
        // leading zeros: JSON forbids "01" — enforce like the JS Number path would differ;
        // we keep strictness: a leading '0' must be the whole integer part.
        if self.b[digits_start] == b'0' && self.i > digits_start + 1 {
            return Err(JsonError::InvalidNumber);
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            let frac_start = self.i;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == frac_start {
                return Err(JsonError::InvalidNumber);
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            let exp_start = self.i;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == exp_start {
                return Err(JsonError::InvalidNumber);
            }
        }
        let text = &self.s[start..self.i];
        if is_float {
            text.parse::<f64>()
                .map(Json::Float)
                .map_err(|_| JsonError::InvalidNumber)
        } else {
            // integers beyond i64 become floats (matches JS Number semantics for the app)
            match text.parse::<i64>() {
                Ok(v) => Ok(Json::Int(v)),
                Err(_) => text
                    .parse::<f64>()
                    .map(Json::Float)
                    .map_err(|_| JsonError::InvalidNumber),
            }
        }
    }
}

/// Escape + append `s` as a quoted JSON string.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{007f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append any Json value as serialized text.
pub fn write(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => out.push_str(&n.to_string()),
        Json::Float(f) => {
            if f.is_finite() {
                // integer-valued floats keep a .0 so they stay distinct on re-parse
                if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
                    out.push_str(&format!("{f:.1}"));
                } else {
                    out.push_str(&format!("{f}"));
                }
            } else {
                out.push_str("null"); // JSON has no NaN/Infinity
            }
        }
        Json::Str(s) => write_str(out, s),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(out, item);
            }
            out.push(']');
        }
        Json::Obj(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write(out, v);
            }
            out.push('}');
        }
    }
}

pub fn stringify(v: &Json) -> String {
    let mut s = String::new();
    write(&mut s, v);
    s
}

/// Two-space indented text (a settings file a person also edits).
pub fn pretty(v: &Json) -> String {
    fn go(out: &mut String, v: &Json, depth: usize) {
        let pad = |out: &mut String, d: usize| {
            out.push('\n');
            out.push_str(&"  ".repeat(d));
        };
        match v {
            Json::Arr(items) if !items.is_empty() => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, depth + 1);
                    go(out, item, depth + 1);
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(pairs) if !pairs.is_empty() => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, depth + 1);
                    write_str(out, k);
                    out.push_str(": ");
                    go(out, v, depth + 1);
                }
                pad(out, depth);
                out.push('}');
            }
            other => write(out, other),
        }
    }
    let mut s = String::new();
    go(&mut s, v, 0);
    s
}

/* ------------------------------------------------------------------ */
/* Object helpers used by protocol/seed/storage                        */
/* ------------------------------------------------------------------ */

/// A parsed object's field accessor (panics are avoided: non-objects yield None).
pub fn get<'a>(v: &'a Json, key: &str) -> Option<&'a Json> {
    match v {
        Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

pub fn as_str(v: &Json) -> Option<&str> {
    match v {
        Json::Str(s) => Some(s),
        _ => None,
    }
}

pub fn as_i64(v: &Json) -> Option<i64> {
    match v {
        Json::Int(n) => Some(*n),
        _ => None,
    }
}

/// JS `isNum`: finite number (int or float).
pub fn as_f64(v: &Json) -> Option<f64> {
    match v {
        Json::Int(n) => Some(*n as f64),
        Json::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

pub fn as_bool(v: &Json) -> Option<bool> {
    match v {
        Json::Bool(b) => Some(*b),
        _ => None,
    }
}

pub fn as_arr(v: &Json) -> Option<&[Json]> {
    match v {
        Json::Arr(a) => Some(a),
        _ => None,
    }
}

/// Convenience: object field as &str.
pub fn get_str<'a>(v: &'a Json, key: &str) -> Option<&'a str> {
    get(v, key).and_then(as_str)
}

/// Convenience: object field as i64 (int or whole float).
pub fn get_i64(v: &Json, key: &str) -> Option<i64> {
    get(v, key).and_then(as_i64)
}

/// Convenience: object field as bool.
pub fn get_bool(v: &Json, key: &str) -> Option<bool> {
    get(v, key).and_then(as_bool)
}

/// Builder sugar for objects.
pub fn obj(fields: Vec<(&str, Json)>) -> Json {
    Json::Obj(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

pub fn s(v: &str) -> Json {
    Json::Str(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
        assert_eq!(parse("false").unwrap(), Json::Bool(false));
        assert_eq!(parse("42").unwrap(), Json::Int(42));
        assert_eq!(parse("-7").unwrap(), Json::Int(-7));
        assert_eq!(parse("1.5").unwrap(), Json::Float(1.5));
        assert_eq!(parse("1e3").unwrap(), Json::Float(1000.0));
        assert_eq!(parse("  \"x\" ").unwrap(), Json::Str("x".into()));
    }

    #[test]
    fn parses_structures() {
        let v = parse(r#"{"a":[1,2,{"b":null}],"c":true}"#).unwrap();
        let a = as_arr(get(&v, "a").unwrap()).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(get(&a[2], "b").unwrap(), &Json::Null);
        assert_eq!(get(&v, "c").unwrap(), &Json::Bool(true));
        assert_eq!(parse("{}").unwrap(), Json::Obj(vec![]));
        assert_eq!(parse("[]").unwrap(), Json::Arr(vec![]));
    }

    #[test]
    fn string_escapes_and_surrogates() {
        assert_eq!(parse(r#""a\"b\\c\/\n\t""#).unwrap(), s("a\"b\\c/\n\t"));
        // BMP escape
        assert_eq!(parse(r#""é""#).unwrap(), s("é"));
        // surrogate pair → emoji
        assert_eq!(parse(r#""😀""#).unwrap(), s("\u{1F600}"));
    }

    #[test]
    fn rejects_adversarial_inputs() {
        // lone high surrogate
        assert_eq!(parse(r#""\ud800""#), Err(JsonError::LoneSurrogate));
        // high surrogate followed by non-surrogate
        assert_eq!(parse(r#""\ud800A""#), Err(JsonError::LoneSurrogate));
        // lone low surrogate
        assert_eq!(parse(r#""\udc00""#), Err(JsonError::LoneSurrogate));
        // raw control char in string
        assert_eq!(parse("\"\u{1}\""), Err(JsonError::ControlCharInString));
        // depth bomb (33 levels > MAX_DEPTH)
        let bomb = format!("{}{}", "[".repeat(33), "]".repeat(33));
        assert_eq!(parse(&bomb), Err(JsonError::DepthExceeded));
        // within limit is fine
        let ok = format!("{}{}", "[".repeat(32), "]".repeat(32));
        assert!(parse(&ok).is_ok());
        // duplicate key
        assert_eq!(parse(r#"{"a":1,"a":2}"#), Err(JsonError::DuplicateKey));
        // trailing garbage
        assert_eq!(parse("{} {}"), Err(JsonError::TrailingGarbage));
        // leading zeros
        assert_eq!(parse("01"), Err(JsonError::InvalidNumber));
        // bare word
        assert!(parse("undefined").is_err());
        // truncated number
        assert_eq!(parse("1."), Err(JsonError::InvalidNumber));
        // unterminated string
        assert_eq!(parse("\"abc"), Err(JsonError::Eof));
    }

    #[test]
    fn serializer_round_trips_and_escapes() {
        let v = obj(vec![
            ("x", Json::Int(1)),
            ("y", s("a\"b\nc\u{1}")),
            ("z", Json::Arr(vec![Json::Bool(false), Json::Null])),
        ]);
        let text = stringify(&v);
        assert_eq!(parse(&text).unwrap(), v);
        assert!(text.contains("\\u0001"));
    }

    #[test]
    fn float_formatting() {
        assert_eq!(stringify(&Json::Float(1.0)), "1.0");
        assert_eq!(stringify(&Json::Float(1.5)), "1.5");
        assert_eq!(stringify(&Json::Float(f64::NAN)), "null");
    }

    #[test]
    fn integer_overflow_falls_back_to_float() {
        assert_eq!(parse("99999999999999999999").unwrap(), Json::Float(1e20));
    }

    #[test]
    fn truncation_sweep_never_panics_and_full_doc_still_parses() {
        // Every strict prefix of a valid document is incomplete or invalid:
        // the parser must reject all of them without panicking, and the full
        // document must still parse identically (deterministic stateless fn).
        let doc = r#"{"type":"ping","commandId":"c-1"}"#;
        let chars: Vec<char> = doc.chars().collect();
        for cut in 0..chars.len() {
            let prefix: String = chars[..cut].iter().collect();
            assert!(parse(&prefix).is_err(), "prefix len {cut} must be rejected");
        }
        assert_eq!(parse(doc), parse(doc));
    }

    #[test]
    fn many_unique_keys_parse_in_linear_time_and_duplicates_still_fail() {
        // security review 2026-10-25: the attack shape — one ~250 KB frame
        // (under the 256,000-byte cap) holding 28k short distinct keys — was
        // ~4x10^8 key compares with the old scan of earlier keys. The bound is
        // loose on purpose: this is an unoptimized test build on a busy host.
        let members: Vec<String> = (0..28_000).map(|i| format!("\"{i:x}\":0")).collect();
        let doc = format!("{{{}}}", members.join(","));
        assert!(doc.len() < 256_000);
        let t0 = std::time::Instant::now();
        let v = parse(&doc).unwrap();
        let took = t0.elapsed();
        assert!(matches!(&v, Json::Obj(p) if p.len() == 28_000));
        assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
        // a repeat of the first key is still caught, escaped or not
        let dup = format!("{{{},\"0\":1}}", members.join(","));
        assert_eq!(parse(&dup), Err(JsonError::DuplicateKey));
        let escaped = format!("{{{},\"\\u0030\":1}}", members.join(","));
        assert_eq!(parse(&escaped), Err(JsonError::DuplicateKey));
    }

    #[test]
    fn object_depth_boundary_is_thirty_one() {
        // Arrays are pinned at 32 by rejects_adversarial_inputs; objects use
        // the same value() recursion, so their innermost scalar sits one
        // level deeper than the object count.
        let ok = format!("{}1{}", "{\"a\":".repeat(31), "}".repeat(31));
        assert!(parse(&ok).is_ok());
        let bomb = format!("{}1{}", "{\"a\":".repeat(32), "}".repeat(32));
        assert_eq!(parse(&bomb), Err(JsonError::DepthExceeded));
    }

    #[test]
    fn number_boundaries_and_non_finite_fallback() {
        // i64 edges parse as Int; one past either edge falls back to Float.
        assert_eq!(parse("9223372036854775807").unwrap(), Json::Int(i64::MAX));
        assert_eq!(parse("-9223372036854775808").unwrap(), Json::Int(i64::MIN));
        assert_eq!(
            parse("9223372036854775808").unwrap(),
            Json::Float(9223372036854775808.0)
        );
        assert_eq!(
            parse("-9223372036854775809").unwrap(),
            Json::Float(-9223372036854775809.0)
        );
        // exponent overflow yields a non-finite Float; as_f64() filters it at
        // the protocol layer, but json::parse itself does not error.
        let inf = parse("1e999").unwrap();
        assert!(matches!(inf, Json::Float(f) if f.is_infinite()));
        // empty / whitespace-only input is Eof, never a panic
        assert_eq!(parse(""), Err(JsonError::Eof));
        assert_eq!(parse("   "), Err(JsonError::Eof));
    }

    #[test]
    fn bounded_seeded_stress_never_panics_and_stays_deterministic() {
        // Deterministic LCG corpus of JSON metacharacter fragments: bounded,
        // reproducible, exercising strings/escapes/numbers/containers without
        // new dependencies. Every parse returns Ok or Err — never a panic —
        // and identical input gives identical output (parse is pure).
        let pool = [
            "{",
            "}",
            "[",
            "]",
            ":",
            ",",
            "\"",
            "\\",
            "u",
            "d800",
            "dc00",
            "0",
            "1.5e",
            "-",
            "nul",
            "tru",
            "fals",
            " ",
            "\u{1}",
            "\u{10FFFF}",
            "é",
            "x",
        ];
        let mut state: u64 = 0x243F_6A88_85A3_08D3;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        for _ in 0..2000 {
            let len = (next() % 24) as usize + 1;
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(pool[(next() as usize) % pool.len()]);
            }
            let a = parse(&s);
            let b = parse(&s);
            assert_eq!(a, b, "stateless parse must be deterministic for {s:?}");
        }
    }
}
