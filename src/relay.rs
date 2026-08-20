//! Slim relay to the upstream chat-completions endpoint. No egress lanes, no admission
//! control, no retry pool: one pooled fetch, a single retry on retryable
//! outcomes, and the upstream's own bytes on the way back. The relay does not
//! rewrite wire content in either direction — a buffered answer is forwarded
//! without ever being collected, and an SSE stream is relayed frame by frame.

use std::sync::LazyLock;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt, TryStreamExt};
use http_body_util::{BodyExt, BodyStream, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER,
    USER_AGENT,
};
use hyper::{Method, Request, Response, StatusCode};

use crate::{BoxError, ResBody, State, convert, error, fail, identity, json_ct, json_response, now_millis};

const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";
const RETRY_DELAY_MS: u64 = 750;
const RETRY_AFTER_429_SECS: &str = "30";
/// An upstream error page is read only to be quoted into an envelope, and the
/// message is truncated to 500 chars anyway — so it is never worth buffering
/// more than this.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// The session tag reused for every request that arrives without one. Minted
/// once per process so synthesized traffic reads as one long-lived session.
static FALLBACK_SESSION: LazyLock<String> =
    LazyLock::new(|| identity::mint_id("ses", true, now_millis()));

/// The vendor header names, built once rather than per request. `HeaderName`
/// clones are cheap; parsing a non-standard name allocates. The names themselves
/// are compile-time-decoded constants (see `identity`), so they are not spelled
/// out here.
static CUSTOM_HEADER_NAMES: LazyLock<[HeaderName; 4]> = LazyLock::new(|| {
    [
        HeaderName::from_static(identity::H_CLIENT),
        HeaderName::from_static(identity::H_PROJECT),
        HeaderName::from_static(identity::H_SESSION),
        HeaderName::from_static(identity::H_REQUEST),
    ]
});

/// The outbound `HeaderName` for one whitelisted name, without parsing it. The
/// vendor names are runtime constants, so this is a comparison chain rather than
/// a literal `match`.
fn caller_header_name(name: &str) -> Option<HeaderName> {
    let custom = &*CUSTOM_HEADER_NAMES;
    if name == "user-agent" {
        Some(USER_AGENT)
    } else if name == "accept" {
        Some(ACCEPT)
    } else if name == identity::H_CLIENT {
        Some(custom[0].clone())
    } else if name == identity::H_PROJECT {
        Some(custom[1].clone())
    } else if name == identity::H_SESSION {
        Some(custom[2].clone())
    } else if name == identity::H_REQUEST {
        Some(custom[3].clone())
    } else {
        None
    }
}

/// Header names a caller may still own: the vendor identity surface only.
/// hyper has already lowercased every received name, so this compares directly.
fn is_caller_header(name: &str) -> bool {
    name == "user-agent"
        || name == "accept"
        || name == identity::H_CLIENT
        || name == identity::H_PROJECT
        || name == identity::H_SESSION
        || name == identity::H_REQUEST
}

pub async fn chat(req: Request<Incoming>, state: &State) -> Response<ResBody> {
    let (parts, body) = req.into_parts();

    let incoming: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter(|(k, _)| is_caller_header(k.as_str()))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();

    // Buffered, not streamed, because a retry has to be able to send it again.
    let body = match Limited::new(body, state.cfg.max_body).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return body_error(&e, state.cfg.max_body),
    };
    let model = client_model(&body);

    let headers = identity::upstream_headers(Some(&incoming), &FALLBACK_SESSION, now_millis());

    let mut last: Option<ErrorEnvelope> = None;
    let max_retries = state.cfg.max_retries;
    for attempt in 0..=max_retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS * attempt as u64)).await;
        }
        // Cloning `Bytes` bumps a refcount; the body is never copied per retry.
        let outbound = match build_request(state, &headers, body.clone()) {
            Ok(req) => req,
            Err(e) => return fail(500, &format!("could not build upstream request: {e}")),
        };
        match try_once(state, outbound).await {
            Ok(up) => return serve(up, &model),
            Err(e) if e.retryable() && attempt < max_retries => last = Some(e),
            Err(e) => return serve_error(e),
        }
    }
    // Retries exhausted on a retryable outcome: surface the last upstream
    // error; a transport-only failure answers the standard 502 envelope.
    match last {
        Some(e) => serve_error(e),
        None => fail(502, "upstream request failed: retries exhausted"),
    }
}

