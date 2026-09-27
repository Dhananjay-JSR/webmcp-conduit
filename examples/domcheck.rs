//! Throwaway feasibility probe: can a mature JS DOM library be loaded and
//! driven inside our QuickJS isolate, instead of hand-writing one?
//! Run: cargo run --example domcheck -- /path/to/bundle.js
use rquickjs::{Context, Runtime};

fn main() {
    let path = std::env::args().nth(1).expect("usage: domcheck <bundle.js>");
    let src = std::fs::read_to_string(&path).expect("read bundle");
    println!("bundle: {} bytes", src.len());

    let rt = Runtime::new().unwrap();
    rt.set_memory_limit(512 * 1024 * 1024);
    rt.set_max_stack_size(8 * 1024 * 1024);
    let ctx = Context::full(&rt).unwrap();

    ctx.with(|ctx| {
        // Minimal stubs for the few Node builtins the bundle references.
        let prelude = r#"
            globalThis.process = { env: {}, argv: [], platform: 'linux',
                                   version: 'v20.0.0', nextTick: function(f){ Promise.resolve().then(f); } };
            globalThis.global = globalThis;
            globalThis.require = function (m) {
                switch (m) {
                    case 'buffer': return { Buffer: { from: function(){ return {}; }, isBuffer: function(){ return false; } } };
                    case 'path': return { join: function(){ return ''; }, resolve: function(){ return ''; } };
                    case 'url': return { URL: globalThis.URL, parse: function(){ return {}; } };
                    case 'http': case 'https': return { request: function(){ throw new Error('no network'); } };
                    default: return {};
                }
            };
            globalThis.setTimeout = globalThis.setTimeout || function(f){ Promise.resolve().then(f); return 0; };
            globalThis.clearTimeout = globalThis.clearTimeout || function(){};
            globalThis.setInterval = globalThis.setInterval || function(){ return 0; };
            globalThis.clearInterval = globalThis.clearInterval || function(){};
            globalThis.queueMicrotask = globalThis.queueMicrotask || function(f){ Promise.resolve().then(f); };
            if (typeof TextEncoder === 'undefined') {
                globalThis.TextEncoder = function(){
                    this.encode = function(s){ s = String(s); var a = new Uint8Array(s.length);
                        for (var i=0;i<s.length;i++) a[i] = s.charCodeAt(i) & 255; return a; };
                };
                globalThis.TextDecoder = function(){
                    this.decode = function(b){ if(!b) return ''; var o=''; 
                        for (var i=0;i<b.length;i++) o += String.fromCharCode(b[i]); return o; };
                };
            }
            if (typeof Event === 'undefined') { globalThis.Event = function(t){ this.type = t; }; }
            if (typeof performance === 'undefined') {
                var __t0 = Date.now();
                globalThis.performance = { now: function(){ return Date.now() - __t0; },
                                           timeOrigin: __t0, mark: function(){}, measure: function(){} };
            }
        "#;
        if let Err(e) = ctx.eval::<(), _>(prelude) {
            println!("PRELUDE FAILED: {e}");
            return;
        }

        match ctx.eval::<(), _>(src.as_bytes()) {
            Ok(()) => println!("PARSE+EVAL: ok"),
            Err(e) => {
                let detail = ctx
                    .catch()
                    .as_exception()
                    .and_then(|ex| ex.message().map(|m| match ex.line() {
                        Some(l) => format!("{m} (line {l})"),
                        None => m,
                    }))
                    .unwrap_or_else(|| e.to_string());
                println!("EVAL FAILED: {detail}");
                return;
            }
        }

        // Can we actually build a document and query it?
        let checks = [
            ("constructor present", "typeof globalThis.__HappyWindow"),
            ("instantiate window", "(function(){ try { var w = new globalThis.__HappyWindow(); globalThis.__w = w; return 'ok'; } catch(e) { return 'ERR: ' + (e && e.message || e); } })()"),
            ("parse html", "(function(){ try { globalThis.__w.document.body.innerHTML = '<ul id=l><li class=i>a</li><li class=i>b</li></ul>'; return 'ok'; } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("descendant selector", "(function(){ try { return String(globalThis.__w.document.querySelectorAll('#l .i').length); } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("createElement+append", "(function(){ try { var d = globalThis.__w.document; var e = d.createElement('li'); e.textContent='c'; d.getElementById('l').appendChild(e); return String(d.querySelectorAll('#l .i, #l li').length); } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("events", "(function(){ try { var d = globalThis.__w.document; var hit='no'; var b=d.createElement('button'); b.addEventListener('click', function(){hit='yes';}); b.click(); return hit; } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("innerHTML read", "(function(){ try { return globalThis.__w.document.getElementById('l').innerHTML.length > 0 ? 'ok' : 'empty'; } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
        ];
        for (label, expr) in checks {
            match ctx.eval::<String, _>(expr) {
                Ok(v) => println!("  {label:24} -> {v}"),
                Err(_) => println!("  {label:24} -> THREW"),
            }
        }
    });

    while rt.is_job_pending() {
        if rt.execute_pending_job().is_err() { break; }
    }
}
