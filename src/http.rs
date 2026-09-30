//! MCP over HTTP, on the SDK's Streamable HTTP transport.
//!
//! A conduit served over HTTP does not take a URL from its caller. The operator
//! declares the sites up front and each becomes a route:
//!
//! ```text
//! conduit http --site notes=https://notes.example
//! POST /notes
//! ```
//!
//! Both halves of that matter.
//!
//! **The target is not a parameter.** A server that fetches whatever URL it is
//! handed is a server-side request forgery engine, and no amount of allowlisting
//! and private-address filtering makes it not one — it only narrows the door.
//! Declaring the sites removes the door.
//!
//! **Neither is the session.** A session holds cookies and storage, which is to
//! say it holds someone's logged-in state. If the caller names the session, then
//! anyone who guesses a name is that person. So a mount *is* a session: the
//! route decides which browser profile it speaks for, and the caller cannot ask
//! for a different one. Two profiles of one site are two mounts.
//!
//! Everything protocol-shaped — framing, SSE, `Mcp-Session-Id`,
//! `MCP-Protocol-Version`, `Origin` validation, status codes — belongs to the
//! SDK, which is the entire reason for using it.

use crate::engine;
use crate::server::Conduit;
use anyhow::{anyhow, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
// The SDK exposes its Streamable HTTP endpoint as a tower service.
use tower_service::Service as _;

/// One declared site.
#[derive(Clone)]
pub struct Site {
    /// The path segment it is served at, and the session it speaks for.
    pub name: String,
    pub url: String,
}

#[derive(Clone)]
pub struct Config {
    pub sites: Vec<Site>,
    /// Serve `/connect?url=…`, letting a caller name the page.
    pub allow_any_site: bool,
    /// Permit loopback and private addresses as ad-hoc targets.
    pub allow_private_sites: bool,
    pub allow_origins: Vec<String>,
    /// Extra `Host` values this server answers to, beyond loopback.
    pub allow_hosts: Vec<String>,
    pub no_scripts: bool,
    /// How many pages may be held in memory at once.
    pub max_engines: usize,
}

/// Paths that are the server's own, so a site may not take them.
const RESERVED: &[&str] = &["healthz", "connect"];

/// `--site name=https://example.com`
///
/// The name becomes a path segment and a directory name, so it is restricted
/// rather than escaped.
pub fn parse_site(spec: &str) -> Result<Site> {
    let (name, url) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("expected `name=url`, got `{spec}`"))?;

    let name = name.trim();
    crate::session::validate_id(name)
        .with_context(|| format!("`{name}` is not usable as a site name"))?;

    if RESERVED.contains(&name) {
        return Err(anyhow!(
            "`{name}` is reserved by the server; choose another site name"
        ));
    }

    let parsed =
        url::Url::parse(url.trim()).with_context(|| format!("`{url}` is not a valid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!(
            "site `{name}` must be http or https, got `{}`",
            parsed.scheme()
        ));
    }

    Ok(Site {
        name: name.to_string(),
        url: parsed.to_string(),
    })
}

/// A page named by the request rather than by the operator.
///
/// `?url=` is the page. `?session=` is optional, and on a server that anyone
/// can reach it is a **credential, not a name**: whoever knows it gets whatever
/// it is signed into. A random value is the only sensible one; `alice` means
/// the first person to try `alice` is alice. The id is the session's name on
/// disk too, which is why it is restricted rather than escaped.
fn ad_hoc_site(query: &str, config: &Config) -> Result<Site> {
    let target = query_value(query, "url")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("missing `url`: /connect?url=<site>[&session=<secret>]"))?;

    validate_target(&target, config)?;

    Ok(Site {
        // Named by a hash of the URL so two callers asking for the same page
        // share one engine and the second does not pay for the load again.
        name: format!("connect-{:x}", fnv(&target)),
        url: target,
    })
}

/// One query parameter, percent-decoded.
fn query_value(query: &str, wanted: &str) -> Option<String> {
    query.split('&').filter(|p| !p.is_empty()).find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == wanted).then(|| percent_decode(value))
    })
}

