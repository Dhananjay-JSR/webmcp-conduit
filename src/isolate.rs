//! L1 — execute the page's own JavaScript in a QuickJS isolate against the
//! micro-DOM, and harvest whatever it registers with `document.modelContext`.
//!
//! This is the tier that makes browser-free WebMCP possible at all: imperative
//! tools are registered by *running code*, and `execute` is a closure over page
//! state, so there is nothing in the HTML to parse. See `declarative.rs` for
//! the tier that genuinely needs no execution.
//!
//! The DOM is happy-dom, bundled into the binary — writing one by hand is a
//! treadmill where every new site finds a new gap. What remains ours is the
//! glue QuickJS does not provide: timers on a virtual clock, the web globals
//! happy-dom loads against, and the WebMCP shim itself.
//!
//! What this is not: a browser. There is no layout and no rendering. Rather
//! than pretend otherwise, the reach for a geometry API is recorded and
//! reported by `probe`, so the gap is measured instead of guessed.

use crate::tool::{Annotations, WebTool};
use anyhow::{anyhow, Context as _, Result};
use rquickjs::loader::{BuiltinLoader, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::Module as JsModule;
use std::cell::RefCell;
use std::rc::Rc;
use rquickjs::{Context, Ctx, Function, Module, Runtime};
use std::collections::HashMap;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

/// Web globals QuickJS lacks that happy-dom needs at load time.
const HOST_PRE_JS: &str = include_str!("js/host-pre.js");
/// happy-dom, bundled. Vendored rather than built from npm so `cargo install`
/// needs no Node toolchain. See vendor-build/ for how it is produced.
const HAPPY_DOM_JS: &str = include_str!("js/vendor/happy-dom.js");
/// Constructs the Window and hoists it onto globalThis.
const HOST_POST_JS: &str = include_str!("js/host-post.js");
/// document.modelContext, per the W3C IDL.
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
    /// Modules the page tried to import that were not in the prefetched graph.
    pub unresolved_modules: Vec<String>,
}

pub struct Page {
    ctx: Context,
    _rt: Runtime,
    pub diagnostics: Diagnostics,
    /// Specifiers the page asked to import that were not in the prefetched
    /// graph. Populated by RecordingLoader as the page runs.
    module_misses: Rc<RefCell<Vec<String>>>,
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

/// A loader that remembers what it was asked for and could not provide.
///
/// Without this, a module that was never prefetched fails somewhere deep in a
/// promise chain and disappears — the page reports no error and no tools, and
/// there is nothing to act on. A miss recorded here names the exact specifier
/// that was missing, which is the difference between "it did not work" and a
/// one-line fix.
struct RecordingLoader {
    inner: BuiltinLoader,
    misses: Rc<RefCell<Vec<String>>>,
}

impl Loader for RecordingLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<JsModule<'js, Declared>> {
        match self.inner.load(ctx, name) {
            Ok(m) => Ok(m),
            Err(e) => {
                self.misses.borrow_mut().push(name.to_string());
                Err(e)
            }
        }
    }
}

/// Modules the document declares via `<link rel="modulepreload">`.
///
/// These matter more than they look. A framework that resolves module IDs at
/// runtime — React Server Components picking a client component out of a
/// manifest, for instance — imports specifiers that appear nowhere in any
/// source we can scan. The browser is told about them through modulepreload,
/// and so are we.
pub fn collect_modulepreloads(html: &str, base: &url::Url) -> Vec<url::Url> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("link[rel~=modulepreload][href]").unwrap();
    let mut out: Vec<url::Url> = Vec::new();
    for el in doc.select(&sel) {
        if let Some(href) = el.value().attr("href") {
            if let Ok(u) = base.join(href) {
                if !out.contains(&u) {
                    out.push(u);
                }
            }
        }
    }
    out
}

/// Resolves module specifiers the way the web does: as URLs relative to the
/// importing module. QuickJS resolves synchronously, so every module in the
/// graph must already be in the loader's map by this point.
struct UrlResolver {
    /// The document URL, used when the importer has no usable base of its own.
    /// A dynamic `import()` inside a *classic* script has no module identity,
    /// so QuickJS hands us an empty base — and a bare "/assets/app.js" would
    /// otherwise resolve to nothing and miss the loader map entirely.
    page: String,
}

impl Resolver for UrlResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, base: &str, name: &str) -> rquickjs::Result<String> {
        if let Ok(absolute) = url::Url::parse(name) {
            return Ok(absolute.to_string());
        }
        if let Ok(u) = url::Url::parse(base).and_then(|b| b.join(name)) {
            return Ok(u.to_string());
        }
        if let Ok(u) = url::Url::parse(&self.page).and_then(|b| b.join(name)) {
            return Ok(u.to_string());
        }
        Ok(name.to_string())
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
        rt.set_memory_limit(128 * 1024 * 1024);
        rt.set_max_stack_size(1024 * 1024);

        let mut loader = BuiltinLoader::default();
        for (name, source) in &modules {
            loader.add_module(name.clone(), source.clone());
        }
        let misses: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        rt.set_loader(
            UrlResolver { page: url.to_string() },
            RecordingLoader { inner: loader, misses: Rc::clone(&misses) },
        );

        let ctx = Context::full(&rt).context("creating QuickJS context")?;
        let _origin = url.origin().ascii_serialization();

        let diagnostics = Diagnostics {
            scripts_total: scripts.len(),
            ..Default::default()
        };

