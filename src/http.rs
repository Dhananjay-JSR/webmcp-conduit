//! MCP over HTTP.
//!
//! stdio serves one page to one client that spawned the process. A hosted
//! conduit serves many pages to many callers, which changes two things.
//!
//! **Every page needs its own thread.** A QuickJS context is not `Send` — it
//! cannot move between threads, and it cannot be shared. So each (target,
//! session) pair gets a thread that owns its engine for as long as it lives,
//! and requests reach it over a channel. This is also what makes concurrency
//! honest: two callers working on two sites genuinely run at the same time,
//! rather than queueing behind one interpreter.
//!
//! **Loading a page is expensive** — a network fetch, a full JavaScript boot,
//! and settling the event loop. Doing that per request would make every tool
//! call cost a page load, so engines are kept and reused, and retired when they
//! go quiet.

use crate::mcp;
use crate::session;
use anyhow::{anyhow, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Mutex};

/// How long an engine may sit unused before it is retired. Long enough that a
/// conversation with pauses in it keeps its page, short enough that an idle
/// deployment is not holding a JavaScript heap per visitor.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A page load is slow, and a caller should be told so rather than left hanging.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Bodies are small JSON-RPC messages. The cap is what stops an unauthenticated
/// endpoint from being asked to buffer something enormous.
const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Config {
    pub allow_private: bool,
    pub allow_hosts: Vec<String>,
    pub no_scripts: bool,
}

enum Job {
    Rpc {
        request: Value,
        reply: oneshot::Sender<Option<Value>>,
    },
}

/// A running engine, addressed by channel.
struct Engine {
    jobs: mpsc::UnboundedSender<Job>,
    last_used: Instant,
}

type Registry = Arc<Mutex<HashMap<String, Engine>>>;

pub async fn serve(addr: SocketAddr, config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));

    {
        let registry = registry.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                let mut engines = registry.lock().await;
                // Dropping the sender closes the channel, the worker's recv
                // loop ends, and the worker writes its session back on the way
                // out. Retirement is not a way to lose work.
                engines.retain(|key, engine| {
                    let alive = engine.last_used.elapsed() < IDLE_TIMEOUT;
                    if !alive {
                        tracing::info!(target: "conduit", "retiring idle engine {key}");
                    }
                    alive
                });
            }
        });
    }

    eprintln!("conduit: listening on http://{addr}");
    eprintln!("conduit: POST /mcp/v1/connect?url=<site>&session=<id>");

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let config = config.clone();

        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = service_fn(move |req| route(req, registry.clone(), config.clone(), peer));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(target: "conduit", "connection from {peer} ended: {e}");
            }
        });
    }
}

fn json_response(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("a JSON response is always well-formed")
}

fn rpc_error(code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}})
}

async fn route(
    req: Request<hyper::body::Incoming>,
    registry: Registry,
    config: Config,
    peer: SocketAddr,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();

    let response = match (req.method(), path.as_str()) {
        (&Method::GET, "/healthz") => json_response(
            StatusCode::OK,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}),
        ),
        (&Method::POST, "/mcp/v1/connect") => match connect(req, registry, config).await {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(target: "conduit", "{peer}: {e}");
                json_response(StatusCode::BAD_REQUEST, rpc_error(-32600, e.to_string()))
            }
        },
        (&Method::GET, "/mcp/v1/connect") => json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            rpc_error(
                -32600,
                "POST a JSON-RPC message to this endpoint. Server-initiated \
                 streaming is not implemented; every response is the answer to \
                 one request."
                    .into(),
            ),
        ),
        _ => json_response(
            StatusCode::NOT_FOUND,
            rpc_error(-32601, format!("no route for {path}")),
        ),
    };

    Ok(response)
}

async fn connect(
    req: Request<hyper::body::Incoming>,
    registry: Registry,
    config: Config,
) -> Result<Response<Full<Bytes>>> {
    let query = req.uri().query().unwrap_or("").to_string();
    let (target, session_id) = parse_query(&query)?;

    validate_target(&target, &config)?;
    if let Some(id) = &session_id {
        // Rejected here rather than deeper: this is the untrusted edge, and a
        // session id becomes a directory name.
        session::validate_id(id)?;
    }

    let body = read_body(req).await?;
    let message: Value =
        serde_json::from_slice(&body).map_err(|e| anyhow!("body is not JSON: {e}"))?;

    let key = match &session_id {
        Some(id) => format!("{target}#{id}"),
        None => format!("{target}#"),
    };

    let jobs = engine_for(&key, &target, session_id, &config, &registry).await?;

    let (reply_tx, reply_rx) = oneshot::channel();
    jobs.send(Job::Rpc {
        request: message,
        reply: reply_tx,
    })
    .map_err(|_| anyhow!("the engine for this page has stopped"))?;

    match reply_rx.await {
        // A notification is answered with 202 and no body, which is what the
        // absence of a JSON-RPC id means.
        Ok(None) => Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Full::new(Bytes::new()))
            .expect("empty response is well-formed")),
        Ok(Some(response)) => Ok(json_response(StatusCode::OK, response)),
        Err(_) => Err(anyhow!("the engine stopped before answering")),
    }
}

