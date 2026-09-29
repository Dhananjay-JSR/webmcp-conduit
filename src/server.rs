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

#[derive(Clone)]
pub struct Conduit {
    engine: engine::Handle,
    /// Captured once, because `get_info` is synchronous in the SDK while the
    /// answer lives behind a channel. It is fixed for the life of the page, so
    /// there is nothing to keep in sync.
    instructions: Arc<str>,
}

impl Conduit {
    pub async fn new(engine: engine::Handle, target: &str) -> Self {
        let instructions = match engine.describe().await {
            Ok(d) => format!(
                "Tools exposed by {target}, discovered via WebMCP and served over MCP \
                 by conduit (engine: {}). Tool names are prefixed with `{}` to identify \
                 their origin site.",
                d.engine, d.prefix
            ),
            Err(_) => format!("Tools exposed by {target}, discovered via WebMCP."),
        };

        Self {
            engine,
            instructions: Arc::from(instructions.as_str()),
        }
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
            .with_instructions(self.instructions.to_string())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let raw = self.engine.list_tools().await.map_err(engine_gone)?;

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
            .engine
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
