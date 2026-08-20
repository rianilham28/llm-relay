//! `GET /egress`: which source address upstream actually sees us from.
//!
//! Both probes go out over the **same pooled client** the relay itself uses, so
//! the answer describes the real egress path rather than a second one built for
//! the occasion. ipify splits the two families across hostnames —
//! `api.ipify.org` resolves A only, `api6.ipify.org` AAAA only — which is what
//! lets one request report both independently.

use std::net::IpAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{HeaderValue, USER_AGENT};
use hyper::{Method, Request, Response, StatusCode, Uri};

use crate::{BoxError, ResBody, State, convert, json_response};

pub const V4_PROBE: &str = "https://api.ipify.org";
pub const V6_PROBE: &str = "https://api6.ipify.org";
/// A probe is a diagnostic, so it fails fast rather than holding the endpoint:
/// a host with no IPv6 route must not make `/egress` feel hung.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// An address is at most 45 characters; anything larger is not an answer.
const MAX_PROBE_BODY: usize = 128;

pub async fn egress(state: &State) -> Response<ResBody> {
    // Concurrent: the two families are independent, and a dead one must not add
    // its timeout to the other's latency.
    let (v4, v6) = tokio::join!(
        probe(state, &state.cfg.egress_v4, true),
        probe(state, &state.cfg.egress_v6, false)
    );

    let mut out = String::with_capacity(320);
    out.push('{');
    write_member(&mut out, "ipv4", &v4);
    out.push(',');
    write_member(&mut out, "ipv6", &v6);
    out.push_str(",\"upstream_host\":");
    convert::write_json_string(&mut out, state.cfg.upstream.host().unwrap_or(""));
    out.push('}');

    // A host with no IPv6 route is ordinary, so one family answering is a
    // success. Neither answering means egress itself is broken.
    let status = if v4.is_ok() || v6.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::BAD_GATEWAY
    };
    json_response(status, Bytes::from(out))
}

/// `"<name>": <ip|null>, "<name>_error": <cause|null>` — the cause is upstream
/// text, so it goes through the escaper rather than straight between quotes.
fn write_member(out: &mut String, name: &str, result: &Result<IpAddr, String>) {
    let field = |out: &mut String, suffix: &str| {
        out.push('"');
        out.push_str(name);
        out.push_str(suffix);
        out.push_str("\":");
    };
    field(out, "");
    match result {
        Ok(ip) => convert::write_json_string(out, &ip.to_string()),
        Err(_) => out.push_str("null"),
    }
    out.push(',');
    field(out, "_error");
    match result {
        Ok(_) => out.push_str("null"),
        Err(cause) => convert::write_json_string(out, cause),
    }
}

async fn probe(state: &State, uri: &Uri, want_v4: bool) -> Result<IpAddr, String> {
    let family = if want_v4 { "IPv4" } else { "IPv6" };
    match tokio::time::timeout(PROBE_TIMEOUT, fetch_ip(state, uri)).await {
        Err(_) => Err(format!("{family} probe timed out after {}s", PROBE_TIMEOUT.as_secs())),
        Ok(Err(e)) => Err(e.to_string()),
        // Belt and braces: reporting an address as a family it is not would be
        // worse than reporting nothing.
        Ok(Ok(ip)) if ip.is_ipv4() != want_v4 => {
            Err(format!("{uri} answered {ip}, which is not {family}"))
        }
        Ok(Ok(ip)) => Ok(ip),
    }
}

async fn fetch_ip(state: &State, uri: &Uri) -> Result<IpAddr, BoxError> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri.clone())
        .header(USER_AGENT, HeaderValue::from_static("llm-relay/egress"))
        .body(Full::new(Bytes::new()))?;

    let resp = state.client.request(req).await?;
    let status = resp.status();
    let body = Limited::new(resp.into_body(), MAX_PROBE_BODY)
        .collect()
        .await?
        .to_bytes();
    if !status.is_success() {
        return Err(format!("probe answered {status}").into());
    }
    // Parsed, never echoed: the probe is a third party, so its body reaches the
    // caller only as an address this process validated.
    Ok(std::str::from_utf8(&body)?.trim().parse::<IpAddr>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The envelope is composed by hand around a failure string that came from
    /// the network, so it has to survive a hostile one.
    #[test]
    fn a_probe_failure_cannot_break_out_of_the_envelope() {
        let mut out = String::from("{");
        write_member(&mut out, "ipv4", &Ok("203.0.113.9".parse().unwrap()));
        out.push(',');
        write_member(
            &mut out,
            "ipv6",
            &Err(r#"","injected":{"x":"#.to_string()),
        );
        out.push('}');

        let v: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["ipv4"], "203.0.113.9");
        assert!(v["ipv4_error"].is_null());
        assert!(v["ipv6"].is_null());
        assert_eq!(v["ipv6_error"], r#"","injected":{"x":"#);
        assert!(v.get("injected").is_none(), "injected member appeared");
    }
}
