//! Byte-offset preserving JSON parser used by the writer to compute JRIF ranges.
//!
//! The parser conforms to RFC 8259 with one practical restriction: input must be
//! valid UTF-8 (JRIF payloads are JSON, which RFC 8259 mandates be encoded in
//! UTF-8 for interchange).

use std::fmt;

#[derive(Debug)]
pub struct ParseError {
    pub pos: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "json parse error at byte {}: {}", self.pos, self.message)
    }
}

impl std::error::Error for ParseError {}

/// True for the four ASCII bytes RFC 8259 §2 lists as JSON whitespace.
/// `u8::is_ascii_whitespace` accepts form-feed and so doesn't match JSON.
#[inline]
pub const fn is_json_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

#[derive(Debug)]
pub enum Value {
    Null {
        start: usize,
        end: usize,
    },
    Bool {
        start: usize,
        end: usize,
        value: bool,
    },
    Number {
        start: usize,
        end: usize,
    },
    String {
        start: usize,
        end: usize,
    },
    Array {
        start: usize,
        end: usize,
        items: Vec<Self>,
    },
    Object {
        start: usize,
        end: usize,
        members: Vec<Member>,
    },
}

#[derive(Debug)]
pub struct Member {
    pub name: String,
    pub name_start: usize,
    pub value: Value,
}

impl Value {
    pub const fn start(&self) -> usize {
        match self {
            Self::Null { start, .. }
            | Self::Bool { start, .. }
            | Self::Number { start, .. }
            | Self::String { start, .. }
            | Self::Array { start, .. }
            | Self::Object { start, .. } => *start,
        }
    }

    pub const fn end(&self) -> usize {
        match self {
            Self::Null { end, .. }
            | Self::Bool { end, .. }
            | Self::Number { end, .. }
            | Self::String { end, .. }
            | Self::Array { end, .. }
            | Self::Object { end, .. } => *end,
        }
    }

    pub const fn is_compound(&self) -> bool {
        matches!(self, Self::Array { .. } | Self::Object { .. })
    }
}

pub fn parse(bytes: &[u8]) -> Result<Value, ParseError> {
    let mut p = Parser { bytes, pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(p.err("trailing data after top-level value"));
    }
    Ok(v)
}