        ctx.with(|ctx| -> Result<()> {
            let globals = ctx.globals();
            globals.set("__CONDUIT_URL__", url.as_str())?;

            // Rust takes only a finished string; formatting lives in JS, which
            // can tell an Error from a plain object. From here every non-string
            // argument looks alike, and a binding typed to `String` would make
            // `console.error(someObject)` throw into the page — instrumentation
            // breaking the very render it was meant to observe.
            let log = Function::new(ctx.clone(), |line: String| {
                tracing::debug!(target: "page", "{line}");
            })?;
            globals.set("__conduit_log", log)?;

            // Order matters: happy-dom subclasses URL and reads timers at load
            // time, so the prelude has to be in place before it evaluates.
            ctx.eval::<(), _>(HOST_PRE_JS)
                .map_err(|e| anyhow!("host-pre.js: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(HAPPY_DOM_JS)
                .map_err(|e| anyhow!("happy-dom: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(HOST_POST_JS)
                .map_err(|e| anyhow!("host-post.js: {}", describe_exception(&ctx, &e.to_string())))?;

            // Let happy-dom parse the document. It is a real HTML parser, so
            // this is more faithful than any tree we could hand it.
            let load: Function = ctx.globals().get("__conduit_load_html")?;
            let err: String = load
                .call((html,))
                .map_err(|e| anyhow!("parsing document: {}", describe_exception(&ctx, &e.to_string())))?;
            if !err.is_empty() {
                return Err(anyhow!("parsing document: {err}"));
            }

            ctx.eval::<(), _>(SHIM_JS)
                .map_err(|e| anyhow!("shim.js: {}", describe_exception(&ctx, &e.to_string())))?;
            Ok(())
        })?;

        let mut page = Page {
            ctx,
            _rt: rt,
            diagnostics,
            module_misses: misses,
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

        // A module the page reached for that we never fetched is the single
        // most actionable failure there is, so report it as its own class
        // rather than leaving it buried in a link error.
        let misses = self.module_misses.borrow();
        if !misses.is_empty() {
            let mut unique: Vec<&String> = Vec::new();
            for m in misses.iter() {
                if !unique.contains(&m) {
                    unique.push(m);
                }
            }
            self.diagnostics.unresolved_modules =
                unique.into_iter().cloned().collect();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const TODO: &str = include_str!("../fixtures/todo.html");
    const MODULE_TODO: &str = include_str!("../fixtures/module-todo.html");

    fn load_fixture() -> Page {
        let url = url::Url::parse("https://todo.example/app").unwrap();
        let (scripts, external) = collect_script_refs(TODO, &url);
        assert!(external.is_empty(), "fixture should have no external scripts");
        Page::load(TODO, &url, scripts, HashMap::new()).expect("page should load")
    }

    fn load_module_fixture() -> Page {
        let url = url::Url::parse("https://todo.example/app").unwrap();
        let (scripts, external) = collect_script_refs(MODULE_TODO, &url);
        assert!(external.is_empty());
        assert!(scripts.iter().any(|s| s.is_module), "fixture must use a module");
        Page::load(MODULE_TODO, &url, scripts, HashMap::new()).expect("page should load")
    }

    #[test]
    fn runs_module_scripts_and_awaits_deferred_registration() {
        // `import ... from` is a syntax error in classic-script mode, and a
        // module that defers past a microtask and a timer only registers if
        // microtasks and timers are drained together.
        let page = load_module_fixture();
        assert!(
            page.diagnostics.script_errors.is_empty(),
            "unexpected errors: {:?}",
            page.diagnostics.script_errors
        );
        let tools = page.harvest().unwrap();
        assert_eq!(tools.len(), 1, "got {:?}", tools.iter().map(|t| &t.name).collect::<Vec<_>>());
        assert_eq!(tools[0].name, "module-add");
    }

    #[test]
    fn module_side_effects_reach_the_dom() {
        let page = load_module_fixture();
        let text = page.eval_debug("document.getElementById('root').textContent").unwrap();
        assert_eq!(text, "booted");
    }

    #[test]
    fn location_is_fully_populated() {
        // A router reads location.pathname and calls string methods on it.
        // Leaving these undefined kills a render with a TypeError raised far
        // from the actual cause.
        let page = load_fixture();
        for prop in ["pathname", "origin", "protocol", "host", "search", "hash"] {
            let v = page
                .eval_debug(&format!("typeof location.{prop}"))
                .unwrap();
            assert_eq!(v, "string", "location.{prop} should be a string, got {v}");
        }
    }

    #[test]
    fn document_exposes_its_window() {
        // Framework runtimes reach the window through the document, e.g.
        // `document.defaultView.history`. defaultView is happy-dom's Window
        // instance rather than globalThis, which is the correct relationship.
        let page = load_fixture();
        assert_eq!(
            page.eval_debug("document.defaultView === window").unwrap(),
            "true"
        );
        assert_eq!(
            page.eval_debug("typeof document.defaultView.history.pushState").unwrap(),
            "function"
        );
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
    fn unresolved_modules_are_reported_by_name() {
        // A module the page imports but that was never prefetched used to
        // fail somewhere in a promise chain and vanish, leaving no error and
        // no tools. The specifier itself is the actionable part.
        let url = url::Url::parse("https://app.example/index.html").unwrap();
        let html = r#"<html><body><script type="module">
            import "./never-fetched.js";
        </script></body></html>"#;
        let (scripts, _) = collect_script_refs(html, &url);
        let page = Page::load(html, &url, scripts, HashMap::new()).unwrap();
        assert!(
            page.diagnostics
                .unresolved_modules
                .iter()
                .any(|m| m.contains("never-fetched.js")),
            "got {:?}",
            page.diagnostics.unresolved_modules
        );
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
