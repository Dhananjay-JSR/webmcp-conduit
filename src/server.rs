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
    engine: Arc<tokio::sync::OnceCell<engine::Handle>>,
}

impl Conduit {
    pub fn new(spec: EngineSpec) -> Self {
        Self {
            spec,
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
                match &self.spec.session {
                    // Pooled and persistent: the secret is the identity, and
                    // anyone else presenting it joins the same engine.
                    Some(session) => {
                        self.spec
                            .pool
                            .get_or_spawn(&self.spec.url, Some(session), self.spec.no_scripts)
                            .await
                    }
                    // Private to this connection. Not pooled, so nobody else
                    // can reach it, and no session means nothing is written to
                    // disk.
                    None => {
                        engine::spawn(self.spec.url.to_string(), None, self.spec.no_scripts).await
                    }
                }
            })
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
    }
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