/// Parse `bytes` as JSONL: zero or more JSON values separated by whitespace
/// (typically line feeds). Returns the records in document order. Leading
/// and trailing whitespace are permitted; blank lines between records are
/// permitted.
pub fn parse_jsonl(bytes: &[u8]) -> Result<Vec<Value>, ParseError> {
    let mut p = Parser { bytes, pos: 0 };
    let mut records = Vec::new();
    loop {
        p.skip_ws();
        if p.pos == bytes.len() {
            return Ok(records);
        }
        records.push(p.parse_value()?);
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    #[cold]
    #[inline(never)]
    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError {
            pos: self.pos,
            message: msg.into(),
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    #[inline]
    fn expect(&mut self, b: u8) -> Result<(), ParseError> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(format!("expected '{}'", b as char)))
        }
    }

    #[inline]
    fn skip_ws(&mut self) {
        let bytes = self.bytes;
        let mut pos = self.pos;
        while pos < bytes.len() && is_json_ws(bytes[pos]) {
            pos += 1;
        }
        self.pos = pos;
    }

    #[inline]
    fn parse_value(&mut self) -> Result<Value, ParseError> {
        self.skip_ws();
        let start = self.pos;
        let b = self
            .peek()
            .ok_or_else(|| self.err("unexpected end of input"))?;
        match b {
            b'{' => self.parse_object(start),
            b'[' => self.parse_array(start),
            b'"' => {
                let end = self.walk_string(None)?;
                Ok(Value::String { start, end })
            }
            b't' | b'f' => self.parse_bool(start),
            b'n' => self.parse_null(start),
            b'-' | b'0'..=b'9' => self.parse_number(start),
            _ => Err(self.err(format!("unexpected byte '{}'", b as char))),
        }
    }

    fn parse_null(&mut self, start: usize) -> Result<Value, ParseError> {
        self.expect_literal(b"null")?;
        Ok(Value::Null {
            start,
            end: start + 3,
        })
    }

    fn parse_bool(&mut self, start: usize) -> Result<Value, ParseError> {
        if self.peek() == Some(b't') {
            self.expect_literal(b"true")?;
            Ok(Value::Bool {
                start,
                end: start + 3,
                value: true,
            })
        } else {
            self.expect_literal(b"false")?;
            Ok(Value::Bool {
                start,
                end: start + 4,
                value: false,
            })
        }
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Result<(), ParseError> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(self.err(format!(
                "expected literal '{}'",
                std::str::from_utf8(literal).expect("literal is ASCII")
            )))
        }
    }

    fn parse_number(&mut self, start: usize) -> Result<Value, ParseError> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number: missing digits")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number: missing fraction digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number: missing exponent digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        Ok(Value::Number {
            start,
            end: self.pos - 1,
        })
    }

    fn parse_array(&mut self, start: usize) -> Result<Value, ParseError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array {
                start,
                end: self.pos - 1,
                items,
            });
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array {
                        start,
                        end: self.pos - 1,
                        items,
                    });
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn parse_object(&mut self, start: usize) -> Result<Value, ParseError> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object {
                start,
                end: self.pos - 1,
                members,
            });
        }
        loop {
            self.skip_ws();
            let name_start = self.pos;
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object member name"));
            }
            let (name, _) = self.parse_string_value()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            members.push(Member {
                name,
                name_start,
                value,
            });
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object {
                        start,
                        end: self.pos - 1,
                        members,
                    });
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn parse_string_value(&mut self) -> Result<(String, usize), ParseError> {
        let mut out = String::new();
        let end = self.walk_string(Some(&mut out))?;
        Ok((out, end))
    }

    /// Walk past a JSON string, validating UTF-8 and escapes. When `out` is
    /// `Some`, the decoded contents are appended; when `None`, decoding is
    /// skipped (validation still runs). Returns the byte offset of the closing
    /// quote.
    fn walk_string(&mut self, mut out: Option<&mut String>) -> Result<usize, ParseError> {
        self.expect(b'"')?;
        let mut chunk_start = self.pos;

        loop {
            let bytes = self.bytes;
            let i = if let Some(off) = memchr::memchr2(b'"', b'\\', &bytes[self.pos..]) {
                self.pos + off
            } else {
                self.pos = bytes.len();
                return Err(self.err("unterminated string"));
            };
            self.pos = i;

            let chunk = &bytes[chunk_start..i];
            // Unescaped control bytes (< 0x20) are illegal in JSON strings;
            // memchr can't search for a range, so do a separate pass — the
            // optimizer autovectorizes this for ASCII-clean input.
            if let Some(ctrl_off) = chunk.iter().position(|&b| b < 0x20) {
                self.pos = chunk_start + ctrl_off;
                return Err(self.err("unescaped control character in string"));
            }
            let chunk_str = std::str::from_utf8(chunk).map_err(|e| ParseError {
                pos: chunk_start + e.valid_up_to(),
                message: "invalid UTF-8 sequence".to_owned(),
            })?;
            if let Some(acc) = out.as_deref_mut() {
                acc.push_str(chunk_str);
            }

            if bytes[i] == b'"' {
                let end = self.pos;
                self.pos += 1;
                return Ok(end);
            }
            self.pos += 1; // consume the backslash
            let esc = self
                .bump()
                .ok_or_else(|| self.err("unterminated escape sequence"))?;
            match esc {
                b'"' => push_char(out.as_deref_mut(), '"'),
                b'\\' => push_char(out.as_deref_mut(), '\\'),
                b'/' => push_char(out.as_deref_mut(), '/'),
                b'b' => push_char(out.as_deref_mut(), '\u{08}'),
                b'f' => push_char(out.as_deref_mut(), '\u{0C}'),
                b'n' => push_char(out.as_deref_mut(), '\n'),
                b'r' => push_char(out.as_deref_mut(), '\r'),
                b't' => push_char(out.as_deref_mut(), '\t'),
                b'u' => {
                    let cp = self.read_hex4()?;
                    if (0xD800..=0xDBFF).contains(&cp) {
                        if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                            return Err(self.err("expected low surrogate"));
                        }
                        let lo = self.read_hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&lo) {
                            return Err(self.err("invalid low surrogate"));
                        }
                        let code = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        let ch = char::from_u32(code)
                            .ok_or_else(|| self.err("invalid surrogate pair"))?;
                        push_char(out.as_deref_mut(), ch);
                    } else if (0xDC00..=0xDFFF).contains(&cp) {
                        return Err(self.err("unexpected low surrogate"));
                    } else {
                        let ch =
                            char::from_u32(cp).ok_or_else(|| self.err("invalid code point"))?;
                        push_char(out.as_deref_mut(), ch);
                    }
                }
                _ => return Err(self.err(format!("invalid escape '\\{}'", esc as char))),
            }
            chunk_start = self.pos;
        }
    }

    #[inline]
    fn read_hex4(&mut self) -> Result<u32, ParseError> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let b = self
                .bump()
                .ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.err("invalid hex digit in \\u escape")),
            };
            v = v * 16 + u32::from(d);
        }
        Ok(v)
    }
}

#[inline]
fn push_char(out: Option<&mut String>, ch: char) {
    if let Some(acc) = out {
        acc.push(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primitives_with_ranges() {
        let bytes = b"  null ";
        let v = parse(bytes).unwrap();
        assert!(matches!(v, Value::Null { start: 2, end: 5 }));
    }

    #[test]
    fn parses_object_and_records_member_ranges() {
        let bytes = br#"{"a": 1, "b": "two"}"#;
        let v = parse(bytes).unwrap();
        let Value::Object {
            start,
            end,
            members,
        } = v
        else {
            panic!("expected object");
        };
        assert_eq!(start, 0);
        assert_eq!(end, bytes.len() - 1);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].name, "a");
        assert_eq!(members[0].name_start, 1);
        assert_eq!(members[0].value.start(), 6);
        assert_eq!(members[0].value.end(), 6);
        assert_eq!(members[1].name, "b");
        assert_eq!(members[1].value.start(), 14);
        assert_eq!(members[1].value.end(), 18);
    }

    #[test]
    fn rejects_trailing_data() {
        assert!(parse(b"null x").is_err());
    }

    #[test]
    fn handles_escapes_and_surrogates() {
        let bytes = "\"é 😀\"".as_bytes();
        let v = parse(bytes).unwrap();
        assert!(matches!(v, Value::String { start: 0, .. }));
    }

    #[test]
    fn handles_unicode_escape_with_surrogate_pair() {
        // 😀 == U+1F600 GRINNING FACE.
        let bytes = b"\"\\uD83D\\uDE00\"";
        let v = parse(bytes).unwrap();
        assert!(matches!(v, Value::String { start: 0, .. }));
    }
}
