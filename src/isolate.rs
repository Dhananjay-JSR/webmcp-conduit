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
use serde_json::{json, Value};

/// Host-owned decisions: virtual-clock timers, console bridging, diagnostics.
const HOST_PRE_JS: &str = include_str!("js/host-pre.js");
/// Encoding, as its own bundle because it has to be first: bundlers run every
/// module initializer before the entry body, and whatwg-url constructs a
/// TextEncoder at module scope.
const ENCODING_JS: &str = include_str!("js/vendor/encoding.js");
/// The rest of the web platform QuickJS lacks — streams, URL, structuredClone
/// — from vendored spec-tracking implementations rather than anything written
/// here. Must precede happy-dom, which subclasses URL at load time.
const PLATFORM_JS: &str = include_str!("js/vendor/platform.js");
/// happy-dom, bundled. Vendored rather than built from npm so `cargo install`
/// needs no Node toolchain. See vendor-build/ for how it is produced.
const HAPPY_DOM_JS: &str = include_str!("js/vendor/happy-dom.js");
/// Constructs the Window and hoists it onto globalThis.
const HOST_POST_JS: &str = include_str!("js/host-post.js");
/// XMLHttpRequest, backed by the host — the network boundary is ours.
const HOST_FETCH_JS: &str = include_str!("js/host-fetch.js");
/// Intl. QuickJS ships none at all, and Intl.Segmenter is what a text editor
/// reaches for to find grapheme and word boundaries.
const INTL_JS: &str = include_str!("js/vendor/intl.js");
/// IndexedDB, in memory. A local-first app cannot boot without it.
const STORAGE_JS: &str = include_str!("js/vendor/storage.js");
/// Spec types for fetch: Headers, Request, Response. Loaded after host-post
/// because happy-dom-without-node ships them as empty shells and hoisting puts
/// those on globalThis first.
const FETCH_JS: &str = include_str!("js/vendor/fetch.js");
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
    /// Promise rejections nothing handled. Usually the only trace of an async
    /// bootstrap that gave up part way.
    pub unhandled_rejections: Vec<String>,
    pub scripts_total: usize,
    pub scripts_failed: usize,
    /// Modules the page tried to import that were not in the prefetched graph.
    pub unresolved_modules: Vec<String>,
    /// How many times the page read `document.modelContext`. Zero means the
    /// page never looked, which is the difference between a site that does not
    /// use WebMCP and one that does but never finished booting.
    pub model_context_lookups: usize,
    /// How many times `registerTool` was called, including calls that were
    /// then rejected.
    pub register_calls: usize,
    /// How many times the page called `getTools` or `executeTool` — that is,
    /// used WebMCP as a client rather than providing tools of its own.
    pub consume_calls: usize,
    /// True when the page still had queued work when the engine stopped, so
    /// whatever it was building may simply not have finished.
    pub settle_exhausted: bool,
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


/// Perform one HTTP request on behalf of the page.
///
/// The same origin policy that governs script loading governs this: a page's
/// own backend is reachable, an arbitrary third party is not. A blocked
/// request comes back as a structured error rather than an exception, so the
/// page sees a normal network failure and conduit records why.
fn http_request(
    page_origin: &str,
    method: &str,
    url: &str,
    headers_json: &str,
    body: &str,
) -> String {
    let fail = |msg: String| json!({"error": msg}).to_string();

    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return fail(format!("invalid URL {url}: {e}")),
    };
    if !crate::fetch::origin_allowed(&parsed, page_origin) {
        return fail(format!(
            "cross-origin request blocked: {url} (page origin {page_origin})"
        ));
    }

    let mut req = ureq::request(method, parsed.as_str())
        .timeout(std::time::Duration::from_secs(15));

    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(headers_json) {
        for (k, v) in map {
            if let Some(val) = v.as_str() {
                req = req.set(&k, val);
            }
        }
    }

    let sent = if body.is_empty() {
        req.call()
    } else {
        req.send_string(body)
    };

    tracing::debug!(
        target: "http",
        "{method} {url} -> {}",
        match &sent {
            Ok(r) => format!("{}", r.status()),
            Err(ureq::Error::Status(c, _)) => format!("{c}"),
            Err(e) => format!("error: {e}"),
        }
    );

    // An HTTP error status is a normal response to a page, not a failure.
    let resp = match sent {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return fail(format!("{e}")),
    };

    let status = resp.status();
    let status_text = resp.status_text().to_string();
    let final_url = resp.get_url().to_string();
    let mut headers = serde_json::Map::new();
    for name in resp.headers_names() {
        if let Some(v) = resp.header(&name) {
            headers.insert(name.to_lowercase(), json!(v));
        }
    }
    let text = resp.into_string().unwrap_or_default();

    json!({
        "status": status,
        "statusText": status_text,
        "url": final_url,
        "headers": serde_json::Value::Object(headers),
        "body": text,
    })
    .to_string()
}

