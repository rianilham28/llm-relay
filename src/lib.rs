//! Slim OpenAI-compatible chat-completions relay.
//!
//! `convert`, `error`, and `identity` are pure wire logic — no HTTP client, no
//! clock of their own — so they compile and run their tests on the host.
//! `relay` holds the fetch path to upstream and `server` the accept loop.

mod convert;
pub mod egress;
mod error;
mod identity;
pub mod relay;
pub mod server;

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{ALLOW, CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// One response body type for every path, so a streamed upstream body and a
/// composed envelope can come back from the same function.
pub type ResBody = BoxBody<Bytes, BoxError>;
/// Requests go out as `Full<Bytes>`: the body is buffered anyway, because a
/// retry has to be able to send it a second time.
pub type UpstreamClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Decode a XOR-obfuscated byte array into a `&'static str` **at compile time**.
///
/// The upstream identity (its URL, the client header names, the agent string)
/// is stored XOR'd so the source is not greppable for it, but the whole thing
/// resolves in a const context — the plaintext is materialized by the compiler,
/// never assembled at runtime, and there is no decode cost on any request path.
/// Reversing it is deliberately trivial: XOR every byte with the one-byte key.
#[macro_export]
macro_rules! deob {
    ($key:literal, [$($b:literal),* $(,)?]) => {{
        const OBF: &[u8] = &[$($b),*];
        const N: usize = OBF.len();
        const DEC: [u8; N] = {
            let mut out = [0u8; N];
            let mut i = 0;
            while i < N {
                out[i] = OBF[i] ^ $key;
                i += 1;
            }
            out
        };
        match ::core::str::from_utf8(&DEC) {
            Ok(s) => s,
            Err(_) => panic!("obfuscated constant is not valid UTF-8"),
        }
    }};
}

/// The upstream chat-completions endpoint. Obfuscated in source, decoded at
/// compile time — see [`deob`].
pub const UPSTREAM: &str = deob!(
    0x5A,
    [
        0x32, 0x2e, 0x2e, 0x2a, 0x29, 0x60, 0x75, 0x75, 0x35, 0x2a, 0x3f, 0x34, 0x39, 0x35, 0x3e,
        0x3f, 0x74, 0x3b, 0x33, 0x75, 0x20, 0x3f, 0x34, 0x75, 0x2c, 0x6b, 0x75, 0x39, 0x32, 0x3b,
        0x2e, 0x75, 0x39, 0x35, 0x37, 0x2a, 0x36, 0x3f, 0x2e, 0x33, 0x35, 0x34, 0x29,
    ]
);
/// The one value still read from the environment: platforms assign the port
/// they route to, so this is a contract rather than a tuning knob.
pub const DEFAULT_PORT: u16 = 8787;
/// The relay must buffer a request body to be able to retry it, so an
/// unbounded body is an unbounded allocation. workerd capped this for us.
pub const MAX_BODY: usize = 32 * 1024 * 1024;
pub const MAX_RETRIES: u32 = 1;

/// Idle upstream connections are held for a long time on purpose. A cold
/// connection to upstream measured 400-500ms of DNS + TCP + TLS, which dwarfs
/// everything else this process does, so keeping one warm across a traffic gap
/// is the single largest latency lever available.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const POOL_MAX_IDLE_PER_HOST: usize = 32;
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_TCP_KEEPALIVE: Duration = Duration::from_secs(60);
/// Measured with `oha -c 100` on the no-upstream path: throughput is flat from
/// 1 to 4 workers (98-100k rps) and 8 workers buys 11% more rps for a p99 of
/// 7.2ms against 3.1ms. The tail is worth more than the throughput here, since
/// the relay is never the bottleneck — upstream is.
const WORKER_CEILING: usize = 4;

/// Resolved once at startup: the URI pre-parsed, the credential pre-encoded as
/// a header value, and the worker count sized to the machine. Every value is a
/// constant except the port, so there is no configuration to get wrong.
pub struct Config {
    pub upstream: Uri,
    pub authorization: HeaderValue,
    pub max_retries: u32,
    pub max_body: usize,
    pub bind: IpAddr,
    pub port: u16,
    pub egress_v4: Uri,
    pub egress_v6: Uri,
    pub worker_threads: usize,
}

impl Config {
    pub fn new() -> Result<Self, BoxError> {
        let key = upstream_key();
        let mut authorization = HeaderValue::try_from(format!("Bearer {key}"))
            .map_err(|_| "the upstream key contains bytes that cannot go in a header")?;
        // Keeps the credential out of HPACK's shared table and out of logs.
        authorization.set_sensitive(true);

        Ok(Self {
            upstream: UPSTREAM.parse()?,
            authorization,
            max_retries: MAX_RETRIES,
            max_body: MAX_BODY,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: listen_port(),
            egress_v4: egress::V4_PROBE.parse()?,
            egress_v6: egress::V6_PROBE.parse()?,
            worker_threads: worker_threads(),
        })
    }
}

/// The upstream credential, read from the environment so no credential is ever
/// compiled into the binary. Absent (or blank), it falls back to the placeholder
/// that only ever answers 401 — which keeps a public build runnable without a
/// real key, but a real key must be supplied at runtime, never committed.
fn upstream_key() -> String {
    std::env::var("RELAY_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| "public".to_string())
}

/// `PORT` is how a platform tells a container where to listen; ignoring it
/// would mean every deployment needs its target port set by hand.
fn listen_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// A container usually sees every core on the host, not its own share, so an
/// unbounded count would size the pool for hardware this process cannot use —
/// and pay for it in tail latency.
fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, WORKER_CEILING)
}

