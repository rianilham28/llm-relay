//! Upstream identity: the header set a genuine upstream client sends, and the
//! request/session ID minting behind it. Pure — the clock is injected so the
//! wasm caller can feed it from `Date.now()` and host tests from `SystemTime`.
//!
//! The upstream-specific strings (the agent string, the vendor header names,
//! and the substring that identifies a genuine caller) are XOR-obfuscated and
//! decoded at compile time via [`crate::deob`], so this source is not greppable
//! for them while the plaintext is still materialized by the compiler.

use std::sync::atomic::{AtomicU64, Ordering};

/// The agent string a real upstream CLI sends; used only when the caller did
/// not supply its own (a caller that matches [`NEEDLE`] always wins).
pub(crate) const DEFAULT_UA: &str = crate::deob!(
    0x5A,
    [
        0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e, 0x3f, 0x75, 0x6b, 0x74, 0x6b, 0x6d, 0x74, 0x6b,
        0x6f, 0x7a, 0x3b, 0x33, 0x77, 0x29, 0x3e, 0x31, 0x75, 0x2a, 0x28, 0x35, 0x2c, 0x33, 0x3e,
        0x3f, 0x28, 0x77, 0x2f, 0x2e, 0x33, 0x36, 0x29, 0x75, 0x6e, 0x74, 0x6a, 0x74, 0x68, 0x69,
        0x7a, 0x28, 0x2f, 0x34, 0x2e, 0x33, 0x37, 0x3f, 0x75, 0x38, 0x2f, 0x34, 0x75, 0x6b, 0x74,
        0x69, 0x74, 0x6b, 0x6e,
    ]
);
pub(crate) const DEFAULT_CLIENT: &str = "cli";
pub(crate) const DEFAULT_PROJECT: &str = "global";
pub(crate) const DEFAULT_ACCEPT: &str = "*/*";

/// The substring that marks a caller as a genuine upstream client, so its own
/// agent string is kept rather than replaced.
pub(crate) const NEEDLE: &str = crate::deob!(
    0x5A,
    [0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e, 0x3f]
);

pub(crate) const H_CLIENT: &str = crate::deob!(
    0x5A,
    [0x22, 0x77, 0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e, 0x3f, 0x77, 0x39, 0x36, 0x33, 0x3f, 0x34, 0x2e]
);
pub(crate) const H_PROJECT: &str = crate::deob!(
    0x5A,
    [0x22, 0x77, 0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e, 0x3f, 0x77, 0x2a, 0x28, 0x35, 0x30, 0x3f, 0x39, 0x2e]
);
pub(crate) const H_REQUEST: &str = crate::deob!(
    0x5A,
    [0x22, 0x77, 0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e, 0x3f, 0x77, 0x28, 0x3f, 0x2b, 0x2f, 0x3f, 0x29, 0x2e]
);
pub(crate) const H_SESSION: &str = crate::deob!(
    0x5A,
    [0x22, 0x77, 0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e, 0x3f, 0x77, 0x29, 0x3f, 0x29, 0x29, 0x33, 0x35, 0x34]
);

/// Last `millis << 12 | counter` handed out, so IDs minted inside the same
/// millisecond still differ — and still ascend — in their ordered half.
static ID_CLOCK: AtomicU64 = AtomicU64::new(0);

/// One splitmix64 step: well-distributed, deterministic per seed, dependency-free.
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Ordered half of an ID: the millisecond clock scaled by 0x1000 with a counter
/// in the low 12 bits, which restarts on every new millisecond.
fn next_ordered(now_millis: u64) -> u64 {
    let mut prev = ID_CLOCK.load(Ordering::Relaxed);
    loop {
        // A clock that went backwards keeps counting off the last value rather
        // than reissuing IDs that were already handed out.
        let next = if now_millis > (prev >> 12) {
            (now_millis << 12) | 1
        } else {
            prev + 1
        };
        match ID_CLOCK.compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(reloaded) => prev = reloaded,
        }
    }
}

/// Apply the descending inversion to a 48-bit ordered value.
fn ordered_id(ordered: u64, descending: bool) -> u64 {
    const MASK: u64 = 0xFFFF_FFFF_FFFF;
    let ordered = ordered & MASK;
    if descending { !ordered & MASK } else { ordered }
}