// Unhandled promise rejections, captured out of QuickJS itself.
//
// This is the difference between "the page did nothing and said nothing" and
// an actual cause. An async chain that rejects with no handler — a failed
// dynamic import, a framework bootstrap that throws inside a `then` — is
// otherwise completely invisible: no script error, no console output, no
// tools, nothing to act on.
//
// rquickjs 0.6 exposes no safe API for this, so it goes through the raw
// QuickJS binding. Single-threaded by construction, hence thread-local.
thread_local! {
    static REJECTIONS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// # Safety
/// Installed as a QuickJS callback; pointers are supplied by the engine and
/// valid for the duration of the call.
unsafe extern "C" fn on_promise_rejection(
    ctx: *mut rquickjs::qjs::JSContext,
    _promise: rquickjs::qjs::JSValue,
    reason: rquickjs::qjs::JSValue,
    is_handled: std::os::raw::c_int,
    _opaque: *mut std::os::raw::c_void,
) {
    // QuickJS reports a rejection twice: once when it happens, and again if a
    // handler is attached later. Only the unhandled report is interesting.
    if is_handled != 0 {
        return;
    }
    unsafe fn to_string(ctx: *mut rquickjs::qjs::JSContext, v: rquickjs::qjs::JSValue) -> Option<String> {
        let mut len: rquickjs::qjs::size_t = 0;
        let raw = rquickjs::qjs::JS_ToCStringLen2(ctx, &mut len, v, 0);
        if raw.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(raw as *const u8, len as usize);
        let out = String::from_utf8_lossy(bytes).into_owned();
        rquickjs::qjs::JS_FreeCString(ctx, raw);
        Some(out)
    }

    let mut text = to_string(ctx, reason).unwrap_or_else(|| "<unprintable rejection>".into());

    // The message alone rarely identifies the culprit — "not a function" could
    // be anywhere. The first stack frame usually does.
    let key = std::ffi::CString::new("stack").unwrap();
    let stack_val = rquickjs::qjs::JS_GetPropertyStr(ctx, reason, key.as_ptr());
    if let Some(stack) = to_string(ctx, stack_val) {
        if let Some(frame) = stack.lines().map(str::trim).find(|l| !l.is_empty()) {
            text.push_str("  |  ");
            text.push_str(frame);
        }
    }
    rquickjs::qjs::JS_FreeValue(ctx, stack_val);

    REJECTIONS.with(|r| r.borrow_mut().push(text));
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
    origin: String,
}

impl Loader for RecordingLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<JsModule<'js, Declared>> {
        if let Ok(m) = self.inner.load(ctx, name) {
            return Ok(m);
        }

        // Not prefetched. Static scanning cannot see every import — a code-split
        // chunk named only in a runtime manifest, for instance — so fetch it
        // now. QuickJS resolves synchronously and conduit's HTTP client is
        // synchronous too, so this is possible here in a way it would not be
        // in a browser-shaped engine.
        match fetch_module_sync(&self.origin, name) {
            Some(source) => {
                tracing::debug!(target: "http", "lazily fetched module {name}");
                JsModule::declare(ctx.clone(), name, source)
            }
            None => {
                self.misses.borrow_mut().push(name.to_string());
                Err(rquickjs::Error::new_loading(name))
            }
        }
    }
}

