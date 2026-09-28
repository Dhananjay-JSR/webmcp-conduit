//! A standard MCP server over stdio, fronting whatever the page exposes.
//!
//! The whole point of the project lives here: the client on the other end of
//! this pipe is an ordinary MCP client that knows nothing about WebMCP,
//! browsers, or tokens.

use crate::declarative;
use crate::fetch;
use crate::isolate::{self, Page};
use crate::tool::{self, Engine, WebTool};
use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use url::Url;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

enum Source {
    /// Executes inside the QuickJS page context.
    Page(String),
    /// Executes as a plain HTTP form submission.
    Form(Box<WebTool>),
}

pub struct Session {
    page: Option<Page>,
    routes: HashMap<String, Source>,
    listed: Vec<Value>,
    untrusted: HashMap<String, bool>,
    client: reqwest::Client,
    base: Url,
    jar: crate::cookies::SharedJar,
    pub engine: Engine,
    pub diagnostics: isolate::Diagnostics,
    pub prefix: String,
}

impl Session {
    /// Snapshot the page's storage back into a session. A target with no
    /// JavaScript engine has nothing to save, which is not a failure.
    pub fn snapshot(&mut self) -> Result<Option<String>> {
        match self.page.as_mut() {
            Some(page) => page.export_session().map(Some),
            None => Ok(None),
        }
    }

    /// The key a session files this page's storage under. Matches what was
    /// used to restore it, including the file:// special case.
    /// The cookies this run ended up holding, ready to store.
    pub fn cookies(&self) -> Vec<crate::session::Cookie> {
        self.jar
            .lock()
            .map(|jar| jar.to_stored())
            .unwrap_or_default()
    }

    pub fn origin(&self) -> String {
        if self.base.scheme() == "file" {
            return self.base.as_str().to_string();
        }
        self.base.origin().ascii_serialization()
    }

    pub async fn load(
        target: &str,
        allow_scripts: bool,
        session: Option<&crate::session::Handle>,
    ) -> Result<Self> {
        let client = fetch::client()?;

        // The jar is built before the first request, because the document
        // fetch is exactly where a restored login cookie has to be sent.
        let jar = crate::cookies::shared();
        if let Some(handle) = session {
            *jar.lock().unwrap() = crate::cookies::Jar::from_stored(handle.state().cookies.clone());
        }

        // A local file is a first-class target: it makes the engine testable
        // without a network round trip, and it is how you debug a page.
        let (html, base) = if let Some(path) = target.strip_prefix("file://") {
            (std::fs::read_to_string(path)?, Url::parse(target)?)
        } else if std::path::Path::new(target).exists() {
            let abs = std::fs::canonicalize(target)?;
            let url = Url::from_file_path(&abs)
                .map_err(|_| anyhow!("cannot build a file URL for {target}"))?;
            (std::fs::read_to_string(&abs)?, url)
        } else {
            let url = Url::parse(target)
                .with_context(|| format!("`{target}` is not a URL or an existing file"))?;
            let fetched = fetch::page(&client, &url, Some(&jar)).await?;
            if fetched.tools_disabled {
                return Err(anyhow!(
                    "{url} sends `Permissions-Policy: tools=()`. The site has \
                     explicitly disabled WebMCP; conduit will not override that."
                ));
            }
            (fetched.html, fetched.final_url)
        };

        // `<base href>` redirects every relative URL on the page. The origin
        // policy still keys off where the document actually came from.
        let origin = base.origin().ascii_serialization();
        // Every file:// URL serialises to the opaque origin "null", so keying
        // storage on it would pool every local fixture into one bucket. Real
        // browsers do isolate them; the URL is the closest thing we have.
        let storage_key = if base.scheme() == "file" {
            base.as_str().to_string()
        } else {
            origin.clone()
        };
        let doc_base = isolate::document_base(&html, &base);
        let prefix = tool::host_prefix(base.host_str().unwrap_or("local"));

        // L0 — always runs. No JavaScript involved.
        let declarative_tools = declarative::extract(&html, &origin);

        // L1 — run the page's own scripts, unless asked not to.
        let mut page = None;
        let mut diagnostics = isolate::Diagnostics::default();
        let mut engine = Engine::Static;

        if allow_scripts {
            let (mut scripts, external) = isolate::collect_script_refs(&html, &doc_base);
            for (idx, url) in external {
                match fetch::script(&client, &url, &origin, Some(&jar)).await {
                    Ok(src) => scripts[idx].source = src,
                    Err(e) => diagnostics.script_errors.push(format!("{url}: {e}")),
                }
            }
            // Imports must all be in hand before evaluation: QuickJS resolves
            // module specifiers synchronously and cannot await a fetch.
            //
            // Classic scripts are seeded too, not just modules. Modern
            // bundlers bootstrap the whole application from a classic inline
            // script that does `import("/assets/entry.js")`, so skipping them
            // means the app never loads and nothing ever registers.
            let entries: Vec<(String, String)> = scripts
                .iter()
                .filter(|s| !s.source.trim().is_empty())
                .map(|s| (s.name.clone(), s.source.clone()))
                .collect();
            // `<link rel="modulepreload">` names modules the page will import
            // at runtime through IDs we cannot see in any source.
            let mut preloaded: Vec<(String, String)> = Vec::new();
            for u in isolate::collect_modulepreloads(&html, &doc_base) {
                match fetch::script(&client, &u, &origin, Some(&jar)).await {
                    Ok(src) => preloaded.push((u.to_string(), src)),
                    Err(e) => diagnostics.script_errors.push(format!("{u}: {e}")),
                }
            }

            let (module_graph, module_errors) = if entries.is_empty() && preloaded.is_empty() {
                (Default::default(), Vec::new())
            } else {
                crate::modules::prefetch_graph(&client, entries, preloaded, &origin, Some(&jar))
                    .await
            };
            diagnostics.script_errors.extend(module_errors);

            // A session's storage belongs to the origin that wrote it.
            // Restoring one origin's data into another would be exactly the
            // cross-site leak the same-origin policy exists to prevent.
            let restore = session.and_then(|s| s.storage_for(&storage_key));
            let loaded = Page::load(
                &html,
                &base,
                &doc_base,
                scripts,
                module_graph,
                restore.as_deref(),
                Some(jar.clone()),
            )?;
            diagnostics = isolate::Diagnostics {
                script_errors: {
                    let mut all = diagnostics.script_errors;
                    all.extend(loaded.diagnostics.script_errors.clone());
                    all
                },
                ..loaded.diagnostics.clone()
            };
            engine = Engine::Isolate;
            page = Some(loaded);
        }

        let mut routes = HashMap::new();
        let mut listed = Vec::new();
        let mut untrusted = HashMap::new();

        if let Some(p) = &page {
            for t in p.harvest()? {
                let mcp_name = tool::mcp_tool_name(&prefix, &t.name);
                untrusted.insert(mcp_name.clone(), t.annotations.untrusted_content_hint);
                routes.insert(mcp_name.clone(), Source::Page(t.name.clone()));
                listed.push(t.to_mcp(&prefix));
            }
        }
        for t in declarative_tools {
            let mcp_name = tool::mcp_tool_name(&prefix, &t.name);
            if routes.contains_key(&mcp_name) {
                continue; // an imperative tool of the same name wins
            }
            untrusted.insert(mcp_name.clone(), t.annotations.untrusted_content_hint);
            listed.push(t.to_mcp(&prefix));
            routes.insert(mcp_name, Source::Form(Box::new(t)));
        }

        Ok(Session {
            page,
            routes,
            listed,
            untrusted,
            client,
            base,
            jar,
            engine,
            diagnostics,
            prefix,
        })
    }

