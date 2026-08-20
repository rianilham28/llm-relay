//! The OpenAI error envelope every terminal failure answers with:
//! `error.message`, `error.type`, `error.param`, `error.code`; the `type`
//! always follows the status map strict clients switch on.

use crate::convert;

/// The OpenAI error `type` a strict client expects for a given status code.
pub fn error_type(status: u16) -> &'static str {
    match status {
        400 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        500..=599 => "server_error",
        _ => "api_error",
    }
}

/// Truncate to at most `limit` characters, marking the cut so a long upstream
/// error page cannot balloon the relayed body.
pub fn truncate(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

/// One OpenAI error envelope body, type derived from the status.
pub fn openai_error_body(status: u16, message: &str) -> Vec<u8> {
    let mut out = String::with_capacity(message.len() + 96);
    out.push_str(r#"{"error":{"message":"#);
    convert::write_json_string(&mut out, message);
    // `error_type` returns one of a fixed set of bare identifiers, so it needs
    // no escaping; the caller's message is the only untrusted half.
    out.push_str(r#","type":""#);
    out.push_str(error_type(status));
    out.push_str(r#"","param":null,"code":null}}"#);
    out.into_bytes()
}

/// Upstream bodies that already speak the OpenAI error shape pass through
/// untouched; anything else is wrapped with the status-derived type and the
/// raw body text as the message.
pub fn normalize_error_body(payload: &[u8], status: u16) -> Vec<u8> {
    if convert::has_nested_string(payload, b"error", b"message") {
        return payload.to_vec();
    }
    let message = truncate(&String::from_utf8_lossy(payload), 500);
    openai_error_body(status, &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn wraps_non_openai_error_bodies_in_the_error_shape() {
        let wrapped = normalize_error_body(b"the upstream exploded", 502);
        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        assert_eq!(v["error"]["type"], "server_error");
        assert_eq!(v["error"]["message"], "the upstream exploded");
    }

    #[test]
    fn openai_shaped_errors_pass_through_untouched() {
        let openai = br#"{"error":{"message":"bad","type":"invalid_request_error","param":"x","code":null,"extra":1}}"#;
        let out = normalize_error_body(openai, 400);
        assert_eq!(out, openai);
    }

    #[test]
    fn an_oversized_body_maps_to_invalid_request_error() {
        // 413 is not in OpenAI's own status map, but it is a request problem,
        // so the fallthrough `api_error` would mislead a strict client.
        let wrapped = normalize_error_body(b"too big", 413);
        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn rate_limit_status_maps_to_rate_limit_error() {
        let wrapped = normalize_error_body(b"slow down", 429);
        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        assert_eq!(v["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn a_hostile_upstream_body_cannot_break_out_of_the_envelope() {
        // The upstream body is untrusted text spliced into an envelope this
        // crate composes by hand. A body full of quotes and braces must come
        // back as one JSON string, not as injected structure.
        let hostile = br#"","type":"injected","x":{"y":"#;
        let wrapped = normalize_error_body(hostile, 502);
        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        assert_eq!(v["error"]["type"], "server_error");
        assert_eq!(v["error"]["message"], r#"","type":"injected","x":{"y":"#);
        assert!(v["error"].get("x").is_none(), "injected member appeared");
    }

    #[test]
    fn control_bytes_in_an_error_body_stay_inside_the_string() {
        let wrapped = normalize_error_body(b"line1\nline2\ttab\x01ctl", 500);
        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        assert_eq!(v["error"]["message"], "line1\nline2\ttab\u{1}ctl");
    }

    #[test]
    fn long_error_bodies_are_truncated() {
        let long = "x".repeat(2000);
        let wrapped = normalize_error_body(long.as_bytes(), 502);
        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.ends_with('…'));
        assert!(msg.chars().count() <= 501);
    }
}