fn build_request(
    state: &State,
    headers: &[(String, String)],
    body: Bytes,
) -> Result<Request<Full<Bytes>>, BoxError> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(state.cfg.upstream.clone());
    let out = builder
        .headers_mut()
        .ok_or("upstream URI was rejected by the request builder")?;
    for (k, v) in headers {
        // The name set is closed and known at compile time, so it is matched
        // rather than parsed: `from_bytes` would allocate for each of the vendor
        // header names, which are not in http's static table. A value that will
        // not go in a header is skipped rather than failing the whole request.
        let (Some(name), Ok(value)) = (caller_header_name(k), HeaderValue::from_str(v)) else {
            continue;
        };
        out.insert(name, value);
    }
    out.insert(AUTHORIZATION, state.cfg.authorization.clone());
    out.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(builder.body(Full::new(body))?)
}

/// One relayed error outcome: either an upstream status + raw body, or a
/// transport failure with no upstream answer.
enum ErrorEnvelope {
    Status { status: u16, body: Bytes },
    Transport(String),
}

impl ErrorEnvelope {
    fn retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, .. } => matches!(*status, 408 | 429 | 500..=599),
        }
    }
}

/// The upstream answer, classified for the relay path: streaming bodies stay
/// streams; everything else is forwarded without being collected.
enum Upstream {
    Stream(Incoming),
    Body { status: u16, body: Incoming },
}

async fn try_once(state: &State, req: Request<Full<Bytes>>) -> Result<Upstream, ErrorEnvelope> {
    let resp = match state.client.request(req).await {
        Ok(r) => r,
        Err(e) => return Err(ErrorEnvelope::Transport(e.to_string())),
    };
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = Limited::new(resp.into_body(), MAX_ERROR_BODY)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        return Err(ErrorEnvelope::Status { status, body });
    }
    let streams = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("event-stream"));
    let body = resp.into_body();
    if streams {
        Ok(Upstream::Stream(body))
    } else {
        Ok(Upstream::Body { status, body })
    }
}

/// Relay the upstream answer in the form it arrived. The upstream honors the
/// request's `stream` flag, so that form is already the one the client asked
/// for and no shape conversion is needed in either direction.
fn serve(up: Upstream, model: &str) -> Response<ResBody> {
    match up {
        // Forwarded, not collected: the upstream body object becomes the
        // client's response body, so no byte is ever buffered here.
        Upstream::Body { status, body } => {
            let mut resp = Response::new(body.map_err(|e| Box::new(e) as BoxError).boxed());
            *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            resp.headers_mut().insert(CONTENT_TYPE, json_ct());
            resp
        }
        Upstream::Stream(body) => relay_sse(body, model.to_string()),
    }
}

