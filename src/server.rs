//! The MCP server itself, implemented against the official SDK.
//!
//! Everything protocol-shaped lives here and nowhere else: framing, the
//! handshake, version negotiation, error codes, `Origin` validation, session
//! headers. conduit's job is to know what a page can do; the SDK's job is to
//! speak MCP correctly, and it does that better than a hand-written dispatch
//! loop did — the hand-written one had two `MUST` violations in it.
//!
//! This type is the bridge. It is `Send + Sync`, as the SDK requires, and it
//! owns nothing but a channel to the thread where the real engine lives.

use crate::engine;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use std::sync::Arc;

/// How to get an engine for this connection, once one is wanted.
#[derive(Clone)]
pub struct EngineSpec {
    pub url: Arc<str>,
    /// A caller-supplied session, if they asked for one. Present means the
    /// engine is pooled and its cookies and storage persist across
    /// connections; absent means this connection gets a private engine that
    /// writes nothing and is dropped with it.
    pub session: Option<Arc<str>>,
    pub no_scripts: bool,
    pub pool: engine::Pool,
}

/// One MCP connection's view of a page.
///
/// The SDK builds one of these per MCP session, which is what makes callers
/// isolated from each other without conduit doing anything: two connections to
/// the same URL get two handlers, and — absent a shared `session` secret — two
/// engines.
///
/// The engine is created on first use rather than at construction. The SDK's
/// service factory is synchronous while loading a page is emphatically not,
/// and it means `initialize` answers immediately instead of blocking for the
/// tens of seconds a real page takes to boot.
#[derive(Clone)]
pub struct Conduit {
    spec: EngineSpec,
    /// Which engine this handler owns.
    ///
    /// A shared secret when the caller supplied one, and otherwise a random
    /// value unique to this connection. Either way it is a pool key, which is
    /// what lets the pool bound every engine rather than only the shared ones —
    /// an engine outside the pool is an engine nothing can reclaim, and an MCP
    /// session has no idle timeout, so a client that disconnects without a
    /// DELETE would leak one forever.
    identity: Arc<str>,
    engine: Arc<tokio::sync::OnceCell<engine::Handle>>,
}

impl Conduit {
    pub fn new(spec: EngineSpec) -> Self {
        let identity = match &spec.session {
            Some(session) => session.clone(),
            None => Arc::from(random_identity().as_str()),
        };
        Self {
            spec,
            identity,
            engine: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// A handler over an engine that is already running.
    ///
    /// stdio loads its page before serving — there is one page, one client, and
    /// nothing to be lazy about — so the cell starts filled and `engine()` never
    /// takes the loading path.
    pub fn ready(handle: engine::Handle, url: &str, pool: engine::Pool) -> Self {
        let cell = tokio::sync::OnceCell::new();
        cell.set(handle).ok();
        Self {
            identity: Arc::from("stdio"),
            spec: EngineSpec {
                url: Arc::from(url),
                session: None,
                no_scripts: false,
                pool,
            },
            engine: Arc::new(cell),
        }
    }

    /// The engine for this connection, loading the page if this is the first
    /// request that needs it.
    ///
    /// `get_or_try_init` rather than `get_or_init`: a page that fails to load
    /// must not poison the cell, because the failure is usually the site being
    /// briefly unreachable and the next request deserves a fresh attempt.
    async fn engine(&self) -> Result<&engine::Handle, ErrorData> {
        self.engine
            .get_or_try_init(|| async {
                self.spec
                    .pool
                    .get_or_spawn(
                        &self.spec.url,
                        &self.identity,
                        // Only a caller-supplied session persists. A private
                        // engine writes nothing: it exists for one connection
                        // and there is nobody to restore it for.
                        self.spec.session.as_deref(),
                        self.spec.no_scripts,
                    )
                    .await
            })
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
    }
}

/// A value no other connection will hold. 128 bits from the OS, because a
/// collision would put two callers on one page — the thing this exists to
/// prevent.
fn random_identity() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Falling back to a counter rather than a constant: a predictable
        // identity is survivable, a shared one is not.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        bytes[..8].copy_from_slice(&n.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An engine that has stopped is an internal failure, not a bad request: the
/// caller did nothing wrong and retrying the same call may well work.
fn engine_gone(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

impl ServerHandler for Conduit {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "webmcp-conduit",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Tools exposed by {}, discovered via WebMCP and served over MCP by \
                 conduit. Tool names are prefixed to identify their origin site.",
                self.spec.url
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let raw = self
            .engine()
            .await?
            .list_tools()
            .await
            .map_err(engine_gone)?;

        // The tool layer already emits MCP's wire shape, so this is a parse
        // rather than a translation. A tool that will not deserialise is
        // dropped with a warning instead of failing the whole listing: one
        // malformed registration should not hide every other tool on the page.
        let tools = raw
            .into_iter()
            .filter_map(|value| match serde_json::from_value::<Tool>(value.clone()) {
                Ok(tool) => Some(tool),
                Err(e) => {
                    tracing::warn!(target: "conduit", "skipping a tool that is not valid MCP: {e} ({value})");
                    None
                }
            })
            .collect();

        Ok(ListToolsResult {
            tools,
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::json!({}));

        let outcome = self
            .engine()
            .await?
            .call_tool(&request.name, arguments)
            .await
            .map_err(engine_gone)?;

        let content = vec![ContentBlock::text(outcome.text)];
        let result = if outcome.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };

        Ok(result.into())
    }
}