async fn read_body(req: Request<hyper::body::Incoming>) -> Result<Bytes> {
    let upper = req.body().size_hint().upper().unwrap_or(u64::MAX);
    if upper > MAX_BODY as u64 {
        return Err(anyhow!(
            "request body is too large (limit {MAX_BODY} bytes)"
        ));
    }
    Ok(req.into_body().collect().await?.to_bytes())
}

/// Find a live engine for this page, or start one.
async fn engine_for(
    key: &str,
    target: &str,
    session_id: Option<String>,
    config: &Config,
    registry: &Registry,
) -> Result<mpsc::UnboundedSender<Job>> {
    {
        let mut engines = registry.lock().await;
        if let Some(engine) = engines.get_mut(key) {
            if !engine.jobs.is_closed() {
                engine.last_used = Instant::now();
                return Ok(engine.jobs.clone());
            }
            // The worker died — a page that threw during load, most likely.
            // Drop it so the next line starts a fresh one rather than handing
            // out a channel nobody is listening to.
            engines.remove(key);
        }
    }

    let jobs = spawn_engine(target.to_string(), session_id, config.no_scripts).await?;

    let mut engines = registry.lock().await;
    engines.insert(
        key.to_string(),
        Engine {
            jobs: jobs.clone(),
            last_used: Instant::now(),
        },
    );
    Ok(jobs)
}

/// The engine needs a deep stack for the same reason the CLI does: a framework
/// reconciler recurses once per node, and React turns the overflow into a blank
/// page rather than a crash.
fn spawn_engine(
    target: String,
    session_id: Option<String>,
    no_scripts: bool,
) -> impl std::future::Future<Output = Result<mpsc::UnboundedSender<Job>>> {
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel::<Job>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

    let spawned = std::thread::Builder::new()
        .name(format!("conduit-engine-{target}"))
        .stack_size(crate::ENGINE_STACK)
        .spawn(move || {
            // Its own runtime, because this thread owns a `!Send` engine and
            // cannot borrow the server's.
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("starting a runtime: {e}")));
                    return;
                }
            };

            runtime.block_on(async move {
                let mut handle = match session_id.as_deref().map(session::Handle::open) {
                    Some(Ok(h)) => Some(h),
                    Some(Err(e)) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                    None => None,
                };

                let mut loaded =
                    match mcp::Session::load(&target, !no_scripts, handle.as_ref()).await {
                        Ok(session) => session,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.to_string()));
                            return;
                        }
                    };

                if ready_tx.send(Ok(())).is_err() {
                    return; // The caller gave up while the page was loading.
                }

                while let Some(job) = jobs_rx.recv().await {
                    match job {
                        Job::Rpc { request, reply } => {
                            let mutating = mcp::mutates(&request);
                            let response = mcp::handle(&mut loaded, &target, &request).await;

                            // Committed per tool call rather than only at
                            // shutdown. A hosted process can be killed without
                            // warning, and losing a caller's work because the
                            // container was recycled is not acceptable.
                            if mutating {
                                commit(handle.as_mut(), &mut loaded);
                            }

                            let _ = reply.send(response);
                        }
                    }
                }

                // The channel closed: retired for idleness, or shutting down.
                commit(handle.as_mut(), &mut loaded);
            });
        });

    async move {
        spawned.context("spawning an engine thread")?;

        match tokio::time::timeout(LOAD_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => Ok(jobs_tx),
            Ok(Ok(Err(e))) => Err(anyhow!("{e}")),
            Ok(Err(_)) => Err(anyhow!("the engine stopped while loading the page")),
            Err(_) => Err(anyhow!(
                "timed out after {}s loading the page",
                LOAD_TIMEOUT.as_secs()
            )),
        }
    }
}

fn commit(handle: Option<&mut session::Handle>, loaded: &mut mcp::Session) {
    let Some(handle) = handle else { return };
    let origin = loaded.origin();
    let cookies = loaded.cookies();

    let storage = match loaded.snapshot() {
        Ok(Some(storage)) => storage,
        Ok(None) => "{}".to_string(),
        Err(e) => {
            tracing::warn!(target: "conduit", "snapshotting session {}: {e}", handle.id());
            "{}".to_string()
        }
    };

    if let Err(e) = handle.commit(&origin, &storage, cookies) {
        tracing::warn!(target: "conduit", "saving session {}: {e}", handle.id());
    }
}

fn parse_query(query: &str) -> Result<(String, Option<String>)> {
    let mut target = None;
    let mut session_id = None;

    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded = percent_decode(value);
        match key {
            "url" => target = Some(decoded),
            // An empty `session=` is no session, not a session named "".
            "session" if !decoded.is_empty() => session_id = Some(decoded),
            _ => {}
        }
    }

    let target = target
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("missing `url`: POST /mcp/v1/connect?url=<site>&session=<id>"))?;
    Ok((target, session_id))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The CLI is run by someone who chose the URL. An HTTP endpoint is handed one
