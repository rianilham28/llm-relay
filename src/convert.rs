//! Reading single values off a wire payload without parsing it, and writing
//! JSON strings back out. Nothing here rewrites relayed content: the upstream's
//! bytes reach the client untouched, and the relay only peeks at a member or
//! two along the way, or composes a small envelope of its own.

/// Nesting deeper than this is suspicious enough that the byte scanner gives
/// up rather than recurse further.
const MAX_DEPTH: usize = 64;

/// The value of a **top-level** string member, borrowed from the payload.
///
/// Scoped to the top level on purpose: a plain substring search for `"id"`
/// would happily match one nested in `choices[].delta.tool_calls[]` and hand
/// back a tool-call id as if it were the completion's. Returns None when the
/// payload is not an object, the key is absent, or its value is not a plain
/// unescaped string.
/// Locate a member of the JSON object at the start of `raw`. Returns the span
/// of its value and whether that value is a string literal.
fn find_member(raw: &[u8], key: &[u8]) -> Option<(usize, usize, bool)> {
    let mut s = Scanner::new(raw);
    s.skip_ws();
    if s.peek() != Some(b'{') {
        return None;
    }
    s.i += 1;
    loop {
        s.skip_ws();
        match s.peek() {
            None | Some(b'}') => return None,
            Some(b',') => {
                s.i += 1;
                continue;
            }
            Some(_) => {}
        }
        let found = s.read_string()?;
        let matches = &found[1..found.len() - 1] == key;
        s.skip_ws();
        if s.peek() != Some(b':') {
            return None;
        }
        s.i += 1;
        s.skip_ws();
        let is_string = s.peek() == Some(b'"');
        let start = s.i;
        if !s.skip_value() {
            return None;
        }
        if matches {
            return Some((start, s.i, is_string));
        }
    }
}

/// The value of a **top-level** string member, borrowed from the payload.
///
/// Scoped to the top level on purpose: a plain substring search for `"id"`
/// would happily match one nested in `choices[].delta.tool_calls[]` and hand
/// back a tool-call id as if it were the completion's. Returns None when the
/// payload is not an object, the key is absent, or its value is not a plain
/// unescaped string.
pub fn top_level_str<'a>(raw: &'a [u8], key: &[u8]) -> Option<&'a str> {
    let (start, end, is_string) = find_member(raw, key)?;
    if !is_string {
        return None;
    }
    let text = &raw[start + 1..end - 1];
    // An escaped value would need unescaping to be usable as-is.
    (!text.contains(&b'\\'))
        .then(|| std::str::from_utf8(text).ok())
        .flatten()
}

/// Whether `raw` holds an `outer_key` object that itself holds a string
/// `inner_key` — the shape that marks a body as already speaking the OpenAI
/// error envelope (`error.message`).
pub fn has_nested_string(raw: &[u8], outer_key: &[u8], inner_key: &[u8]) -> bool {
    let Some((start, end, _)) = find_member(raw, outer_key) else {
        return false;
    };
    matches!(find_member(&raw[start..end], inner_key), Some((_, _, true)))
}

