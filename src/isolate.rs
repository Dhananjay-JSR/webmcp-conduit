//! L1 — execute the page's own JavaScript in a QuickJS isolate against the
//! micro-DOM, and harvest whatever it registers with `document.modelContext`.
//!
//! This is the tier that makes browser-free WebMCP possible at all: imperative
//! tools are registered by *running code*, and `execute` is a closure over page
//! state, so there is nothing in the HTML to parse. See `declarative.rs` for
//! the tier that genuinely needs no execution.
//!
//! What this is not: a browser. There is no layout, no rendering, no network
//! by default. Rather than pretend otherwise, every unimplemented platform API
//! a page reaches for is recorded and reported by `probe`, so the gap between
//! this and a real browser is measured instead of guessed.

use crate::tool::{Annotations, WebTool};
use anyhow::{anyhow, Context as _, Result};
use rquickjs::loader::{BuiltinLoader, Resolver};
use rquickjs::function::Rest;
use rquickjs::{Context, Ctx, Function, Module, Runtime, Type, Value as JsValue};
use std::collections::HashMap;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::{json, Map, Value};

const DOM_JS: &str = include_str!("js/dom.js");
const SHIM_JS: &str = include_str!("js/shim.js");

/// What happened while running a page, beyond the tools themselves.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Diagnostics {
    /// Platform APIs the page used that the micro-DOM does not implement.
    /// This is the roadmap for what to build next.
    pub missing_apis: Vec<String>,
    /// Errors thrown by page scripts, one entry per failure.
    pub script_errors: Vec<String>,
    pub scripts_total: usize,
    pub scripts_failed: usize,
}

pub struct Page {
    ctx: Context,
    _rt: Runtime,
    pub diagnostics: Diagnostics,
    pub origin: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarvestedTool {
    name: String,
    title: Option<String>,
    description: String,
    input_schema: Option<Value>,
    annotations: HarvestedAnnotations,
    origin: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct HarvestedAnnotations {
    read_only_hint: bool,
    untrusted_content_hint: bool,
    consequential_hint: bool,
    debugging: bool,
}

/// A script found on the page, already resolved to source text.
pub struct Script {
    pub source: String,
    /// Human-readable, for diagnostics.
    pub label: String,
    /// Absolute URL identifying this script. Module imports resolve against
    /// it, so inline modules get the page URL plus a fragment rather than a
    /// bare "inline#0", which is not resolvable.
    pub name: String,
    pub is_module: bool,
}

/// Pull scripts out of a document in order. External scripts are returned as
/// URLs for the caller to fetch — this function does no I/O.
pub fn collect_script_refs(html: &str, base: &url::Url) -> (Vec<Script>, Vec<(usize, url::Url)>) {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("script").unwrap();

    let mut scripts = Vec::new();
    let mut external = Vec::new();

    for el in doc.select(&sel) {
        let e = el.value();
        let ty = e.attr("type").unwrap_or("text/javascript").to_lowercase();
        let is_module = ty == "module";
        // Skip JSON-LD, importmaps, templates — anything that is not script.
        if !is_module
            && !matches!(
                ty.as_str(),
                "" | "text/javascript" | "application/javascript" | "module"
            )
        {
            continue;
        }

        match e.attr("src") {
            Some(src) => {
                if let Ok(u) = base.join(src) {
                    external.push((scripts.len(), u.clone()));
                    scripts.push(Script {
                        source: String::new(),
                        label: u.to_string(),
                        name: u.to_string(),
                        is_module,
                    });
                }
            }
            None => {
                let source: String = el.text().collect();
                if !source.trim().is_empty() {
                    let n = scripts.len();
                    scripts.push(Script {
                        source,
                        label: format!("inline#{n}"),
                        name: format!("{base}#inline{n}"),
                        is_module,
                    });
                }
            }
        }
    }

    (scripts, external)
}

/// Serialize the parsed document into the compact tree `dom.js` rebuilds.
pub fn serialize_dom(html: &str) -> Value {
    let doc = Html::parse_document(html);
    let root = doc
        .tree
        .root()
        .children()
        .find(|n| n.value().is_element())
        .map(|n| node_to_json(n));
    root.unwrap_or_else(|| json!({"t": "element", "n": "html", "a": {}, "c": []}))
}

fn node_to_json(node: ego_tree::NodeRef<scraper::node::Node>) -> Value {
    match node.value() {
        scraper::node::Node::Element(el) => {
            let mut attrs = Map::new();
            for (k, v) in el.attrs() {
                attrs.insert(k.to_string(), Value::String(v.to_string()));
            }
            let children: Vec<Value> = node
                .children()
                .filter(|c| c.value().is_element() || c.value().is_text())
                .map(node_to_json)
                .collect();
            json!({
                "t": "element",
                "n": el.name(),
                "a": Value::Object(attrs),
                "c": children
            })
        }
        scraper::node::Node::Text(t) => json!({"t": "text", "v": t.to_string()}),
        _ => json!({"t": "text", "v": ""}),
    }
}

/// Resolves module specifiers the way the web does: as URLs relative to the
/// importing module. QuickJS resolves synchronously, so every module in the
/// graph must already be in the loader's map by this point.
struct UrlResolver;

impl Resolver for UrlResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, base: &str, name: &str) -> rquickjs::Result<String> {
        if let Ok(absolute) = url::Url::parse(name) {
            return Ok(absolute.to_string());
        }
        match url::Url::parse(base).and_then(|b| b.join(name)) {
            Ok(u) => Ok(u.to_string()),
            Err(_) => Ok(name.to_string()),
        }
    }
}