/// Relay an upstream SSE stream frame by frame, until `[DONE]` or the stream
/// ends or fails. A broken stream gets an in-band truncation frame and never a
/// clean `[DONE]`; strict clients raise on the error object.
fn relay_sse(upstream: Incoming, model: String) -> Response<ResBody> {
    let frames = relay_frames(BodyStream::new(upstream), model).map_ok(Frame::data);
    // Disambiguated: `StreamBody` satisfies both `BodyExt` and `StreamExt`.
    let mut resp = Response::new(BodyExt::boxed(StreamBody::new(frames)));
    let headers = resp.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Reverse proxies that buffer by default would hold frames until the
    // stream ends, which is the one thing a token stream cannot survive.
    headers.insert(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"));
    resp
}

/// Relay state: buffered bytes, frame reassembly, and the in-band truncation
/// context (upstream chunk id + model).
struct RelayState<S> {
    inner: S,
    pending: BytesMut,
    /// How far into `pending` the frame-boundary search already looked, so a
    /// long frame is scanned once overall rather than once per arriving chunk.
    scanned: usize,
    done: bool,
    finished: bool,
    stream_id: Option<String>,
    model: String,
}

/// Fold a body stream into OpenAI chunk frames, one per step, until done.
fn relay_frames<S, E>(upstream: S, model: String) -> impl Stream<Item = Result<Bytes, BoxError>>
where
    S: Stream<Item = Result<Frame<Bytes>, E>> + Unpin,
    E: std::fmt::Display,
{
    let state = RelayState {
        inner: upstream,
        pending: BytesMut::new(),
        scanned: 0,
        done: false,
        finished: false,
        stream_id: None,
        model,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if st.finished {
                return None;
            }
            // A complete frame already buffered wins over reading more.
            if let Some(end) = find_frame_end(&st.pending, st.scanned) {
                // `split_to` hands the frame out without copying it.
                let frame = st.pending.split_to(end + 1).freeze();
                st.scanned = 0;
                return Some((frame_step(&mut st, frame), st));
            }
            // Only a boundary straddling the join needs re-examining.
            st.scanned = st.pending.len().saturating_sub(1);
            match st.inner.next().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        st.pending.extend_from_slice(&data);
                    }
                }
                Some(Err(e)) => {
                    st.finished = true;
                    return Some((Ok(truncation_frame(&st, &e.to_string())), st));
                }
                None => {
                    // Upstream ended: a partial tail is a truncation, a clean
                    // end without [DONE] still gets the terminator.
                    st.finished = true;
                    if !st.pending.is_empty() {
                        return Some((Ok(truncation_frame(&st, "closed mid-frame")), st));
                    }
                    if st.done {
                        return None;
                    }
                    return Some((Ok(Bytes::from_static(DONE_FRAME)), st));
                }
            }
        }
    })
}

/// One buffered SSE frame → relayed bytes. Every branch yields something
/// (possibly the frame untouched, for comments and non-data events).
fn frame_step<S>(st: &mut RelayState<S>, frame: Bytes) -> Result<Bytes, BoxError> {
    let Some(payload) = sse_payload(&frame) else {
        // Comment lines and non-data events pass through untouched.
        return Ok(frame);
    };
    if payload == b"[DONE]" {
        // `[DONE]` is end-of-stream: stop relay and drop anything after it.
        st.done = true;
        st.finished = true;
        st.pending.clear();
        return Ok(Bytes::from_static(DONE_FRAME));
    }
    if st.stream_id.is_none() {
        // Read for the truncation frame's id only; the frame itself is
        // relayed byte for byte.
        st.stream_id = convert::top_level_str(payload, b"id").map(str::to_string);
    }
    Ok(frame)
}

/// A final chunk marking the answer as cut short: `finish_reason: "length"`
/// plus the cause in an `error` object — never followed by `[DONE]`.
fn truncation_frame<S>(st: &RelayState<S>, message: &str) -> Bytes {
    let id = st.stream_id.as_deref().unwrap_or("llm-relay-0");
    let mut out = String::with_capacity(message.len() + st.model.len() + 192);
    out.push_str("data: {\"id\":");
    convert::write_json_string(&mut out, id);
    out.push_str(",\"object\":\"chat.completion.chunk\",\"model\":");
    convert::write_json_string(&mut out, &st.model);
    out.push_str(",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}],\"error\":{\"message\":");
    let mut cause = String::with_capacity(message.len() + 24);
    cause.push_str("upstream stream failed: ");
    cause.push_str(message);
    convert::write_json_string(&mut out, &cause);
    out.push_str("}}\n\n");
    Bytes::from(out)
}

/// First `\n\n` boundary at or after `from`, as an inclusive end index.
fn find_frame_end(pending: &[u8], from: usize) -> Option<usize> {
    if from >= pending.len() {
        return None;
    }
    memchr::memmem::find(&pending[from..], b"\n\n").map(|i| from + i + 1)
}

