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
            globalThis.setImmediate = globalThis.setImmediate || function(f){ return globalThis.setTimeout(f, 0); };
            globalThis.clearImmediate = globalThis.clearImmediate || function(id){ return globalThis.clearTimeout(id); };
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
            // QuickJS has no URL/URLSearchParams, and happy-dom subclasses URL.
            if (typeof URLSearchParams === 'undefined') {
                globalThis.URLSearchParams = class URLSearchParams {
                    constructor(init) { this._p = [];
                        if (typeof init === 'string') { var q = init[0]==='?'?init.slice(1):init;
                            if (q) q.split('&').forEach(function(kv){ if(!kv) return; var i=kv.indexOf('=');
                                var k = i<0?kv:kv.slice(0,i), v = i<0?'':kv.slice(i+1);
                                this._p.push([decodeURIComponent(k.replace(/\+/g,' ')), decodeURIComponent(v.replace(/\+/g,' '))]); }, this); }
                        else if (init && typeof init === 'object') { for (var k in init) this._p.push([k, String(init[k])]); } }
                    get(k){ for (var i=0;i<this._p.length;i++) if(this._p[i][0]===k) return this._p[i][1]; return null; }
                    getAll(k){ return this._p.filter(e=>e[0]===k).map(e=>e[1]); }
                    has(k){ return this.get(k) !== null; }
                    append(k,v){ this._p.push([String(k),String(v)]); }
                    set(k,v){ this.delete(k); this._p.push([String(k),String(v)]); }
                    delete(k){ this._p = this._p.filter(e=>e[0]!==k); }
                    forEach(f,t){ this._p.forEach(e=>f.call(t,e[1],e[0],this)); }
                    keys(){ return this._p.map(e=>e[0])[Symbol.iterator](); }
                    values(){ return this._p.map(e=>e[1])[Symbol.iterator](); }
                    entries(){ return this._p.map(e=>[e[0],e[1]])[Symbol.iterator](); }
                    [Symbol.iterator](){ return this.entries(); }
                    toString(){ return this._p.map(e=>encodeURIComponent(e[0])+'='+encodeURIComponent(e[1])).join('&'); }
                };
            }
            if (typeof URL === 'undefined') {
                var RE = /^([a-zA-Z][a-zA-Z0-9+.-]*:)\/\/([^\/?#:]*)(?::(\d+))?([^?#]*)(\?[^#]*)?(#.*)?$/;
                globalThis.URL = class URL {
                    constructor(input, base) {
                        var href = String(input);
                        if (base && !RE.test(href)) {
                            var b = String(base).replace(/[?#].*$/, '');
                            if (href[0] === '/') { var m0 = RE.exec(b); href = m0 ? m0[1]+'//'+m0[2]+(m0[3]?':'+m0[3]:'')+href : href; }
                            else href = b.replace(/\/[^\/]*$/, '/') + href;
                        }
                        var m = RE.exec(href);
                        if (!m) {
                            // Opaque schemes: about:blank, data:, blob:, javascript:
                            var op = /^([a-zA-Z][a-zA-Z0-9+.-]*:)([^#]*)(#.*)?$/.exec(href);
                            if (!op) throw new TypeError('Invalid URL: ' + input);
                            this.href = href; this.protocol = op[1]; this.hostname = '';
                            this.port = ''; this.host = ''; this.pathname = op[2] || '';
                            this.search = ''; this.hash = op[3] || ''; this.origin = 'null';
                            this.username = ''; this.password = '';
                            this.searchParams = new globalThis.URLSearchParams('');
                            return;
                        }
                        this.href = href; this.protocol = m[1]; this.hostname = m[2];
                        this.port = m[3] || ''; this.host = m[2] + (m[3] ? ':'+m[3] : '');
                        this.pathname = m[4] || '/'; this.search = m[5] || ''; this.hash = m[6] || '';
                        this.origin = m[1] + '//' + this.host;
                        this.username = ''; this.password = '';
                        this.searchParams = new globalThis.URLSearchParams(this.search);
                    }
                    toString(){ return this.href; }
                    toJSON(){ return this.href; }
                };
            }
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
