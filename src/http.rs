//! MCP over HTTP, on the SDK's Streamable HTTP transport.
//!
//! The SDK's service is built for one server at one endpoint. conduit serves
//! *many* pages from one endpoint, chosen per request by `?url=`, and the SDK's
//! service factory takes no arguments — it cannot see the request. So this
//! layer does the one thing the SDK cannot: read the query string, find or
//! start the engine for that page, and hand the request to a Streamable HTTP
//! service bound to it.
//!
//! Everything protocol-shaped below that point — framing, SSE, `Mcp-Session-Id`,
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
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;
// The SDK exposes its Streamable HTTP endpoint as a tower service.
use tower_service::Service as _;

#[derive(Clone)]
pub struct Config {
    pub allow_private: bool,
    pub allow_hosts: Vec<String>,
    pub allow_origins: Vec<String>,
    pub no_scripts: bool,
}

type Services = Arc<Mutex<HashMap<String, StreamableHttpService<Conduit, LocalSessionManager>>>>;

pub async fn serve(addr: SocketAddr, config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let pool = engine::Pool::new();
    pool.start_reaper();
    let services: Services = Arc::new(Mutex::new(HashMap::new()));

    eprintln!("conduit: listening on http://{addr}");
    eprintln!("conduit: POST /mcp/v1/connect?url=<site>&session=<id>");

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
    config: Config,
    peer: SocketAddr,
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    if req.method() == Method::GET && req.uri().path() == "/healthz" {
        return Ok(json_response(
            StatusCode::OK,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}),
        ));
    }

    if req.uri().path() != "/mcp/v1/connect" {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            rpc_error(-32601, format!("no route for {}", req.uri().path())),
        ));
    }

    match connect(req, pool, services, config).await {
        Ok(response) => Ok(response),
        Err(e) => {
            tracing::warn!(target: "conduit", "{peer}: {e}");
            Ok(json_response(
                StatusCode::BAD_REQUEST,
                rpc_error(-32600, e.to_string()),
            ))
        }
    }
}

async fn connect(
    req: Request<hyper::body::Incoming>,
    pool: engine::Pool,
    services: Services,
    config: Config,
) -> Result<Response<BoxBody>> {
    let (target, session_id) = parse_query(req.uri().query().unwrap_or(""))?;
    validate_target(&target, &config)?;
    if let Some(id) = &session_id {
        // Checked here rather than deeper: this is the untrusted edge, and a
        // session id goes on to become a directory name.
        crate::session::validate_id(id)?;
    }

    let key = engine::Pool::key(&target, session_id.as_deref());

    let mut service = {
        let mut services = services.lock().await;
        match services.get(&key) {
            Some(service) => service.clone(),
            None => {
                // The page loads here, before the SDK sees anything, so a page
                // that cannot load is reported as a plain HTTP error rather
                // than as a mysteriously broken MCP handshake.
                let handle = pool
                    .get_or_spawn(&target, session_id.as_deref(), config.no_scripts)
                    .await?;
                let conduit = Conduit::new(handle, &target).await;

                let service = StreamableHttpService::new(
                    // Each MCP session gets its own handler, but all of them
                    // point at the one engine for this page. The page is the
                    // shared thing: two clients on the same conduit session
                    // should see the same state, not two copies of it.
                    move || Ok(conduit.clone()),
                    Arc::new(LocalSessionManager::default()),
                    // The spec requires Origin validation to prevent DNS
                    // rebinding. `enforce_origin_validation` is not optional
                    // here: an empty allowlist otherwise means *no* checking at
                    // all rather than "reject every browser origin", so leaving
                    // it off gives a server that looks locked down and is not.
                    StreamableHttpServerConfig::default()
                        .with_allowed_origins(config.allow_origins.clone())
                        .enforce_origin_validation(),
                );
                services.insert(key, service.clone());
                service
            }
        }
    };

    service
        .call(req)
        .await
        .map_err(|e| anyhow!("streamable http transport: {e}"))
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
/// refuses the addresses a stranger has no business reaching: the loopback
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
    // form; a full defence belongs in the network, and --allow-host is the real
    // answer for a public deployment.
    //
    // Taken from `Host` rather than by parsing `host_str()`: that returns an
    // IPv6 address still wrapped in its URL brackets, `[::1]`, which does not
    // parse as an address — so loopback would pass the check.
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
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| is_private(&IpAddr::V4(v4)))
                    .unwrap_or(false)
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
            allow_origins: Vec::new(),
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