/// Describe whatever a page threw. JS permits throwing any value, and when it
/// is not an `Error` rquickjs surfaces its own conversion failure instead of
/// the page's problem — which is useless for diagnosis.
fn describe_exception(ctx: &Ctx<'_>, fallback: &str) -> String {
    let caught = ctx.catch();
    if let Some(ex) = caught.as_exception() {
        let msg = ex
            .message()
            .or_else(|| ex.to_string().into())
            .unwrap_or_else(|| fallback.to_string());
        return match ex.line() {
            Some(line) => format!("{msg} (line {line})"),
            None => msg,
        };
    }
    // Not an Error object: coerce through JS rather than through Rust.
    if let Ok(global) = ctx.globals().get::<_, rquickjs::Object>("JSON") {
        if let Ok(stringify) = global.get::<_, Function>("stringify") {
            if let Ok(s) = stringify.call::<_, String>((caught.clone(),)) {
                if !s.is_empty() && s != "null" {
                    return s;
                }
            }
        }
    }
    fallback.to_string()
}

impl Page {
    /// Build an isolate, install the micro-DOM and the WebMCP shim, then run
    /// the page's scripts in document order.
    ///
    /// `modules` is the pre-fetched import graph; QuickJS resolves imports
    /// synchronously so nothing can be fetched once evaluation starts.
    pub fn load(
        html: &str,
        url: &url::Url,
        scripts: Vec<Script>,
        modules: HashMap<String, String>,
    ) -> Result<Self> {
        let rt = Runtime::new().context("creating QuickJS runtime")?;
        // Page scripts are untrusted. Cap memory and stack so a hostile or
        // merely broken page cannot take the process down with it.
        rt.set_memory_limit(64 * 1024 * 1024);
        rt.set_max_stack_size(1024 * 1024);

        let mut loader = BuiltinLoader::default();
        for (name, source) in &modules {
            loader.add_module(name.clone(), source.clone());
        }
        rt.set_loader(UrlResolver, loader);

        let ctx = Context::full(&rt).context("creating QuickJS context")?;
        let origin = url.origin().ascii_serialization();
        let dom = serialize_dom(html);

        let diagnostics = Diagnostics {
            scripts_total: scripts.len(),
            ..Default::default()
        };

        ctx.with(|ctx| -> Result<()> {
            let globals = ctx.globals();
            globals.set("__CONDUIT_DOM__", json_to_js(&ctx, &dom)?)?;
            globals.set("__CONDUIT_URL__", url.as_str())?;

            // A console that actually reaches the operator's stderr, rather
            // than a black hole — page logs are useful when diagnosing a miss.
            //
            // It must accept *any* arguments. A binding typed to `String`
            // makes `console.error(someObject)` throw a conversion error into
            // the page, which is how instrumentation ends up breaking the very
            // render it was meant to observe.
            let log = Function::new(ctx.clone(), |args: Rest<JsValue<'_>>| {
                let line = args
                    .iter()
                    .map(|v| match v.type_of() {
                        Type::String => v.as_string().and_then(|s| s.to_string().ok()),
                        _ => None,
                    }
                    .unwrap_or_else(|| format!("{:?}", v.type_of())))
                    .collect::<Vec<_>>()
                    .join(" ");
                tracing::debug!(target: "page", "{line}");
            })?;
            let console = rquickjs::Object::new(ctx.clone())?;
            for m in ["log", "info", "warn", "error", "debug"] {
                console.set(m, log.clone())?;
            }
            globals.set("console", console)?;

            ctx.eval::<(), _>(DOM_JS).map_err(|e| anyhow!("dom.js: {e}"))?;
            ctx.eval::<(), _>(SHIM_JS).map_err(|e| anyhow!("shim.js: {e}"))?;
            Ok(())
        })?;