/// by a stranger, which makes it a server-side request forgery engine unless it
/// refuses the addresses that a stranger has no business reaching: the loopback
/// interface, a private network, and cloud metadata services.
fn validate_target(target: &str, config: &Config) -> Result<()> {
    let url = url::Url::parse(target).map_err(|e| anyhow!("`url` is not a valid URL: {e}"))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!(
            "only http and https targets are allowed over HTTP (got `{}`)",
            url.scheme()
        ));
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("`url` has no host"))?
        .to_ascii_lowercase();

    if !config.allow_hosts.is_empty() {
        let permitted = config.allow_hosts.iter().any(|allowed| {
            host == *allowed
                || host
                    .strip_suffix(allowed)
                    .map(|prefix| prefix.ends_with('.'))
                    .unwrap_or(false)
        });
        if !permitted {
            return Err(anyhow!("{host} is not in this server's allowlist"));
        }
    }

    if config.allow_private {
        return Ok(());
    }

    if host == "localhost" || host.ends_with(".localhost") {
        return Err(anyhow!(
            "refusing to reach {host}; pass --allow-private if that is intended"
        ));
    }

    // A hostname that resolves to a private address is still reachable, and DNS
    // rebinding makes a name-only check worthless. This catches the literal
    // form; a full defence belongs in the network, not here, and the allowlist
    // is the real answer for a public deployment.
    //
    // Taken from `Host` rather than parsing `host_str()`: that returns an IPv6
    // address still wrapped in its URL brackets, `[::1]`, which does not parse
    // as an address — so loopback would pass the check.
    let literal = match url.host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    };
    if let Some(ip) = literal {
        if is_private(&ip) {
            return Err(anyhow!(
                "refusing to reach the private address {ip}; pass --allow-private if that is intended"
            ));
        }
    }

    Ok(())
}

fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()   // 169.254.0.0/16 — cloud metadata lives here
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 100.64.0.0/10, carrier-grade NAT and Tailscale.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local and fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // An IPv4 address wearing an IPv6 costume.
                || v6.to_ipv4_mapped().map(|v4| is_private(&IpAddr::V4(v4))).unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            allow_private: false,
            allow_hosts: Vec::new(),
            no_scripts: false,
        }
    }

    #[test]
    fn the_query_carries_a_url_and_an_optional_session() {
        let (target, session) =
            parse_query("url=https%3A%2F%2Fa.example%2Fx&session=alice").unwrap();
        assert_eq!(target, "https://a.example/x");
        assert_eq!(session.as_deref(), Some("alice"));

        let (target, session) = parse_query("url=https%3A%2F%2Fa.example").unwrap();
        assert_eq!(target, "https://a.example");
        assert_eq!(session, None);

        // An empty session is no session, not a session named "".
        assert_eq!(
            parse_query("url=https%3A%2F%2Fa.example&session=")
                .unwrap()
                .1,
            None
        );
        assert!(parse_query("session=alice").is_err());
    }

    #[test]
    fn the_endpoint_refuses_to_be_an_ssrf_proxy() {
        let refuse = |u: &str| validate_target(u, &config()).is_err();

        assert!(refuse("http://127.0.0.1/"));
        assert!(refuse("http://localhost:8080/"));
        assert!(refuse("http://10.0.0.5/"));
        assert!(refuse("http://192.168.1.1/"));
        assert!(refuse("http://172.16.0.1/"));
        // The one every SSRF write-up opens with.
        assert!(refuse("http://169.254.169.254/latest/meta-data/"));
        assert!(refuse("http://[::1]/"));
        assert!(refuse("http://[fd00::1]/"));
        assert!(refuse("http://100.64.0.1/"));
        // Not HTTP at all.
        assert!(refuse("file:///etc/passwd"));
        assert!(refuse("gopher://example.com/"));

        assert!(validate_target("https://example.com/app", &config()).is_ok());
    }

    #[test]
    fn private_targets_are_reachable_when_asked_for() {
        let permissive = Config {
            allow_private: true,
            ..config()
        };
        assert!(validate_target("http://127.0.0.1:8931/", &permissive).is_ok());
        // Still not a non-HTTP scheme, even then.
        assert!(validate_target("file:///etc/passwd", &permissive).is_err());
    }

    #[test]
    fn an_allowlist_pins_the_server_to_known_sites() {
        let pinned = Config {
            allow_hosts: vec!["example.com".into()],
            ..config()
        };
        assert!(validate_target("https://example.com/", &pinned).is_ok());
        assert!(validate_target("https://app.example.com/", &pinned).is_ok());
        assert!(validate_target("https://other.example/", &pinned).is_err());
        // The suffix trap: notexample.com must not pass for example.com.
        assert!(validate_target("https://notexample.com/", &pinned).is_err());
    }

    #[test]
    fn percent_decoding_handles_what_a_url_carries() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fa.example"),
            "https://a.example"
        );
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // A stray percent is left alone rather than swallowing the next char.
        assert_eq!(percent_decode("100%"), "100%");
    }
}