/// The session this request asks for, if any.
///
/// Absent is the common case and the safe one: this MCP connection gets a
/// private engine that writes nothing to disk and is dropped with the
/// connection. Isolation comes from the transport — the SDK builds one handler
/// per connection — rather than from anything the caller has to remember.
///
/// Present means persistence across connections, and the id is then a
/// **credential rather than a name**: whoever presents it joins that engine and
/// gets whatever it is signed into. It should be random. `alice` means the
/// first person to try `alice` is alice.
fn requested_session(query: &str) -> Result<Option<String>> {
    match query_value(query, "session").filter(|s| !s.is_empty()) {
        Some(id) => {
            // It becomes a directory name.
            crate::session::validate_id(&id)?;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

/// A stable, short identifier for a URL, used to pool engines for sessionless
/// callers. FNV-1a: not a security boundary, just a way to avoid a filesystem
/// path built from an arbitrary URL.
fn fnv(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The CLI is run by someone who chose the URL. `/connect` is handed one by a
/// stranger, which makes it a server-side request forgery engine unless it
/// refuses the addresses a stranger has no business reaching: the loopback
/// interface, a private network, and cloud metadata services.
fn validate_target(target: &str, config: &Config) -> Result<()> {
    let url = url::Url::parse(target).map_err(|e| anyhow!("`url` is not a valid URL: {e}"))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!(
            "only http and https targets are allowed (got `{}`)",
            url.scheme()
        ));
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("`url` has no host"))?
        .to_ascii_lowercase();

    if config.allow_private_sites {
        return Ok(());
    }

    if host == "localhost" || host.ends_with(".localhost") {
        return Err(anyhow!(
            "refusing to reach {host}; pass --allow-private-sites if that is intended"
        ));
    }

    // A hostname that resolves to a private address is still reachable, and DNS
    // rebinding makes a name-only check worthless. This catches the literal
    // form; a full defence belongs in the network.
    //
    // Taken from `Host` rather than by parsing `host_str()`: that returns an
    // IPv6 address still wrapped in its URL brackets, `[::1]`, which does not
    // parse as an address — so loopback would pass the check.
    let literal = match url.host() {
        Some(url::Host::Ipv4(v4)) => Some(std::net::IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(std::net::IpAddr::V6(v6)),
        _ => None,
    };
    if let Some(ip) = literal {
        if is_private(&ip) {
            return Err(anyhow!(
                "refusing to reach the private address {ip}; pass --allow-private-sites if that is intended"
            ));
        }
    }

    Ok(())
}

fn is_private(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
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
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| is_private(&IpAddr::V4(v4)))
                    .unwrap_or(false)
        }
    }
}

/// Percent-decoding, for a query string this layer parses itself.
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

type Services = Arc<Mutex<HashMap<String, StreamableHttpService<Conduit, LocalSessionManager>>>>;

pub async fn serve(addr: SocketAddr, config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let pool = engine::Pool::new(config.max_engines);
    pool.start_reaper();
    let services: Services = Arc::new(Mutex::new(HashMap::new()));
    let config = Arc::new(config);

    // The address the OS actually gave us, which differs from what was asked
    // for whenever the port was 0 — and a caller that cannot discover the port
    // cannot connect to it.
    let bound = listener.local_addr().unwrap_or(addr);
    eprintln!("conduit: listening on http://{bound}");
    for site in &config.sites {
        eprintln!("conduit:   POST /{}  ->  {}", site.name, site.url);
    }
    if config.allow_any_site {
        eprintln!("conduit:   POST /connect?url=<site>");
    }
    eprintln!(
        "conduit: every request needs ?session=<random-string>; it is a credential, not a name"
    );

    loop {
        let (stream, peer) = listener.accept().await?;
        let pool = pool.clone();
        let services = services.clone();
        let config = config.clone();

        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = service_fn(move |req| {
                route(req, pool.clone(), services.clone(), config.clone(), peer)
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(target: "conduit", "connection from {peer} ended: {e}");
            }
        });
    }
}

/// The SDK's responses carry an infallible body, so ours must too — the two
/// share one return type.
type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

fn json_response(status: StatusCode, body: Value) -> Response<BoxBody> {
    let full = Full::new(Bytes::from(body.to_string()));
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(BodyExt::boxed(full.map_err(|never| match never {})))
        .expect("a JSON response is always well-formed")
}

fn rpc_error(code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}})
}