/// Process-wide state: the config and the pooled upstream client. One client
/// for the whole process is the point — its connection pool is what removes a
/// TLS handshake from every request.
pub struct State {
    pub cfg: Config,
    pub client: UpstreamClient,
}

impl State {
    pub fn new(cfg: Config) -> Self {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        // Small SSE frames must not wait on Nagle in either direction.
        http.set_nodelay(true);
        http.set_keepalive(Some(UPSTREAM_TCP_KEEPALIVE));

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            // Roots are compiled in, so the runtime image needs no CA bundle.
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);

        let client = Client::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            // A pooled h2 connection that died silently must be discovered by
            // the keepalive ping, not by a request failing on it.
            .http2_keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
            .http2_keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT)
            .http2_keep_alive_while_idle(true)
            // A long conversation transcript is a large upload; letting hyper
            // size the h2 window beats a fixed default on any real link.
            .http2_adaptive_window(true)
            .build(https);

        Self { cfg, client }
    }
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Both service responses are fixed text, so they are assembled at compile
/// time rather than built through a serializer on every request.
const HEALTHZ_BODY: &str = r#"{"status":"ok"}"#;
const INDEX_BODY: &str = concat!(
    r#"{"name":"llm-relay","version":""#,
    env!("CARGO_PKG_VERSION"),
    r#"","endpoints":["POST /v1/chat/completions","GET /egress","GET /healthz"]}"#
);

pub async fn route(req: Request<Incoming>, state: Arc<State>) -> Result<Response<ResBody>, Infallible> {
    let resp = match (req.method(), req.uri().path()) {
        (&Method::GET, "/healthz") => json_response(StatusCode::OK, Bytes::from_static(HEALTHZ_BODY.as_bytes())),
        (&Method::GET, "/egress") => egress::egress(&state).await,
        (&Method::GET, "/") => json_response(StatusCode::OK, Bytes::from_static(INDEX_BODY.as_bytes())),
        (&Method::POST, "/v1/chat/completions" | "/chat/completions") => relay::chat(req, &state).await,
        // A path that exists but not for this method: saying "unknown endpoint"
        // would send the caller looking for the wrong problem.
        (_, "/v1/chat/completions" | "/chat/completions") => not_allowed("POST"),
        (_, "/healthz" | "/egress" | "/") => not_allowed("GET"),
        (_, path) => fail(404, &format!("unknown endpoint: {path}")),
    };
    Ok(resp)
}

pub(crate) fn json_ct() -> HeaderValue {
    HeaderValue::from_static("application/json; charset=utf-8")
}

pub(crate) fn full(body: Bytes) -> ResBody {
    Full::new(body).map_err(|never| match never {}).boxed()
}

pub(crate) fn json_response(status: StatusCode, body: Bytes) -> Response<ResBody> {
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(CONTENT_TYPE, json_ct());
    resp
}

/// 405 always names the methods that would have worked, as RFC 9110 requires.
fn not_allowed(allow: &'static str) -> Response<ResBody> {
    let mut resp = fail(405, &format!("method not allowed; use {allow}"));
    resp.headers_mut().insert(ALLOW, HeaderValue::from_static(allow));
    resp
}

/// A terminal failure, in the OpenAI error envelope a strict client switches on.
pub(crate) fn fail(status: u16, message: &str) -> Response<ResBody> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    json_response(code, Bytes::from(error::openai_error_body(status, message)))
}