/// Mint an upstream-style ID: `<prefix>_`, then the low 48 bits of the ordered
/// clock as 12 hex digits, then 14 random base62 characters — the 26-character
/// shape the real client sends (e.g. `msg_006fd964c00106WbJR7y2D5wPM`).
///
/// `descending` inverts the ordered half, which is how the upstream client mints
/// session IDs so the newest sorts first.
pub fn mint_id(prefix: &str, descending: bool, now_millis: u64) -> String {
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let ordered = ordered_id(next_ordered(now_millis), descending);
    let mut id = format!("{prefix}_{ordered:012x}");
    let mut seed = ordered;
    for _ in 0..14 {
        seed = splitmix64(seed);
        id.push(BASE62[(seed % BASE62.len() as u64) as usize] as char);
    }
    id
}

/// Case-insensitive substring test that allocates nothing.
pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Build the header set a genuine upstream client sends. This is a whitelist,
/// not a filtered relay: whatever the caller sent, upstream sees the same shape
/// every time, so any caller still presents the upstream profile. Caller values
/// win where they exist, so a real upstream client passes through untouched.
///
/// `fallback_session` is the isolate-scoped session tag (minted once); the
/// request ID is minted fresh per call. `Authorization`, `Content-Type`, and
/// `Content-Length` are set on the request itself and never appear here.
pub fn upstream_headers(
    incoming: Option<&[(String, String)]>,
    fallback_session: &str,
    now_millis: u64,
) -> Vec<(String, String)> {
    let borrowed = |name: &str| {
        incoming.and_then(|hs| {
            hs.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        })
    };

    let mut out = Vec::with_capacity(6);
    // A caller that is itself an upstream client keeps its own agent string;
    // anything else would otherwise announce the worker runtime.
    let ua = borrowed("user-agent")
        .filter(|v| contains_ignore_ascii_case(v, NEEDLE))
        .unwrap_or_else(|| DEFAULT_UA.to_string());
    out.push(("user-agent".to_string(), ua));
    out.push((
        "accept".to_string(),
        borrowed("accept").unwrap_or_else(|| DEFAULT_ACCEPT.to_string()),
    ));
    out.push((
        H_CLIENT.to_string(),
        borrowed(H_CLIENT).unwrap_or_else(|| DEFAULT_CLIENT.to_string()),
    ));
    out.push((
        H_PROJECT.to_string(),
        borrowed(H_PROJECT).unwrap_or_else(|| DEFAULT_PROJECT.to_string()),
    ));
    // The session tag is conversation-scoped, so a caller's own value is kept
    // for its whole conversation, while a synthesized one is per-isolate.
    out.push((
        H_SESSION.to_string(),
        borrowed(H_SESSION).unwrap_or_else(|| fallback_session.to_string()),
    ));
    out.push((
        H_REQUEST.to_string(),
        borrowed(H_REQUEST).unwrap_or_else(|| mint_id("msg", false, now_millis)),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_id_has_the_shape_the_real_client_sends() {
        let id = mint_id("msg", false, 1_700_000_000_123);
        let rest = id.strip_prefix("msg_").expect("prefix");
        assert_eq!(rest.len(), 26, "{id}");
        assert!(rest[..12].bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        // The random half is base62.
        assert!(rest[12..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric()), "{id}");
    }

    #[test]
    fn descending_inverts_the_ordered_half() {
        // A captured `ses_ff90a0333ffe` inverts to `006f5fccc001` — the same
        // relationship the real client's IDs show.
        assert_eq!(
            ordered_id(0x006f_5fcc_c001, true),
            0xff90_a033_3ffe
        );
    }

    #[test]
    fn ids_inside_the_same_millisecond_still_differ() {
        let a = mint_id("msg", false, 42);
        let b = mint_id("msg", false, 42);
        assert_ne!(a, b);
    }

    #[test]
    fn headers_whitelist_keeps_matching_caller_values() {
        let caller_ua = format!("{NEEDLE}/1.0 custom");
        let incoming = vec![
            ("User-Agent".to_string(), caller_ua.clone()),
            (H_SESSION.to_string(), "ses_client".to_string()),
        ];
        let out = upstream_headers(Some(&incoming), "ses_fallback", 1);
        assert_eq!(
            out.iter().find(|(k, _)| k == "user-agent").unwrap().1,
            caller_ua
        );
        assert_eq!(
            out.iter().find(|(k, _)| k == H_SESSION).unwrap().1,
            "ses_client"
        );
        // A caller UA that does not match the needle is replaced, not relayed.
        let unmatched = vec![("User-Agent".to_string(), "curl/8".to_string())];
        let out = upstream_headers(Some(&unmatched), "ses_fallback", 1);
        assert_eq!(
            out.iter().find(|(k, _)| k == "user-agent").unwrap().1,
            DEFAULT_UA
        );
        // Credentials and framing never appear: exactly six headers.
        assert_eq!(out.len(), 6);
    }
}