        let mut page = Page {
            ctx,
            _rt: rt,
            diagnostics,
            origin,
        };

        // Browsers run classic scripts as they are parsed and defer modules
        // until afterwards. Mirroring that order matters: inline classic code
        // frequently sets up globals a module then expects to find.
        for script in scripts.iter().filter(|s| !s.is_module) {
            page.run_script(script);
        }
        page.settle();
        for script in scripts.iter().filter(|s| s.is_module) {
            page.run_script(script);
            page.settle();
        }

        // Many pages defer registration until the document is ready. Nothing
        // fires these events here, so do it once all scripts have run.
        page.ctx.with(|ctx| {
            if let Ok(fire) = ctx.globals().get::<_, Function>("__conduit_fire_ready") {
                let _ = fire.call::<_, ()>(());
            }
        });
        page.settle();
        page.collect_diagnostics();

        Ok(page)
    }

    /// Run one page script. A failure is recorded and execution continues:
    /// one broken analytics bundle should not cost us the whole page's tools.
    fn run_script(&mut self, script: &Script) {
        if script.source.trim().is_empty() {
            return;
        }
        if script.is_module {
            self.run_module(script);
            return;
        }
        let failed = self.ctx.with(|ctx| {
            match ctx.eval::<(), _>(script.source.as_bytes()) {
                Ok(()) => None,
                Err(e) => {
                    // Pull the real exception, with position. "expecting '('"
                    // is useless on its own; "line 42" is actionable.
                    let detail = describe_exception(&ctx, &e.to_string());
                    Some(format!("{}: {}", script.label, detail))
                }
            }
        });
        if let Some(err) = failed {
            self.diagnostics.scripts_failed += 1;
            self.diagnostics.script_errors.push(err);
        }
    }

    /// Evaluate a `<script type="module">`.
    ///
    /// Modules need their own evaluation path: `import ... from` is a syntax
    /// error in classic-script mode, which QuickJS reports as the famously
    /// unhelpful `expecting '('`.
    ///
    /// Evaluation is asynchronous. A declare-time failure (bad syntax, an
    /// unresolvable import) comes back as `Err`, but anything the module body
    /// throws only ever reaches the returned promise. We hand that promise to
    /// a JS-side watcher so a rejection is recorded like any other page error
    /// rather than passing as a silent success.
    fn run_module(&mut self, script: &Script) {
        let name = script.name.clone();
        let label = script.label.clone();

        let failed = self.ctx.with(|ctx| {
            match Module::evaluate(ctx.clone(), name.as_str(), script.source.as_bytes()) {
                Ok(promise) => {
                    if let Ok(watch) = ctx.globals().get::<_, Function>("__conduit_watch_module") {
                        let _ = watch.call::<_, ()>((promise, label.as_str()));
                    }
                    None
                }
                Err(e) => {
                    let detail = describe_exception(&ctx, &e.to_string());
                    Some(format!("{label}: {detail}"))
                }
            }
        });

        if let Some(err) = failed {
            self.diagnostics.scripts_failed += 1;
            self.diagnostics.script_errors.push(err);
        }
    }

    /// Run queued microtasks (promise callbacks) to completion, bounded so a
    /// page that schedules work forever cannot hang the harvest.
    fn drain_jobs(&mut self) {
        const MAX_JOBS: usize = 100_000;
        let mut n = 0;
        while self._rt.is_job_pending() && n < MAX_JOBS {
            if self._rt.execute_pending_job().is_err() {
                break;
            }
            n += 1;
        }
    }

    /// Settle the page: alternate between microtasks and due timers until
    /// neither has anything left.
    ///
    /// These feed each other — a timer callback queues promises, a promise
    /// callback schedules more timers — so draining either one alone leaves
    /// work stranded. React in particular will not render until its scheduler
    /// gets a turn through the timer queue.
    fn settle(&mut self) {
        const ROUNDS: usize = 200;
        const TIMER_BUDGET: usize = 500;

        for _ in 0..ROUNDS {
            self.drain_jobs();
            let ran: usize = self.ctx.with(|ctx| {
                ctx.globals()
                    .get::<_, Function>("__conduit_run_timers")
                    .and_then(|f| f.call::<_, usize>((TIMER_BUDGET,)))
                    .unwrap_or(0)
            });
            if ran == 0 && !self._rt.is_job_pending() {
                return;
            }
        }
    }

    fn collect_diagnostics(&mut self) {
        let (missing, errors) = self.ctx.with(|ctx| {
            let missing: Vec<String> = ctx
                .globals()
                .get::<_, Function>("__conduit_missing")
                .and_then(|f| f.call::<_, Vec<String>>(()))
                .unwrap_or_default();
            let errors: Vec<String> = ctx
                .globals()
                .get::<_, Vec<String>>("__conduit_errors")
                .unwrap_or_default();
            (missing, errors)
        });
        self.diagnostics.missing_apis = missing;
        self.diagnostics.script_errors.extend(errors);
    }

    /// Evaluate an arbitrary expression for diagnostics. The result is
    /// stringified on the JS side so any value type can come back.
    pub fn eval_debug(&self, expr: &str) -> Result<String> {
        let wrapped = format!(
            "(function(){{ try {{ var v = ({expr}); \
             return typeof v === 'string' ? v : JSON.stringify(v); \
             }} catch (e) {{ return 'ERROR: ' + String((e && e.message) || e); }} }})()"
        );
        self.ctx.with(|ctx| {
            ctx.eval::<String, _>(wrapped.as_bytes())
                .map_err(|e| anyhow!("{}", describe_exception(&ctx, &e.to_string())))
        })
    }

    /// Read back everything the page registered.
    pub fn harvest(&self) -> Result<Vec<WebTool>> {
        let raw: String = self.ctx.with(|ctx| {
            ctx.globals()
                .get::<_, Function>("__conduit_harvest")
                .and_then(|f| f.call::<_, String>(()))
                .map_err(|e| anyhow!("harvest failed: {e}"))
        })?;

        let harvested: Vec<HarvestedTool> =
            serde_json::from_str(&raw).context("parsing harvested tools")?;

        Ok(harvested
            .into_iter()
            .filter(|t| !t.annotations.debugging) // debug tools stay hidden
            .map(|t| WebTool {
                name: t.name,
                title: t.title,
                description: t.description,
                input_schema: t.input_schema,
                annotations: Annotations {
                    read_only_hint: t.annotations.read_only_hint,
                    untrusted_content_hint: t.annotations.untrusted_content_hint,
                    consequential_hint: t.annotations.consequential_hint,
                    debugging: t.annotations.debugging,
                },
                origin: t.origin,
                form: None,
            })
            .collect())
    }

    /// Invoke a tool by name. Returns the raw JSON string the WebMCP IDL
    /// specifies `executeTool()` resolves to.
    pub fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        let args_json = serde_json::to_string(args)?;

        self.ctx.with(|ctx| -> Result<()> {
            let run: Function = ctx.globals().get("__conduit_run")?;
            run.call::<_, ()>((name, args_json))
                .map_err(|e| anyhow!("invoking {name}: {e}"))?;
            Ok(())
        })?;

        self.settle();

        let outcome: Value = self.ctx.with(|ctx| {
            let raw: String = ctx
                .globals()
                .get::<_, Function>("JSON")
                .ok()
                .and_then(|_| None::<String>)
                .unwrap_or_default();
            let _ = raw;
            // Stringify on the JS side so we get a plain String across.
            ctx.eval::<String, _>("JSON.stringify(globalThis.__conduit_result)")
                .map_err(|e| anyhow!("reading result: {e}"))
                .and_then(|s| serde_json::from_str(&s).map_err(|e| anyhow!("{e}")))
        })?;

        match outcome.get("status").and_then(|s| s.as_str()) {
            Some("ok") => Ok(outcome
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("null")
                .to_string()),
            Some("error") => Err(anyhow!(
                "{}",
                outcome
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool failed")
            )),
            Some("pending") => Err(anyhow!(
                "tool '{name}' did not settle — it is likely awaiting network or a \
                 timer, which this engine does not provide. Try the browser engine."
            )),
            _ => Err(anyhow!("tool '{name}' produced no result")),
        }
    }
}