    pub fn tools(&self) -> &[Value] {
        &self.listed
    }

    /// Evaluate an expression in the loaded page. Diagnostics only — this is
    /// how you answer "did the framework actually render?" without guessing.
    pub fn eval(&self, expr: &str) -> Result<String> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("no page context (scripts are disabled)"))?;
        page.eval_debug(expr)
    }

    pub async fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        match self.routes.get(name) {
            None => Err(anyhow!("unknown tool: {name}")),
            Some(Source::Page(original)) => {
                let original = original.clone();
                let page = self
                    .page
                    .as_mut()
                    .ok_or_else(|| anyhow!("no page context loaded"))?;
                page.call(&original, args)
            }
            Some(Source::Form(t)) => {
                let t = t.clone();
                self.submit_form(&t, args).await
            }
        }
    }

    /// Execute a declarative tool the way a browser would: submit the form.
    async fn submit_form(&self, t: &WebTool, args: &Value) -> Result<String> {
        let form = t
            .form
            .as_ref()
            .ok_or_else(|| anyhow!("{} has no form action", t.name))?;

        let action = if form.action.is_empty() {
            self.base.clone()
        } else {
            self.base.join(&form.action)?
        };

        let mut fields: Vec<(String, String)> = form.fixed.clone();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                fields.push((k.clone(), s));
            }
        }

        let resp = if form.method == "POST" {
            self.client.post(action).form(&fields).send().await?
        } else {
            self.client.get(action).query(&fields).send().await?
        };

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Keep the payload bounded; a full HTML page is rarely a useful result
        // and can blow out a model's context window.
        let snippet: String = body.chars().take(4000).collect();

        Ok(json!({
            "status": status.as_u16(),
            "body": snippet,
        })
        .to_string())
    }

    fn is_untrusted(&self, name: &str) -> bool {
        self.untrusted.get(name).copied().unwrap_or(false)
    }
}

// ----------------------------------------------------------------- JSON-RPC

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Serve MCP on stdin/stdout until the client closes the pipe.
// Borrowed rather than consumed, so the caller still holds the page once the
// client disconnects — that is when a session gets written back, and tool
// calls are exactly the thing worth persisting.
pub async fn serve(session: &mut Session, target: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                respond(&err(Value::Null, -32700, &format!("parse error: {e}")))?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        // Notifications carry no id and must not be answered.
        let is_notification = req.get("id").is_none();

        let response = match method {
            "initialize" => ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "webmcp-conduit",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": format!(
                        "Tools exposed by {target}, discovered via WebMCP and served over \
                         MCP by conduit (engine: {}). Tool names are prefixed with `{}` to \
                         identify their origin site.",
                        session.engine.as_str(), session.prefix
                    ),
                }),
            ),
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, json!({ "tools": session.tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let untrusted = session.is_untrusted(&name);

                match session.call(&name, &args).await {
                    Ok(raw) => ok(id, tool::to_mcp_result(&raw, untrusted)),
                    // A failing tool is a normal result with isError, not a
                    // protocol error — the model should see it and adapt.
                    Err(e) => ok(id, tool::to_mcp_error(&e.to_string())),
                }
            }
            _ if is_notification => continue,
            _ => err(id, -32601, &format!("method not found: {method}")),
        };

        if !is_notification {
            respond(&response)?;
        }
    }

    Ok(())
}

fn respond(v: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, v)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