async fn route(
    req: Request<hyper::body::Incoming>,
    pool: engine::Pool,
    services: Services,
    config: Arc<Config>,
    peer: SocketAddr,
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    let path = req.uri().path().to_string();

    if req.method() == Method::GET && path == "/healthz" {
        return Ok(json_response(
            StatusCode::OK,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}),
        ));
    }

    // What this server serves. Names only: the URLs behind them are the
    // operator's business, not a caller's.
    if req.method() == Method::GET && path == "/" {
        let names: Vec<&str> = config.sites.iter().map(|s| s.name.as_str()).collect();
        return Ok(json_response(StatusCode::OK, json!({"sites": names})));
    }

    // A site is served at its own name. There is no version prefix: this server
    // speaks whatever MCP version the SDK negotiates per connection, so a
    // number in the path would only be a second, staler answer to that.
    let name = path.trim_start_matches('/');

    // `/connect?url=` names the page in the request rather than the config.
    //
    // Off unless asked for, because a server that fetches whatever URL it is
    // handed is a server-side request forgery engine. What makes it tolerable
    // when it is on: the target is checked against the addresses a stranger has
    // no business reaching, and an ad-hoc page never gets a session — there is
    // no name to key one by, so there is no signed-in identity for a caller to
    // borrow by guessing it.
    if name == "connect" {
        if !config.allow_any_site {
            return Ok(json_response(
                StatusCode::NOT_FOUND,
                rpc_error(
                    -32601,
                    "this server does not accept a `url`; GET / lists what it serves".to_string(),
                ),
            ));
        }

        let site = match ad_hoc_site(req.uri().query().unwrap_or(""), &config) {
            Ok(site) => site,
            Err(e) => {
                tracing::warn!(target: "conduit", "{peer}: {e:#}");
                return Ok(json_response(
                    StatusCode::BAD_REQUEST,
                    rpc_error(-32600, format!("{e:#}")),
                ));
            }
        };

        let session_id = match requested_session(req.uri().query().unwrap_or("")) {
            Ok(session) => session,
            Err(e) => {
                return Ok(json_response(
                    StatusCode::BAD_REQUEST,
                    rpc_error(-32600, format!("{e:#}")),
                ));
            }
        };

        return match connect(req, site, session_id, pool, services, config).await {
            Ok(response) => Ok(response),
            Err(e) => {
                let detail = format!("{e:#}");
                tracing::warn!(target: "conduit", "{peer}: {detail}");
                Ok(json_response(
                    StatusCode::BAD_GATEWAY,
                    rpc_error(-32603, detail),
                ))
            }
        };
    }

    // An unknown name and a known one must be told apart by the operator's
    // config, never by anything in the request.
    let Some(site) = config.sites.iter().find(|s| s.name == name) else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            rpc_error(
                -32601,
                format!("`{name}` is not a site this server serves; GET / lists them"),
            ),
        ));
    };
    let site = site.clone();

    let session_id = match requested_session(req.uri().query().unwrap_or("")) {
        Ok(session) => session,
        Err(e) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                rpc_error(-32600, format!("{e:#}")),
            ));
        }
    };

    match connect(req, site, session_id, pool, services, config).await {
        Ok(response) => Ok(response),
        Err(e) => {
            // `{:#}` rather than `{}`: the outermost context alone says
            // "fetching <url>" and drops the reason, so a refused connection
            // and a 500 from the site look identical to whoever is debugging.
            let detail = format!("{e:#}");
            tracing::warn!(target: "conduit", "{peer}: {detail}");
            Ok(json_response(
                StatusCode::BAD_GATEWAY,
                rpc_error(-32603, detail),
            ))
        }
    }
}

/// Buffer the body so it can be logged, then hand the request on.
///
/// Bodies here are single JSON-RPC messages, so this costs nothing worth
/// measuring, and `CONDUIT_LOG=debug` showing exactly what a client sent is the
/// difference between diagnosing an interoperability problem and guessing at
/// it. `StreamableHttpService` is generic over the body type, so it takes the
/// buffered request unchanged.
async fn buffer(req: Request<hyper::body::Incoming>) -> Result<Request<Full<Bytes>>> {
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await?.to_bytes();

    if tracing::enabled!(target: "http", tracing::Level::DEBUG) {
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string()
        };
        tracing::debug!(
            target: "http",
            "{} {} | accept={} content-type={} mcp-session-id={} mcp-protocol-version={} origin={} host={} | {}",
            parts.method,
            parts.uri.path(),
            header("accept"),
            header("content-type"),
            header("mcp-session-id"),
            header("mcp-protocol-version"),
            header("origin"),
            header("host"),
            String::from_utf8_lossy(&bytes),
        );
    }

    Ok(Request::from_parts(parts, Full::new(bytes)))
}