/// The payload of an SSE data frame: `None` for comments and non-data events.
fn sse_payload(frame: &[u8]) -> Option<&[u8]> {
    let frame = trim_ascii(frame);
    if frame.starts_with(b":") {
        return None;
    }
    let payload = frame.strip_prefix(b"data:")?;
    Some(trim_ascii(payload))
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn serve_error(e: ErrorEnvelope) -> Response<ResBody> {
    match e {
        ErrorEnvelope::Status { status, body } => {
            let normalized = error::normalize_error_body(&body, status);
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut resp = json_response(code, Bytes::from(normalized));
            if status == 429 {
                resp.headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_429_SECS));
            }
            resp
        }
        ErrorEnvelope::Transport(cause) => fail(502, &format!("upstream request failed: {cause}")),
    }
}

/// A request body the relay refused to read: over the cap, or broken mid-read.
fn body_error(e: &BoxError, max_body: usize) -> Response<ResBody> {
    if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
        fail(413, &format!("request body exceeds the {max_body} byte limit"))
    } else {
        fail(400, &format!("could not read request body: {e}"))
    }
}

/// Open the upstream connection before any request needs it, by making one
/// cheap request whose answer is discarded. The pool only holds connections the
/// client itself created, so there is no way to warm it without going through
/// the client.
///
/// On a platform that sleeps an idle service, the wake is triggered by a
/// request the proxy is already holding, so this handshake overlaps the rest of
/// startup instead of landing inside that first request's latency.
pub async fn prewarm(state: &State) {
    let req = match Request::builder()
        .method(Method::GET)
        .uri(state.cfg.upstream.clone())
        .header(AUTHORIZATION, state.cfg.authorization.clone())
        .body(Full::new(Bytes::new()))
    {
        Ok(req) => req,
        Err(e) => return eprintln!("prewarm skipped: {e}"),
    };
    // Whatever status comes back has already done the work that mattered: DNS,
    // TCP, TLS and ALPN. The body must still be drained, or the connection
    // cannot be returned to the pool.
    // Timed, because the elapsed value *is* what the first request no longer
    // pays: DNS, TCP, TLS and ALPN to the upstream.
    let started = std::time::Instant::now();
    match state.client.request(req).await {
        Ok(resp) => {
            let status = resp.status();
            let _ = Limited::new(resp.into_body(), MAX_ERROR_BODY).collect().await;
            eprintln!(
                "upstream connection warmed in {}ms (probe answered {status})",
                started.elapsed().as_millis()
            );
        }
        // A cold start that cannot reach upstream is worth saying out loud, but
        // it is not fatal: the request path retries on its own.
        Err(e) => eprintln!("prewarm failed, first request will pay the handshake: {e}"),
    }
}

