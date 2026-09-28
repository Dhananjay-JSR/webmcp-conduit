//! Throwaway feasibility probe: can a mature JS DOM library be loaded and
//! driven inside our QuickJS isolate, instead of hand-writing one?
//! Run: cargo run --example domcheck -- /path/to/bundle.js
use rquickjs::{Context, Runtime};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: domcheck <bundle.js>");
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
            globalThis.setImmediate = globalThis.setImmediate || function(f){ return globalThis.setTimeout(f, 0); };
            globalThis.clearImmediate = globalThis.clearImmediate || function(id){ return globalThis.clearTimeout(id); };
            globalThis.queueMicrotask = globalThis.queueMicrotask || function(f){ Promise.resolve().then(f); };
            if (typeof Event === 'undefined') { globalThis.Event = function(t){ this.type = t; }; }
            if (typeof Buffer === 'undefined') {
                globalThis.Buffer = { from: () => ({}), isBuffer: () => false,
                                      alloc: () => ({}), concat: () => ({}), byteLength: () => 0 };
            }
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
                    .and_then(|ex| {
                        ex.message().map(|m| match ex.stack() {
                            Some(st) => match st.lines().map(str::trim).find(|l| !l.is_empty()) {
                                Some(frame) => format!("{m}  |  {frame}"),
                                None => m,
                            },
                            None => m,
                        })
                    })
                    .unwrap_or_else(|| e.to_string());
                println!("EVAL FAILED: {detail}");
                return;
            }
        }

        // Can we actually build a document and query it?
        let checks = [
            ("constructor present", "typeof globalThis.__HappyWindow"),
            ("new URL", "(function(){ try { return new globalThis.URL('https://a.example/b?c=1').href; } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("URL subclassable", "(function(){ try { class X extends globalThis.URL {}; new X('https://a.example/'); return 'ok'; } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("new ReadableStream", "(function(){ try { new globalThis.ReadableStream({start(c){c.close();}}); return 'ok'; } catch(e){ return 'ERR: ' + (e && e.message || e); } })()"),
            ("non-constructible globals", "(function(){ var names=['TextEncoder','TextDecoder','URL','URLSearchParams','ReadableStream','WritableStream','TransformStream','Event','EventTarget','AbortController','AbortSignal','Blob','File','FormData','Headers','Request','Response','MessageChannel','MutationObserver','DOMException','SharedArrayBuffer']; var bad=[]; names.forEach(function(n){ var v=globalThis[n]; if(typeof v==='undefined'){bad.push(n+':missing');return;} try{ new v(); }catch(e){ var m=String(e&&e.message||e); if(/not a constructor/.test(m)) bad.push(n+':NOTCTOR'); } }); return bad.length?bad.join(' '):'all ok'; })()"),
            ("instantiate window", "(function(){ try { var w = new globalThis.__HappyWindow(); globalThis.__w = w; return 'ok'; } catch(e) { return 'ERR: ' + (e && e.message || e) + ' @@ ' + String(e && e.stack || '').split('\\n').slice(0,4).join(' | '); } })()"),
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
        if rt.execute_pending_job().is_err() {
            break;
        }
    }
}