async fn connect(
    req: Request<hyper::body::Incoming>,
    site: Site,
    session_id: Option<String>,
    pool: engine::Pool,
    services: Services,
    config: Arc<Config>,
) -> Result<Response<BoxBody>> {
    // Loopback stays permitted whatever else is declared, so that a deployed
    // server is still reachable from a shell on the box it runs on.
    let mut allowed_hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    allowed_hosts.extend(config.allow_hosts.iter().cloned());

    // One Streamable HTTP service per (page, session secret). The service is
    // only a router: it builds a handler per MCP connection, and those are what
    // hold engines. Two callers presenting different secrets — or none — never
    // meet.
    let key = engine::Pool::key(&site.url, session_id.as_deref());

    let mut service = {
        let mut services = services.lock().await;
        match services.get(&key) {
            Some(service) => service.clone(),
            None => {
                let spec = crate::server::EngineSpec {
                    url: Arc::from(site.url.as_str()),
                    session: session_id.map(Arc::from),
                    no_scripts: config.no_scripts,
                    pool: pool.clone(),
                };

                let service = StreamableHttpService::new(
                    // Called once per MCP connection, which is where isolation
                    // comes from: two clients get two handlers, and without a
                    // shared session secret, two engines. Nothing loads here —
                    // the handler does that on the first request that needs a
                    // page, so `initialize` answers immediately rather than
                    // blocking for the tens of seconds a real page takes.
                    move || Ok(crate::server::Conduit::new(spec.clone())),
                    Arc::new(LocalSessionManager::default()),
                    StreamableHttpServerConfig::default()
                        // Sessions on, so the transport issues an
                        // `Mcp-Session-Id` at initialize and clients echo it
                        // back. That identifier is what separates one caller
                        // from another, and it is a random UUID the caller
                        // never chooses — unguessable in a way `?session=alice`
                        // is not.
                        .with_legacy_session_mode(true)
                        // And this is what made sessions look unaffordable.
                        //
                        // A priming event opens every SSE stream with an empty
                        // `data:` field, and a client that parses each `data:`
                        // line as JSON gets `JSON.parse("")` and reports a
                        // syntax error before seeing a message — Postman does
                        // exactly that. It reads as the price of session mode
                        // and is not: it comes from `sse_retry`, and turning
                        // that off keeps sessions and drops the frame.
                        .with_sse_retry(None)
                        // Plain JSON responses rather than an SSE stream.
                        //
                        // conduit never sends anything before a response — no
                        // sampling, no progress, no server-initiated requests —
                        // so a stream buys nothing, and the SDK falls back to
                        // SSE by itself if a handler ever does emit something.
                        //
                        // It also avoids a real interoperability problem.
                        // Legacy session mode opens every stream with a priming
                        // event whose data field is empty, and a client that
                        // JSON-parses each `data:` gets `JSON.parse("")` and
                        // reports a syntax error before it ever sees a message.
                        // Postman's MCP client does exactly that.
                        // Host values this server answers to. The SDK defaults
                        // to loopback only, which is right for a local server
                        // and would 403 every request to a deployed one, so the
                        // public hostname has to be declared.
                        .with_allowed_hosts(allowed_hosts.clone())
                        // The spec requires Origin validation to prevent DNS
                        // rebinding. `enforce_origin_validation` is not
                        // optional here: an empty allowlist otherwise means
                        // *no* checking at all rather than "reject every
                        // browser origin", so leaving it off gives a server
                        // that looks locked down and is not.
                        .with_allowed_origins(config.allow_origins.clone())
                        .enforce_origin_validation(),
                );
                services.insert(key.clone(), service.clone());
                service
            }
        }
    };

    let response = service
        .call(buffer(req).await?)
        .await
        .map_err(|e| anyhow!("streamable http transport: {e}"))?;

    tracing::debug!(
        target: "http",
        "-> {} {}",
        response.status(),
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
    );

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            sites: Vec::new(),
            allow_any_site: false,
            allow_private_sites: false,
            allow_origins: Vec::new(),
            allow_hosts: Vec::new(),
            no_scripts: false,
            max_engines: crate::engine::DEFAULT_MAX_ENGINES,
        }
    }

    fn open() -> Config {
        Config {
            allow_any_site: true,
            ..config()
        }
    }

    #[test]
    fn connect_refuses_to_be_an_ssrf_proxy() {
        let refuse = |u: &str| validate_target(u, &open()).is_err();

        assert!(refuse("http://127.0.0.1/"));
        assert!(refuse("http://localhost:8080/"));
        assert!(refuse("http://10.0.0.5/"));
        assert!(refuse("http://192.168.1.1/"));
        assert!(refuse("http://172.16.0.1/"));
        // The one every SSRF write-up opens with.
        assert!(refuse("http://169.254.169.254/latest/meta-data/"));
        // Brackets are why this needs url::Host rather than host_str().
        assert!(refuse("http://[::1]/"));
        assert!(refuse("http://[fd00::1]/"));
        assert!(refuse("http://100.64.0.1/"));
        // Not HTTP at all: file:// would read the server's own disk.
        assert!(refuse("file:///etc/passwd"));
        assert!(refuse("gopher://example.com/"));

        assert!(validate_target("https://example.com/app", &open()).is_ok());
    }

    #[test]
    fn private_targets_need_asking_for() {
        let permissive = Config {
            allow_private_sites: true,
            ..open()
        };
        assert!(validate_target("http://127.0.0.1:8931/", &permissive).is_ok());
        // Still not a non-HTTP scheme, even then.
        assert!(validate_target("file:///etc/passwd", &permissive).is_err());
    }

    #[test]
    fn two_callers_on_one_url_share_an_engine() {
        let site = ad_hoc_site("url=https%3A%2F%2Fa.example%2Fx", &open()).unwrap();
        assert_eq!(site.url, "https://a.example/x");

        // The same URL resolves to the same name, so a second caller reuses the
        // loaded page rather than paying for it again.
        let again = ad_hoc_site("url=https%3A%2F%2Fa.example%2Fx", &open()).unwrap();
        assert_eq!(site.name, again.name);

        // A different URL is a different engine.
        let other = ad_hoc_site("url=https%3A%2F%2Fb.example%2Fx", &open()).unwrap();
        assert_ne!(site.name, other.name);
    }

    #[test]
    fn statefulness_is_the_callers_choice() {
        // No session: the page is served fresh and nothing reaches disk.
        assert_eq!(
            requested_session("url=https%3A%2F%2Fa.example").unwrap(),
            None
        );
        assert_eq!(requested_session("").unwrap(), None);
        // An empty session is no session, not a session named "".
        assert_eq!(requested_session("session=").unwrap(), None);

        // A session: cookies and storage persist under that id.
        assert_eq!(
            requested_session("session=Xk7pQ2mZ9vRt4wLn")
                .unwrap()
                .as_deref(),
            Some("Xk7pQ2mZ9vRt4wLn")
        );

        // It becomes a directory name, so it is restricted rather than escaped.
        assert!(requested_session("session=../etc").is_err());
        assert!(requested_session("session=a/b").is_err());
    }

    #[test]
    fn a_session_separates_two_callers_on_one_page() {
        // The pool key is what keeps them apart: same page, different sessions,
        // different engines — otherwise one would see the other's storage.
        let page = "https://a.example/x";
        assert_ne!(
            crate::engine::Pool::key(page, Some("alice")),
            crate::engine::Pool::key(page, Some("bob"))
        );
        assert_ne!(
            crate::engine::Pool::key(page, Some("alice")),
            crate::engine::Pool::key(page, None)
        );
    }

    #[test]
    fn connect_needs_a_url() {
        assert!(ad_hoc_site("", &open()).is_err());
        assert!(ad_hoc_site("session=abc", &open()).is_err());
        assert!(ad_hoc_site("url=", &open()).is_err());
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

    #[test]
    fn a_site_is_a_name_and_a_url() {
        let site = parse_site("notes=https://notes.example/app").unwrap();
        assert_eq!(site.name, "notes");
        assert_eq!(site.url, "https://notes.example/app");
    }

    #[test]
    fn site_names_cannot_escape_a_path_or_a_directory() {
        // The name is both a route segment and a session directory.
        assert!(parse_site("../etc=https://a.example").is_err());
        assert!(parse_site("a/b=https://a.example").is_err());
        assert!(parse_site("=https://a.example").is_err());
        assert!(parse_site("no-equals-sign").is_err());
    }

    #[test]
    fn a_site_cannot_shadow_the_servers_own_paths() {
        assert!(parse_site("healthz=https://a.example").is_err());
    }

    #[test]
    fn a_site_url_must_be_http() {
        // file:// would read the server's own disk.
        assert!(parse_site("x=file:///etc/passwd").is_err());
        assert!(parse_site("x=gopher://a.example").is_err());
        assert!(parse_site("x=not a url").is_err());
    }

    #[test]
    fn private_addresses_are_the_operators_business() {
        // Unlike a caller-supplied URL, a declared one was chosen deliberately.
        // Serving a site on a private network is a normal thing to want.
        assert!(parse_site("local=http://127.0.0.1:8931/app").is_ok());
        assert!(parse_site("internal=http://10.0.0.5/").is_ok());
    }
}
