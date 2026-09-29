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
    pub allow_origins: Vec<String>,
    pub no_scripts: bool,
    /// Run without persistence: no cookies or storage kept between requests.
    pub stateless: bool,
}

/// Paths that are the server's own, so a site may not take them.
const RESERVED: &[&str] = &["healthz"];

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

type Services = Arc<Mutex<HashMap<String, StreamableHttpService<Conduit, LocalSessionManager>>>>;

pub async fn serve(addr: SocketAddr, config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let pool = engine::Pool::new();
    pool.start_reaper();
    let services: Services = Arc::new(Mutex::new(HashMap::new()));
    let config = Arc::new(config);

    eprintln!("conduit: listening on http://{addr}");
    for site in &config.sites {
        eprintln!(
            "conduit:   POST /{}  ->  {}{}",
            site.name,
            site.url,
            if config.stateless {
                String::new()
            } else {
                format!("  [session: {}]", site.name)
            }
        );
    }

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

    match connect(req, site, pool, services, config).await {
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

async fn connect(
    req: Request<hyper::body::Incoming>,
    site: Site,
    pool: engine::Pool,
    services: Services,
    config: Arc<Config>,
) -> Result<Response<BoxBody>> {
    // The mount is the session. Nothing in the request chooses it.
    let session_id = (!config.stateless).then(|| site.name.clone());

    let mut service = {
        let mut services = services.lock().await;
        match services.get(&site.name) {
            Some(service) => service.clone(),
            None => {
                // The page loads here, before the SDK sees anything, so a page
                // that cannot load is reported as a plain HTTP error rather
                // than as a mysteriously broken MCP handshake.
                let handle = pool
                    .get_or_spawn(&site.url, session_id.as_deref(), config.no_scripts)
                    .await?;
                let conduit = Conduit::new(handle, &site.url).await;

                let service = StreamableHttpService::new(
                    // Each MCP session gets its own handler, but all of them
                    // point at the one engine for this mount. The page is the
                    // shared thing: two clients on the same mount should see
                    // the same state, not two copies of it.
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
                services.insert(site.name.clone(), service.clone());
                service
            }
        }
    };

    service
        .call(req)
        .await
        .map_err(|e| anyhow!("streamable http transport: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
