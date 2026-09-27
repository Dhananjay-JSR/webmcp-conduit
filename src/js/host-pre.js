// conduit host prelude — runs before happy-dom.
//
// QuickJS is a bare ES engine: no timers, no URL, no TextEncoder. happy-dom
// expects those to exist (it even subclasses URL), so they have to be in place
// first. It also expects a few Node globals, which are stubbed rather than
// implemented — a WebMCP harvest never needs a filesystem or a subprocess.
(function (globalThis) {
  "use strict";

  // ----------------------------------------------------------- diagnostics
  var missing = [];
  var seen = Object.create(null);
  globalThis.__conduit_errors = [];
  globalThis.__conduit_note_missing = function (what) {
    if (seen[what]) return;
    seen[what] = true;
    missing.push(what);
  };
  globalThis.__conduit_missing = function () { return missing.slice(); };
  globalThis.__conduit_report_error = function (e) {
    try {
      var msg = (e && e.message) ? String(e.message) : String(e);
      globalThis.__conduit_errors.push(msg);
    } catch (_) {}
  };

  // ---------------------------------------------------------------- timers
  // QuickJS has no timers; they are a host facility. Without them a
  // framework's scheduler never gets a turn, so a component tree is
  // scheduled, never rendered, and registerTool inside an effect never runs.
  //
  // Time is virtual: the queue is ordered by due time and drained as fast as
  // it can be, so a page that waits 500ms does not cost 500ms of wall clock.
  var seq = 1;
  var queue = [];
  var now = 0;

  function schedule(fn, delay, args, repeating) {
    if (typeof fn !== "function") return 0;
    var id = seq++;
    queue.push({
      id: id, fn: fn, args: args || [],
      due: now + Math.max(0, delay | 0),
      every: repeating ? Math.max(1, delay | 0) : null,
    });
    return id;
  }
  function cancel(id) {
    for (var i = 0; i < queue.length; i++) {
      if (queue[i].id === id) { queue.splice(i, 1); return; }
    }
  }

  globalThis.setTimeout = function (fn, delay) {
    return schedule(fn, delay, Array.prototype.slice.call(arguments, 2), false);
  };
  globalThis.setInterval = function (fn, delay) {
    return schedule(fn, delay, Array.prototype.slice.call(arguments, 2), true);
  };
  globalThis.clearTimeout = cancel;
  globalThis.clearInterval = cancel;
  globalThis.setImmediate = function (fn) {
    return schedule(fn, 0, Array.prototype.slice.call(arguments, 1), false);
  };
  globalThis.clearImmediate = cancel;
  if (typeof globalThis.queueMicrotask !== "function") {
    globalThis.queueMicrotask = function (fn) { Promise.resolve().then(fn); };
  }
  globalThis.requestAnimationFrame = function (fn) {
    return schedule(function () { fn(now); }, 16, [], false);
  };
  globalThis.cancelAnimationFrame = cancel;

  // Drain due timers. The host calls this, bounded, alternating with the
  // microtask queue — each feeds the other, so draining either alone strands
  // work. Returns how many ran so the host knows whether to keep going.
  globalThis.__conduit_run_timers = function (budget) {
    var ran = 0;
    while (ran < budget && queue.length) {
      queue.sort(function (a, b) { return a.due - b.due || a.id - b.id; });
      var t = queue.shift();
      now = Math.max(now, t.due);
      if (t.every !== null) { t.due = now + t.every; queue.push(t); }
      try { t.fn.apply(globalThis, t.args); }
      catch (e) { globalThis.__conduit_report_error(e); }
      ran++;
    }
    return ran;
  };

  // ------------------------------------------------------------- performance
  var t0 = Date.now();
  if (typeof globalThis.performance === "undefined") {
    globalThis.performance = {
      now: function () { return Date.now() - t0; },
      timeOrigin: t0,
      mark: function () {}, measure: function () {},
      getEntries: function () { return []; },
      getEntriesByName: function () { return []; },
      getEntriesByType: function () { return []; },
      clearMarks: function () {}, clearMeasures: function () {},
    };
  }

  // ------------------------------------------------------ text encode/decode
  if (typeof globalThis.TextEncoder === "undefined") {
    globalThis.TextEncoder = function TextEncoder() {};
    globalThis.TextEncoder.prototype.encode = function (s) {
      s = String(s == null ? "" : s);
      var a = new Uint8Array(s.length);
      for (var i = 0; i < s.length; i++) a[i] = s.charCodeAt(i) & 255;
      return a;
    };
    globalThis.TextDecoder = function TextDecoder() {};
    globalThis.TextDecoder.prototype.decode = function (b) {
      if (!b) return "";
      var o = "";
      for (var i = 0; i < b.length; i++) o += String.fromCharCode(b[i]);
      return o;
    };
  }

  // --------------------------------------------------------- URL primitives
  // happy-dom subclasses URL, so a missing global here is not a soft failure —
  // it is "parent class must be constructor" at load time.
  if (typeof globalThis.URLSearchParams === "undefined") {
    function dec(x) {
      try { return decodeURIComponent(String(x).replace(/\+/g, " ")); }
      catch (e) { return String(x); }
    }
    function USP(init) {
      this._p = [];
      if (typeof init === "string") {
        var q = init.charAt(0) === "?" ? init.slice(1) : init;
        if (q) {
          q.split("&").forEach(function (kv) {
            if (!kv) return;
            var i = kv.indexOf("=");
            this._p.push([dec(i < 0 ? kv : kv.slice(0, i)), dec(i < 0 ? "" : kv.slice(i + 1))]);
          }, this);
        }
      } else if (init && typeof init === "object") {
        if (typeof init.forEach === "function" && init._p) {
          init._p.forEach(function (e) { this._p.push([e[0], e[1]]); }, this);
        } else {
          for (var k in init) this._p.push([k, String(init[k])]);
        }
      }
    }
    USP.prototype.get = function (k) {
      for (var i = 0; i < this._p.length; i++) if (this._p[i][0] === k) return this._p[i][1];
      return null;
    };
    USP.prototype.getAll = function (k) {
      return this._p.filter(function (e) { return e[0] === k; }).map(function (e) { return e[1]; });
    };
    USP.prototype.has = function (k) { return this.get(k) !== null; };
    USP.prototype.append = function (k, v) { this._p.push([String(k), String(v)]); };
    USP.prototype.set = function (k, v) { this.delete(k); this._p.push([String(k), String(v)]); };
    USP.prototype.delete = function (k) {
      this._p = this._p.filter(function (e) { return e[0] !== k; });
    };
    USP.prototype.forEach = function (f, t) {
      this._p.forEach(function (e) { f.call(t, e[1], e[0], this); }, this);
    };
    USP.prototype.keys = function () { return this._p.map(function (e) { return e[0]; }); };
    USP.prototype.values = function () { return this._p.map(function (e) { return e[1]; }); };
    USP.prototype.entries = function () { return this._p.map(function (e) { return [e[0], e[1]]; }); };
    USP.prototype.toString = function () {
      return this._p.map(function (e) {
        return encodeURIComponent(e[0]) + "=" + encodeURIComponent(e[1]);
      }).join("&");
    };
    globalThis.URLSearchParams = USP;
  }

  if (typeof globalThis.URL === "undefined") {
    var HIER = /^([a-zA-Z][a-zA-Z0-9+.-]*:)\/\/([^\/?#:]*)(?::(\d+))?([^?#]*)(\?[^#]*)?(#.*)?$/;
    var OPAQUE = /^([a-zA-Z][a-zA-Z0-9+.-]*:)([^#]*)(#.*)?$/;
    function U(input, base) {
      var href = String(input);
      if (base && !HIER.test(href) && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(href)) {
        var b = String(base).replace(/[?#].*$/, "");
        if (href.charAt(0) === "/") {
          var m0 = HIER.exec(b);
          href = m0 ? m0[1] + "//" + m0[2] + (m0[3] ? ":" + m0[3] : "") + href : href;
        } else {
          href = b.replace(/\/[^\/]*$/, "/") + href;
        }
      }
      var m = HIER.exec(href);
      if (m) {
        this.href = href; this.protocol = m[1]; this.hostname = m[2];
        this.port = m[3] || ""; this.host = m[2] + (m[3] ? ":" + m[3] : "");
        this.pathname = m[4] || "/"; this.search = m[5] || ""; this.hash = m[6] || "";
        this.origin = m[1] + "//" + this.host;
      } else {
        // Opaque schemes: about:blank, data:, blob:, javascript:
        var op = OPAQUE.exec(href);
        if (!op) throw new TypeError("Invalid URL: " + input);
        this.href = href; this.protocol = op[1]; this.hostname = "";
        this.port = ""; this.host = ""; this.pathname = op[2] || "";
        this.search = ""; this.hash = op[3] || ""; this.origin = "null";
      }
      this.username = ""; this.password = "";
      this.searchParams = new globalThis.URLSearchParams(this.search);
    }
    U.prototype.toString = function () { return this.href; };
    U.prototype.toJSON = function () { return this.href; };
    globalThis.URL = U;
  }

  // -------------------------------------------------------- Node-ish stubs
  // Referenced by happy-dom but never reached on a harvest path.
  if (typeof globalThis.Buffer === "undefined") {
    globalThis.Buffer = {
      from: function () { return {}; },
      isBuffer: function () { return false; },
      alloc: function () { return {}; },
      concat: function () { return {}; },
      byteLength: function () { return 0; },
    };
  }
  if (typeof globalThis.process === "undefined") {
    globalThis.process = {
      env: {}, argv: [], platform: "linux", version: "v20.0.0",
      nextTick: function (f) { Promise.resolve().then(f); },
      cwd: function () { return "/"; },
    };
  }
  globalThis.global = globalThis;

  // ------------------------------------------------------------- console
  // Frameworks report their real problems through console.error, so the
  // formatting has to survive an Error object — anything less reduces the
  // message that explains the failure to "[object Object]".
  function fmt(v) {
    if (typeof v === "string") return v;
    if (v instanceof Error || (v && typeof v.message === "string" && v.stack)) {
      return (v.name || "Error") + ": " + v.message;
    }
    try { var j = JSON.stringify(v); if (j !== undefined) return j; } catch (_) {}
    try { return String(v); } catch (_) { return "[unprintable]"; }
  }
  globalThis.console = {};
  ["log", "info", "warn", "error", "debug", "trace"].forEach(function (m) {
    globalThis.console[m] = function () {
      if (typeof globalThis.__conduit_log !== "function") return;
      var parts = [];
      for (var i = 0; i < arguments.length; i++) parts.push(fmt(arguments[i]));
      globalThis.__conduit_log(m.toUpperCase() + " " + parts.join(" "));
    };
  });
})(globalThis);