/// The model id the client asked for, used to label a truncation frame. Read
/// with the byte scanner rather than a parse: the body is the client's whole
/// conversation, and deserializing all of it to look at one string would cost
/// time and allocation proportional to the transcript. An unreadable body
/// yields an empty id rather than refusing the request — the upstream is the
/// one that gets to reject a malformed request.
fn client_model(body: &[u8]) -> String {
    convert::top_level_str(body, b"model")
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Drive `relay_frames` over a scripted upstream: `Ok` items are body
    /// chunks (the split points are deliberate), `Err` is a stream failure.
    async fn relayed(parts: Vec<Result<&'static [u8], &'static str>>) -> Vec<String> {
        let items: Vec<Result<Frame<Bytes>, String>> = parts
            .into_iter()
            .map(|part| match part {
                Ok(chunk) => Ok(Frame::data(Bytes::from_static(chunk))),
                Err(cause) => Err(cause.to_string()),
            })
            .collect();
        let stream = relay_frames(futures_util::stream::iter(items), "test-model".to_string());
        let mut stream = std::pin::pin!(stream);
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(String::from_utf8(item.expect("frame").to_vec()).expect("utf8"));
        }
        out
    }

    #[tokio::test]
    async fn frames_are_relayed_byte_for_byte_however_the_chunks_fall() {
        // The upstream may split anywhere, including inside the `\n\n` that
        // ends a frame. Reassembly must not depend on where the split landed.
        let whole = relayed(vec![Ok(b"data: {\"id\":\"a\"}\n\ndata: [DONE]\n\n")]).await;
        let straddled = relayed(vec![
            Ok(b"data: {\"id\":"),
            Ok(b"\"a\"}\n"),
            Ok(b"\ndata: [DON"),
            Ok(b"E]\n\n"),
        ])
        .await;
        assert_eq!(whole, vec!["data: {\"id\":\"a\"}\n\n", "data: [DONE]\n\n"]);
        assert_eq!(whole, straddled);
    }

    #[tokio::test]
    async fn relay_stops_at_done_and_drops_the_trailing_cost_frame() {
        // The upstream emits `{"choices":[],"cost":"0"}` after [DONE]; every
        // OpenAI client stops at the terminator, so the frame must not be relayed.
        let out = relayed(vec![Ok(
            b"data: {\"id\":\"a\"}\n\ndata: [DONE]\n\ndata: {\"choices\":[],\"cost\":\"0\"}\n\n",
        )])
        .await;
        assert_eq!(out, vec!["data: {\"id\":\"a\"}\n\n", "data: [DONE]\n\n"]);
    }

    #[tokio::test]
    async fn a_clean_end_without_done_still_gets_the_terminator() {
        let out = relayed(vec![Ok(b"data: {\"id\":\"a\"}\n\n")]).await;
        assert_eq!(out.last().unwrap(), "data: [DONE]\n\n");
    }

    #[tokio::test]
    async fn a_stream_cut_mid_frame_ends_on_a_truncation_never_on_done() {
        let out = relayed(vec![
            Ok(b"data: {\"id\":\"router-1\"}\n\n"),
            Ok(b"data: {\"id\":\"router-1\",\"cho"),
        ])
        .await;
        let last = out.last().expect("a frame");
        assert!(!last.contains("[DONE]"), "{last}");
        let payload = last.strip_prefix("data: ").unwrap().trim_end();
        let v: Value = serde_json::from_str(payload).expect("valid JSON");
        // The truncation frame names the stream it belongs to and why it ended.
        assert_eq!(v["id"], "router-1");
        assert_eq!(v["model"], "test-model");
        assert_eq!(v["choices"][0]["finish_reason"], "length");
        assert_eq!(v["error"]["message"], "upstream stream failed: closed mid-frame");
    }

    #[tokio::test]
    async fn a_failing_stream_reports_the_cause_in_band() {
        let out = relayed(vec![
            Ok(b"data: {\"id\":\"router-2\"}\n\n"),
            Err("connection reset"),
        ])
        .await;
        let payload = out.last().unwrap().strip_prefix("data: ").unwrap().trim_end();
        let v: Value = serde_json::from_str(payload).expect("valid JSON");
        assert_eq!(v["id"], "router-2");
        assert_eq!(v["error"]["message"], "upstream stream failed: connection reset");
    }

    #[tokio::test]
    async fn comments_and_non_data_events_pass_through_untouched() {
        let out = relayed(vec![Ok(b": ping\n\nevent: open\n\ndata: [DONE]\n\n")]).await;
        assert_eq!(out, vec![": ping\n\n", "event: open\n\n", "data: [DONE]\n\n"]);
    }

    #[tokio::test]
    async fn a_tool_call_id_is_never_mistaken_for_the_stream_id() {
        // The truncation frame must carry the completion's id, not one dug out
        // of a nested tool call.
        let out = relayed(vec![
            Ok(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call-9\"}]}}]}\n\n"),
            Ok(b"data: partial"),
        ])
        .await;
        let payload = out.last().unwrap().strip_prefix("data: ").unwrap().trim_end();
        let v: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(v["id"], "llm-relay-0");
    }
}