/// Move a serde_json value into the JS context via JSON round-trip. Simple and
/// fast enough: the DOM is serialized once at startup.
fn json_to_js<'js>(ctx: &rquickjs::Ctx<'js>, v: &Value) -> Result<rquickjs::Value<'js>> {
    let s = serde_json::to_string(v)?;
    let parse: Function = ctx.globals().get::<_, rquickjs::Object>("JSON")?.get("parse")?;
    parse
        .call::<_, rquickjs::Value>((s,))
        .map_err(|e| anyhow!("injecting DOM: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODO: &str = include_str!("../fixtures/todo.html");

    fn load_fixture() -> Page {
        let url = url::Url::parse("https://todo.example/app").unwrap();
        let (scripts, external) = collect_script_refs(TODO, &url);
        assert!(external.is_empty(), "fixture should have no external scripts");
        Page::load(TODO, &url, scripts, HashMap::new()).expect("page should load")
    }

    #[test]
    fn harvests_imperative_tools_from_page_script() {
        let page = load_fixture();
        assert!(
            page.diagnostics.script_errors.is_empty(),
            "unexpected script errors: {:?}",
            page.diagnostics.script_errors
        );

        let tools = page.harvest().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"add-todo"));
        assert!(names.contains(&"list-todos"));
        assert!(names.contains(&"clear-all"));
        // `debugging` tools are filtered out before the model ever sees them.
        assert!(!names.contains(&"debug-dump"), "debug tool leaked: {names:?}");
    }

    #[test]
    fn annotations_survive_the_round_trip() {
        let page = load_fixture();
        let tools = page.harvest().unwrap();

        let list = tools.iter().find(|t| t.name == "list-todos").unwrap();
        assert!(list.annotations.read_only_hint);

        let clear = tools.iter().find(|t| t.name == "clear-all").unwrap();
        assert!(clear.annotations.consequential_hint);
        assert!(!clear.annotations.read_only_hint);
    }

    #[test]
    fn executes_a_tool_and_returns_a_json_string() {
        let mut page = load_fixture();
        let out = page
            .call("add-todo", &json!({"text": "walk the dog"}))
            .expect("add-todo should succeed");

        // Per the IDL, executeTool resolves to a *stringified* result.
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["added"], json!("walk the dog"));
        assert_eq!(parsed["count"], json!(2));
    }

    #[test]
    fn tool_execution_mutates_real_page_state() {
        let mut page = load_fixture();
        page.call("add-todo", &json!({"text": "walk the dog"})).unwrap();

        // The second tool reads the DOM the first tool wrote to. If state did
        // not persist across calls, this would still only see "buy milk".
        let out = page.call("list-todos", &json!({})).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let items = parsed["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "got {items:?}");
        assert_eq!(items[1], json!("walk the dog"));
    }

    #[test]
    fn unknown_tools_fail_cleanly() {
        let mut page = load_fixture();
        let err = page.call("no-such-tool", &json!({})).unwrap_err();
        assert!(err.to_string().contains("Unknown tool"), "got: {err}");
    }

    #[test]
    fn dom_is_reconstructed_from_parsed_html() {
        let page = load_fixture();
        let title: String = page
            .ctx
            .with(|ctx| ctx.eval::<String, _>("document.title").unwrap());
        assert_eq!(title, "Todo");

        let heading: String = page.ctx.with(|ctx| {
            ctx.eval::<String, _>("document.getElementById('heading').textContent")
                .unwrap()
        });
        assert_eq!(heading, "My Todos");
    }
}