/// Append `text` to `out` as a quoted JSON string.
///
/// Escapes what RFC 8259 requires — the quote, the backslash, and every
/// control character below 0x20. The relay composes envelopes around arbitrary
/// upstream error text, so this cannot assume the input is safe to drop
/// between quotes unexamined.
pub fn write_json_string(out: &mut String, text: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let n = c as u32;
                out.push_str("\\u00");
                out.push(HEX[((n >> 4) & 0xF) as usize] as char);
                out.push(HEX[(n & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A cursor over the payload bytes. Positions are absolute offsets into
/// `input`, so a span can be borrowed back out verbatim.
struct Scanner<'a> {
    input: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Scanner<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            i: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    /// Read a string literal at the cursor, returning its span including the
    /// quotes. `None` on an unterminated string.
    fn read_string(&mut self) -> Option<&'a [u8]> {
        if self.peek() != Some(b'"') {
            return None;
        }
        let start = self.i;
        self.i += 1;
        loop {
            match self.peek() {
                None => return None,
                Some(b'"') => {
                    self.i += 1;
                    return Some(&self.input[start..self.i]);
                }
                Some(b'\\') => {
                    self.i += 1;
                    self.peek()?;
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
    }

    /// Advance past one JSON value. `false` on malformed input or runaway
    /// nesting.
    fn skip_value(&mut self) -> bool {
        self.skip_ws();
        match self.peek() {
            None => false,
            Some(b'"') => self.read_string().is_some(),
            Some(b'{') => self.skip_compound(b'}'),
            Some(b'[') => self.skip_compound(b']'),
            Some(b't') => self.skip_literal(b"true"),
            Some(b'f') => self.skip_literal(b"false"),
            Some(b'n') => self.skip_literal(b"null"),
            Some(c) if c.is_ascii_digit() || c == b'-' => self.skip_number(),
            Some(_) => false,
        }
    }

    fn skip_literal(&mut self, literal: &[u8]) -> bool {
        if self.input[self.i..].starts_with(literal) {
            self.i += literal.len();
            true
        } else {
            false
        }
    }

    fn skip_number(&mut self) -> bool {
        let start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            self.i += 1;
        }
        self.i > start
    }

    /// Advance past a `{...}` / `[...]` compound whose opening bracket is at
    /// the cursor. Strings inside are escaped correctly; anything else is
    /// consumed token by token until the matching close.
    fn skip_compound(&mut self, close: u8) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return false;
        }
        self.i += 1;
        loop {
            self.skip_ws();
            match self.peek() {
                None => return false,
                Some(b'"') => {
                    if self.read_string().is_none() {
                        return false;
                    }
                }
                Some(b'{') => {
                    if !self.skip_compound(b'}') {
                        return false;
                    }
                }
                Some(b'[') => {
                    if !self.skip_compound(b']') {
                        return false;
                    }
                }
                Some(c) if c == close => {
                    self.i += 1;
                    self.depth -= 1;
                    return true;
                }
                Some(_) => self.i += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escape_what_would_break_out_of_the_quotes() {
        // Upstream error text lands inside an envelope this crate composes, so
        // a quote or a control byte in it must not be able to end the string
        // early and inject structure.
        let mut out = String::new();
        write_json_string(&mut out, "he said \"hi\"\\ and\nstopped\u{1}");
        assert_eq!(out, r#""he said \"hi\"\\ and\nstopped\u0001""#);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, "he said \"hi\"\\ and\nstopped\u{1}");
    }

    #[test]
    fn nested_string_detects_the_openai_error_shape() {
        assert!(has_nested_string(
            br#"{"error":{"message":"bad","code":null}}"#,
            b"error",
            b"message"
        ));
        // Present but not a string, absent, and not nested under `error`.
        assert!(!has_nested_string(br#"{"error":{"message":7}}"#, b"error", b"message"));
        assert!(!has_nested_string(br#"{"error":{"code":1}}"#, b"error", b"message"));
        assert!(!has_nested_string(br#"{"message":"bad"}"#, b"error", b"message"));
        assert!(!has_nested_string(b"not json", b"error", b"message"));
    }

    #[test]
    fn top_level_str_reads_only_the_top_level() {
        let frame = br#"{"id":"cmpl-1","choices":[{"delta":{"tool_calls":[{"id":"call-9"}]}}]}"#;
        assert_eq!(top_level_str(frame, b"id"), Some("cmpl-1"));

        // The nested id must not be mistaken for the completion's when the
        // top level has none.
        let no_id = br#"{"choices":[{"delta":{"tool_calls":[{"id":"call-9"}]}}]}"#;
        assert_eq!(top_level_str(no_id, b"id"), None);
    }

    #[test]
    fn top_level_str_reads_a_real_upstream_frame() {
        // A chunk as the upstream actually sends it, keys in its own order.
        let frame = br#"{"id":"router-18dc9bf3","object":"chat.completion.chunk","created":1787106976,"model":"deepseek-v4-flash-free","choices":[{"index":0,"finish_reason":null,"logprobs":null,"delta":{"role":"assistant","content":"","reasoning_content":null}}]}"#;
        assert_eq!(top_level_str(frame, b"id"), Some("router-18dc9bf3"));
        assert_eq!(top_level_str(frame, b"model"), Some("deepseek-v4-flash-free"));
    }

    #[test]
    fn top_level_str_declines_what_it_cannot_return_verbatim() {
        // Not a string.
        assert_eq!(top_level_str(br#"{"id":7}"#, b"id"), None);
        // Escaped: returning the raw span would hand back the escape sequence.
        assert_eq!(top_level_str(br#"{"id":"a\/b"}"#, b"id"), None);
        // Not an object, and truncated input.
        assert_eq!(top_level_str(b"[1]", b"id"), None);
        assert_eq!(top_level_str(br#"{"id":"unterminated"#, b"id"), None);
    }

    #[test]
    fn top_level_str_gives_up_past_max_depth() {
        // The recursion guard: a payload nested past `MAX_DEPTH` is declined
        // rather than walked, so an adversarial body cannot drive the stack.
        let depth = MAX_DEPTH + 8;
        let mut raw = String::from(r#"{"deep":"#);
        raw.push_str(&"[".repeat(depth));
        raw.push('1');
        raw.push_str(&"]".repeat(depth));
        raw.push_str(r#","id":"x"}"#);
        assert_eq!(top_level_str(raw.as_bytes(), b"id"), None);
    }
}
