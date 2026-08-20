# llm-relay

A slim, OpenAI-compatible chat-completions relay. It forwards
`POST /v1/chat/completions` to a single upstream endpoint over one pooled,
keep-alive HTTP client, retries once on retryable failures, and returns
responses **verbatim** — request bodies go upstream byte for byte, and response
bodies and SSE frames come back byte for byte. No lanes, no admission control,
no ops machinery.

Native `hyper` 1.x + `tokio`, shipped in a distroless container.

## Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/v1/chat/completions` (alias `/chat/completions`) | Relay a chat completion; honors the request's `stream` flag |
| `GET`  | `/egress`  | The source address upstream sees us from (both IP families) |
| `GET`  | `/healthz` | Liveness — `{"status":"ok"}` |
| `GET`  | `/`        | Service index: name, version, endpoints |

A known path with the wrong method answers `405` + `Allow`; an unknown path `404`.

## Configuration

The relay reads exactly two environment variables; everything else is a tuned
constant compiled in.

| Var | Default | Meaning |
|-----|---------|---------|
| `PORT` | `8787` | Port to listen on. Platforms assign the port they route to. |
| `RELAY_API_KEY` | `public` (placeholder) | Bearer credential sent upstream, read at startup. The default only ever answers `401`. **Supply a real key at runtime; never commit one.** |

## Build & run

```sh
cargo build --release --locked --bin llm-relay
RELAY_API_KEY=... ./target/release/llm-relay    # listens on $PORT (default 8787)
```

Container:

```sh
docker build -t llm-relay:latest .
docker run -d -p 8787:8787 -e RELAY_API_KEY=... llm-relay:latest
# or
docker compose up -d --build
```

Smoke:

```sh
curl localhost:8787/healthz    # {"status":"ok"}
```

## License

MIT