/// Fetch one module on demand, under the same origin policy as everything else.
fn fetch_module_sync(page_origin: &str, name: &str) -> Option<String> {
    let url = url::Url::parse(name).ok()?;
    if !crate::fetch::origin_allowed(&url, page_origin) {
        return None;
    }
    if url.scheme() == "file" {
        return std::fs::read_to_string(url.to_file_path().ok()?).ok();
    }
    let resp = ureq::get(url.as_str())
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .ok()?;
    resp.into_string().ok()
}

/// Modules the document declares via `<link rel="modulepreload">`.
///
/// These matter more than they look. A framework that resolves module IDs at
/// runtime — React Server Components picking a client component out of a
/// manifest, for instance — imports specifiers that appear nowhere in any
/// source we can scan. The browser is told about them through modulepreload,
/// and so are we.
/// The document's base URL, honouring `<base href>`.
///
/// Every relative URL on the page — scripts, modules, preloads, form actions —
/// resolves against this rather than the document's own address. Ignoring it
/// silently resolves everything to the wrong place.
pub fn document_base(html: &str, url: &url::Url) -> url::Url {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("base[href]") else {
        return url.clone();
    };
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("href"))
        .and_then(|href| url.join(href).ok())
        .unwrap_or_else(|| url.clone())
}

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
        doc_base: &url::Url,
        scripts: Vec<Script>,
        modules: HashMap<String, String>,
    ) -> Result<Self> {
        let rt = Runtime::new().context("creating QuickJS runtime")?;
        // Page scripts are untrusted. Cap memory and stack so a hostile or
        // merely broken page cannot take the process down with it.
        rt.set_memory_limit(128 * 1024 * 1024);
        rt.set_max_stack_size(1024 * 1024);

        let origin = url.origin().ascii_serialization();

        let mut loader = BuiltinLoader::default();
        for (name, source) in &modules {
            loader.add_module(name.clone(), source.clone());
        }
        let misses: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        rt.set_loader(
            UrlResolver { page: doc_base.to_string() },
            RecordingLoader {
                inner: loader,
                misses: Rc::clone(&misses),
                origin: origin.clone(),
            },
        );

        let ctx = Context::full(&rt).context("creating QuickJS context")?;
        let origin = url.origin().ascii_serialization();

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
            // Install the rejection tracker before any page code runs.
            REJECTIONS.with(|r| r.borrow_mut().clear());
            unsafe {
                let raw_ctx = ctx.as_raw().as_ptr();
                let raw_rt = rquickjs::qjs::JS_GetRuntime(raw_ctx);
                rquickjs::qjs::JS_SetHostPromiseRejectionTracker(
                    raw_rt,
                    Some(on_promise_rejection),
                    std::ptr::null_mut(),
                );
            }

            let log = Function::new(ctx.clone(), |line: String| {
                tracing::debug!(target: "page", "{line}");
            })?;
            globals.set("__conduit_log", log)?;

            // Order matters. happy-dom subclasses URL and reads timers at load
            // time, so both the host prelude and the platform layer have to be
            // in place before it evaluates.
            ctx.eval::<(), _>(HOST_PRE_JS)
                .map_err(|e| anyhow!("host-pre.js: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(ENCODING_JS)
                .map_err(|e| anyhow!("encoding.js: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(INTL_JS)
                .map_err(|e| anyhow!("intl.js: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(PLATFORM_JS)
                .map_err(|e| anyhow!("platform.js: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(HAPPY_DOM_JS)
                .map_err(|e| anyhow!("happy-dom: {}", describe_exception(&ctx, &e.to_string())))?;
            // happy-dom's bundle can evaluate without throwing and still fail
            // to export a Window, which shows up much later as an unhelpful
            // "not a constructor" from host-post.js.
            let kind: String = ctx
                .eval::<String, _>("typeof globalThis.__HappyWindow")
                .unwrap_or_else(|_| "unknown".into());
            if kind != "function" {
                let detail: String = ctx
                    .eval::<String, _>(
                        "JSON.stringify(globalThis.__conduit_errors.slice(0,3))",
                    )
                    .unwrap_or_else(|_| "[]".into());
                return Err(anyhow!(
                    "happy-dom loaded but exported no Window (typeof __HappyWindow = {kind}); page errors: {detail}"
                ));
            }
            ctx.eval::<(), _>(HOST_POST_JS)
                .map_err(|e| anyhow!("host-post.js: {}", describe_exception(&ctx, &e.to_string())))?;

            // Network transport. Synchronous on purpose: conduit harvests a
            // page rather than driving a live one, so there is nothing to
            // interleave with, and a blocking call keeps the whole engine
            // single-threaded and free of a second async runtime.
            let page_origin = origin.clone();
            let http = Function::new(
                ctx.clone(),
                move |method: String, url: String, headers_json: String, body: String| -> String {
                    http_request(&page_origin, &method, &url, &headers_json, &body)
                },
            )?;
            ctx.globals().set("__conduit_http", http)?;

            // Entropy is a host capability, like timers and the network. The
            // engine has no RNG of its own, and a page that cannot mint an id
            // cannot save anything — which is how a local-first app fails.
            let random = Function::new(ctx.clone(), |n: usize| -> Vec<u8> {
                let mut buf = vec![0u8; n.min(65536)];
                if getrandom::fill(&mut buf).is_err() {
                    // Never silently hand back zeroes; ids would collide.
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
                    }
                }
                buf
            })?;
            ctx.globals().set("__conduit_random_bytes", random)?;

            // After the Window is hoisted: fake-indexeddb needs DOMException,
            // which arrives with happy-dom.
            ctx.eval::<(), _>(STORAGE_JS)
                .map_err(|e| anyhow!("storage.js: {}", describe_exception(&ctx, &e.to_string())))?;

            ctx.eval::<(), _>(HOST_FETCH_JS)
                .map_err(|e| anyhow!("host-fetch.js: {}", describe_exception(&ctx, &e.to_string())))?;
            ctx.eval::<(), _>(FETCH_JS)
                .map_err(|e| anyhow!("fetch.js: {}", describe_exception(&ctx, &e.to_string())))?;

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
        const ROUNDS: usize = 1_000;
        const TIMER_BUDGET: usize = 1_000;

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

        // Reaching here means the page was still scheduling work when we
        // stopped. That is a materially different failure from a page that
        // finished and registered nothing, and the two are indistinguishable
        // from the outside without saying so.
        self.diagnostics.settle_exhausted = true;
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
        let (lookups, registers, consumes) = self.ctx.with(|ctx| {
            let g = ctx.globals();
            let l = g
                .get::<_, Function>("__conduit_lookups")
                .and_then(|f| f.call::<_, usize>(()))
                .unwrap_or(0);
            let r = g
                .get::<_, Function>("__conduit_register_calls")
                .and_then(|f| f.call::<_, usize>(()))
                .unwrap_or(0);
            let c = g
                .get::<_, Function>("__conduit_consume_calls")
                .and_then(|f| f.call::<_, usize>(()))
                .unwrap_or(0);
            (l, r, c)
        });
        self.diagnostics.model_context_lookups = lookups;
        self.diagnostics.register_calls = registers;
        self.diagnostics.consume_calls = consumes;

        self.diagnostics.unhandled_rejections = REJECTIONS.with(|r| {
            let mut seen: Vec<String> = Vec::new();
            for m in r.borrow().iter() {
                if !seen.contains(m) {
                    seen.push(m.clone());
                }
            }
            seen
        });

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
        Page::load(TODO, &url, &url, scripts, HashMap::new()).expect("page should load")
    }

    fn load_module_fixture() -> Page {
        let url = url::Url::parse("https://todo.example/app").unwrap();
        let (scripts, external) = collect_script_refs(MODULE_TODO, &url);
        assert!(external.is_empty());
        assert!(scripts.iter().any(|s| s.is_module), "fixture must use a module");
        Page::load(MODULE_TODO, &url, &url, scripts, HashMap::new()).expect("page should load")
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
        let page = Page::load(html, &url, &url, scripts, HashMap::new()).unwrap();
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
    fn the_platform_surface_frameworks_expect_is_present() {
        // The point of vendoring happy-dom and the platform layer is that this
        // surface stops being our problem. This test is the tripwire: if a
        // dependency bump silently drops something, a framework stops booting
        // and the symptom is an empty tool list with no error at all.
        let page = load_fixture();
        let required = [
            // DOM
            "Element", "HTMLElement", "Node", "Document", "DocumentFragment",
            "Text", "Comment", "DOMParser", "MutationObserver", "customElements",
            "ShadowRoot",
            // Events
            "Event", "CustomEvent", "EventTarget", "AbortController",
            "AbortSignal", "KeyboardEvent", "MouseEvent",
            // Platform
            "URL", "URLSearchParams", "TextEncoder", "TextDecoder",
            "ReadableStream", "structuredClone", "Blob", "FormData",
            // Network
            "fetch", "Headers", "Request", "Response", "XMLHttpRequest",
            // Host-owned
            "setTimeout", "queueMicrotask", "requestAnimationFrame",
            "performance", "localStorage", "history", "matchMedia",
            "getComputedStyle",
        ];

        let mut missing = Vec::new();
        for name in required {
            let kind = page
                .eval_debug(&format!("typeof globalThis.{name}"))
                .unwrap_or_else(|_| "error".into());
            if kind == "undefined" {
                missing.push(name);
            }
        }
        assert!(missing.is_empty(), "platform surface regressed: {missing:?}");
    }

    #[test]
    fn urls_resolve_against_a_base() {
        // core-js stands in for whatwg-url, which is not constructible under
        // QuickJS. Relative resolution is the part frameworks and routers
        // actually depend on.
        let page = load_fixture();
        assert_eq!(
            page.eval_debug("new URL('./y', 'https://a.example/x/z').href").unwrap(),
            "https://a.example/x/y"
        );
        assert_eq!(
            page.eval_debug("new URL('https://a.example/p?q=1').searchParams.get('q')").unwrap(),
            "1"
        );
    }

    #[test]
    fn fetch_is_real_not_a_shell() {
        // happy-dom-without-node ships fetch, Headers, Request and Response as
        // empty shells — Headers.prototype carries only `constructor`. A page
        // reading a header then dies with "not a function" inside a promise
        // nobody is listening to, which is invisible.
        let page = load_fixture();
        assert_eq!(page.eval_debug("typeof fetch").unwrap(), "function");
        assert_eq!(page.eval_debug("typeof new Headers().get").unwrap(), "function");
        assert_eq!(
            page.eval_debug("new Headers({'x-a': '1'}).get('x-a')").unwrap(),
            "1"
        );
        assert_eq!(page.eval_debug("typeof new Request('https://a.example/').url").unwrap(), "string");
        assert_eq!(page.eval_debug("new Response('hi').status").unwrap(), "200");
    }

    #[test]
    fn cross_origin_requests_are_blocked() {
        // The network boundary carries the same origin policy as script
        // loading: a page's own backend is reachable, an arbitrary third
        // party is not.
        let out = http_request(
            "https://app.example",
            "GET",
            "https://evil.example/exfiltrate",
            "{}",
            "",
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(
            v["error"].as_str().unwrap_or("").contains("blocked"),
            "expected a block, got {out}"
        );
    }

    #[test]
    fn registration_activity_is_counted() {
        // These two counts are what let `probe` say something true instead of
        // listing every possible reason a page came up empty.
        let page = load_fixture();
        assert!(
            page.diagnostics.model_context_lookups > 0,
            "the fixture reads document.modelContext"
        );
        // Four registerTool calls in the fixture, one of them a debug tool
        // that is filtered from the listing but still counted here.
        assert_eq!(page.diagnostics.register_calls, 4);
    }

    #[test]
    fn a_page_that_never_looks_is_distinguishable() {
        let url = url::Url::parse("https://plain.example/").unwrap();
        let html = "<html><body><script>var x = 1;</script></body></html>";
        let (scripts, _) = collect_script_refs(html, &url);
        let page = Page::load(html, &url, &url, scripts, HashMap::new()).unwrap();
        assert_eq!(page.diagnostics.model_context_lookups, 0);
        assert_eq!(page.diagnostics.register_calls, 0);
        assert_eq!(page.diagnostics.scripts_failed, 0);
    }

    #[test]
    fn a_client_page_is_not_a_failed_provider() {
        // Some WebMCP pages are agents: they read another page's tools and
        // invoke them, and never register any of their own. An empty tool
        // list is the correct answer for those, not a failure, so the two
        // cases have to be distinguishable.
        let url = url::Url::parse("https://agent.example/").unwrap();
        let html = r#"<html><body><script>
            document.modelContext.getTools().then(function(){});
        </script></body></html>"#;
        let (scripts, _) = collect_script_refs(html, &url);
        let page = Page::load(html, &url, &url, scripts, HashMap::new()).unwrap();
        assert_eq!(page.diagnostics.register_calls, 0);
        assert!(page.diagnostics.consume_calls > 0, "getTools should count as consumption");
    }

    #[test]
    fn the_host_capabilities_a_real_app_needs_are_present() {
        // Each of these was found by an application failing on it, not by
        // reading a spec list. Entropy, storage and segmentation are the
        // three that stop a local-first app before it registers anything.
        let page = load_fixture();
        assert_eq!(page.eval_debug("typeof crypto.randomUUID").unwrap(), "function");
        assert_eq!(page.eval_debug("typeof indexedDB").unwrap(), "object");
        assert_eq!(page.eval_debug("typeof Intl.Segmenter").unwrap(), "function");
        assert_eq!(page.eval_debug("typeof MessageChannel").unwrap(), "function");

        // A v4 UUID, and two calls must differ.
        let a = page.eval_debug("crypto.randomUUID()").unwrap();
        let b = page.eval_debug("crypto.randomUUID()").unwrap();
        assert_eq!(a.len(), 36, "got {a}");
        assert_eq!(&a[14..15], "4", "version nibble: {a}");
        assert_ne!(a, b, "randomUUID must not repeat");
    }

    #[test]
    fn window_self_and_global_are_one_object() {
        // Pointing window at a separate Window instance creates two global
        // namespaces: a script doing `self.x = 1` writes to one and a module
        // reading bare `x` reads the other. That is how React Server
        // Components lose the payload handed to them by an inline script.
        let page = load_fixture();
        assert_eq!(page.eval_debug("window === globalThis").unwrap(), "true");
        assert_eq!(page.eval_debug("self === globalThis").unwrap(), "true");
        assert_eq!(page.eval_debug("document.defaultView === globalThis").unwrap(), "true");
        assert_eq!(
            page.eval_debug("(function(){ self.__probe = 7; return globalThis.__probe; })()")
                .unwrap(),
            "7"
        );
    }

    #[test]
    fn text_encoding_round_trips_non_ascii() {
        // A naive stub in the node:util shim was being hoisted over the real
        // UTF-8 encoder, silently mangling every non-ASCII character — one
        // byte per code unit, so an em dash came back as \u0014. It broke
        // JSON payloads on any page that is not pure ASCII, which is most of
        // them, and it failed quietly.
        let page = load_fixture();
        let out = page
            .eval_debug(
                "(function(){ var s='Margin — Local-first ✓ café'; \
                 var e=new TextEncoder().encode(s); \
                 return JSON.stringify({b:e.length, ok:new TextDecoder().decode(e)===s}); })()",
            )
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(true), "UTF-8 round trip failed: {out}");
        // 27 characters, 32 bytes in UTF-8. Equal counts would mean latin-1.
        assert_eq!(v["b"], json!(32), "not UTF-8 byte length: {out}");
    }

    #[test]
    fn base_href_redirects_relative_urls() {
        // `<base href>` changes where every relative URL on the page points.
        // Ignoring it resolves scripts, modules and preloads against the
        // document's own address, which is silently the wrong server.
        let url = url::Url::parse("https://mirror.example/copy/page.html").unwrap();
        let html = r#"<html><head><base href="https://origin.example/app/">
            <link rel="modulepreload" href="/assets/a.js">
            <script src="./b.js"></script></head><body></body></html>"#;

        let base = document_base(html, &url);
        assert_eq!(base.as_str(), "https://origin.example/app/");

        let pre = collect_modulepreloads(html, &base);
        assert_eq!(pre[0].as_str(), "https://origin.example/assets/a.js");

        let (_, external) = collect_script_refs(html, &base);
        assert_eq!(external[0].1.as_str(), "https://origin.example/app/b.js");
    }

    #[test]
    fn document_base_defaults_to_the_document_url() {
        let url = url::Url::parse("https://a.example/x/page.html").unwrap();
        assert_eq!(document_base("<html><body></body></html>", &url), url);
    }

    #[test]
    fn react_hydration_registers_tools_from_an_effect() {
        // A real React 19 app, server-rendered and hydrated with
        // hydrateRoot(document, ...), registering from inside useEffect.
        //
        // This is the exact shape that fails on OpenAI's Margin demo, and it
        // passes here — which is what localises that failure to the RSC
        // payload path rather than to hydration, effects, or the scheduler.
        let html = include_str!("../fixtures/react/hydrate-document.html");
        let bundle = include_str!("../fixtures/react/hydrate-document.js");
        let url = url::Url::parse("https://app.example/index.html").unwrap();

        let (mut scripts, external) = collect_script_refs(html, &url);
        assert_eq!(external.len(), 1, "fixture loads one external bundle");
        scripts[external[0].0].source = bundle.to_string();

        let page = Page::load(html, &url, &url, scripts, HashMap::new()).unwrap();
        assert!(
            page.diagnostics.script_errors.is_empty(),
            "unexpected errors: {:?}",
            page.diagnostics.script_errors
        );

        let tools = page.harvest().unwrap();
        assert_eq!(tools.len(), 1, "got {:?}", tools.iter().map(|t| &t.name).collect::<Vec<_>>());
        assert_eq!(tools[0].name, "add-item");

        // The effect ran to completion, not just far enough to register.
        assert_eq!(
            page.eval_debug("document.getElementById('status').textContent").unwrap(),
            "registered"
        );
    }

    #[test]
    fn rsc_client_components_register_tools() {
        // The full React Server Components path: flight rows handed from
        // classic inline scripts to a module, decoded through
        // createFromReadableStream, unwrapped with use(), hydrated into the
        // document — and a client component resolved from the payload by
        // module id registering a tool from an effect.
        //
        // That last step is Margin's exact shape. It works here, which rules
        // RSC out as the reason Margin registers nothing.
        let html = include_str!("../fixtures/rsc/rsc.html");
        let bundle = include_str!("../fixtures/rsc/rsc.js");
        let url = url::Url::parse("https://app.example/index.html").unwrap();

        let (mut scripts, external) = collect_script_refs(html, &url);
        assert_eq!(external.len(), 1, "fixture loads one module bundle");
        scripts[external[0].0].source = bundle.to_string();

        let page = Page::load(html, &url, &url, scripts, HashMap::new()).unwrap();
        assert!(
            page.diagnostics.script_errors.is_empty(),
            "unexpected errors: {:?}",
            page.diagnostics.script_errors
        );

        let mut names: Vec<String> = page.harvest().unwrap().into_iter().map(|t| t.name).collect();
        names.sort();
        assert_eq!(names, vec!["client-ref-tool", "rsc-tool"], "got {names:?}");

        // The client component both mounted and finished its effect.
        assert_eq!(
            page.eval_debug("document.getElementById('widget').textContent").unwrap(),
            "widget registered"
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
