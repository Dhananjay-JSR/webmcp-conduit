// conduit host prelude — runs before the vendored platform layer.
//
// Only the things that are genuinely *host* decisions live here. The web
// platform proper (streams, URL, encoding) comes from vendored, spec-tracking
// implementations in vendor/platform.js; writing those by hand is the same
// mistake as writing a DOM by hand, one layer down.
//
// What stays:
//   * timers, because they run on a virtual clock rather than real time
//   * console, because it bridges to the host's tracing
//   * diagnostics, which are ours
//   * a few Node globals happy-dom reaches for, stubbed because a WebMCP
//     harvest never needs a filesystem or a subprocess
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

  // --------------------------------------------------- task scheduling
  // MessageChannel is how React's scheduler yields between units of work; it
  // reaches for this before falling back to setTimeout. happy-dom provides
  // MessagePort but not the channel that pairs two of them.
  //
  // This belongs here rather than in a vendored package because it is pure
  // scheduling, and scheduling is already host-owned: delivery goes through
  // the same virtual clock as the timers above, so a page that yields a
  // thousand times costs no wall time.
  if (typeof globalThis.MessageChannel === "undefined") {
    function Port() {
      this.onmessage = null;
      this._peer = null;
      this._listeners = [];
    }
    Port.prototype.addEventListener = function (type, fn) {
      if (type === "message" && fn) this._listeners.push(fn);
    };
    Port.prototype.removeEventListener = function (type, fn) {
      this._listeners = this._listeners.filter(function (f) { return f !== fn; });
    };
    Port.prototype.postMessage = function (data) {
      var peer = this._peer;
      if (!peer) return;
      globalThis.setTimeout(function () {
        var ev = { type: "message", data: data, target: peer, source: null };
        if (typeof peer.onmessage === "function") {
          try { peer.onmessage(ev); } catch (e) { globalThis.__conduit_report_error(e); }
        }
        peer._listeners.slice().forEach(function (f) {
          try { f(ev); } catch (e) { globalThis.__conduit_report_error(e); }
        });
      }, 0);
    };
    Port.prototype.start = function () {};
    Port.prototype.close = function () { this._peer = null; };

    globalThis.MessageChannel = function MessageChannel() {
      this.port1 = new Port();
      this.port2 = new Port();
      this.port1._peer = this.port2;
      this.port2._peer = this.port1;
    };
  }

  // Frameworks schedule low-priority work here and never run it otherwise.
  if (typeof globalThis.requestIdleCallback !== "function") {
    globalThis.requestIdleCallback = function (fn) {
      return globalThis.setTimeout(function () {
        fn({ didTimeout: false, timeRemaining: function () { return 50; } });
      }, 1);
    };
    globalThis.cancelIdleCallback = function (id) { globalThis.clearTimeout(id); };
  }

  // ------------------------------------------------------------- crypto
  // QuickJS has no Web Crypto. A local-first app mints ids with
  // crypto.randomUUID before it can persist anything, so without this it
  // renders a "save issue" and never gets as far as registering its tools.
  //
  // The entropy comes from the host, which is the only place that has any.
  if (typeof globalThis.crypto === "undefined") {
    function fill(arr) {
      var bytes = globalThis.__conduit_random_bytes(arr.length);
      for (var i = 0; i < arr.length; i++) arr[i] = bytes[i];
      return arr;
    }
    globalThis.crypto = {
      getRandomValues: function (arr) {
        if (!arr || typeof arr.length !== "number") {
          throw new TypeError("getRandomValues expects a typed array");
        }
        return fill(arr);
      },
      randomUUID: function () {
        var b = globalThis.__conduit_random_bytes(16);
        // RFC 4122 version 4, variant 1.
        b[6] = (b[6] & 15) | 64;
        b[8] = (b[8] & 63) | 128;
        var hex = [];
        for (var i = 0; i < 16; i++) {
          hex.push((b[i] + 256).toString(16).slice(1));
        }
        return (
          hex.slice(0, 4).join("") + "-" + hex.slice(4, 6).join("") + "-" +
          hex.slice(6, 8).join("") + "-" + hex.slice(8, 10).join("") + "-" +
          hex.slice(10, 16).join("")
        );
      },
      subtle: {},
    };
  }

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

  // ------------------------------------------------- engine capability gaps
  // Not web APIs — these are places where QuickJS predates the JS spec the
  // vendored platform layer was written against. webidl-conversions reads
  // these property descriptors at load time and dies on a missing one, so
  // they have to exist before platform.js evaluates.
  //
  // Each descriptor is checked on its own. QuickJS *does* have
  // SharedArrayBuffer, just not the resizable-buffer additions to it, so
  // gating on whether the constructor exists skips the very properties that
  // are missing.
  function ensureGetter(obj, name, get) {
    if (!obj) return;
    try {
      if (!Object.getOwnPropertyDescriptor(obj, name)) {
        Object.defineProperty(obj, name, { get: get, configurable: true });
      }
    } catch (e) { /* frozen prototype; nothing to do */ }
  }

  // QuickJS has no weak references. A strong-reference stand-in is
  // functionally correct — it only forgoes the collection behaviour, and a
  // harvest is short-lived enough that nothing depends on that.
  if (typeof globalThis.WeakRef === "undefined") {
    globalThis.WeakRef = function WeakRef(target) { this._target = target; };
    globalThis.WeakRef.prototype.deref = function () { return this._target; };
  }
  if (typeof globalThis.FinalizationRegistry === "undefined") {
    globalThis.FinalizationRegistry = function FinalizationRegistry() {};
    globalThis.FinalizationRegistry.prototype.register = function () {};
    globalThis.FinalizationRegistry.prototype.unregister = function () { return false; };
  }

  if (typeof globalThis.SharedArrayBuffer === "undefined") {
    globalThis.SharedArrayBuffer = function SharedArrayBuffer() {
      throw new Error("SharedArrayBuffer is not available in conduit");
    };
  }
  ensureGetter(globalThis.SharedArrayBuffer.prototype, "byteLength", function () { return 0; });
  ensureGetter(globalThis.SharedArrayBuffer.prototype, "growable", function () { return false; });
  ensureGetter(globalThis.SharedArrayBuffer.prototype, "maxByteLength", function () { return 0; });
  ensureGetter(ArrayBuffer.prototype, "resizable", function () { return false; });
  ensureGetter(ArrayBuffer.prototype, "maxByteLength", function () { return this.byteLength; });
  ensureGetter(ArrayBuffer.prototype, "detached", function () { return false; });

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
  // happy-dom is bundled for the Node platform, so esbuild leaves `require`
  // calls in place for the builtins it touches. None of them are reached on a
  // harvest path, but the symbol has to exist for the bundle to load.
  if (typeof globalThis.require === "undefined") {
    globalThis.require = function (name) {
      switch (name) {
        case "buffer":
          return { Buffer: globalThis.Buffer };
        case "url":
          return { URL: globalThis.URL, URLSearchParams: globalThis.URLSearchParams };
        case "util":
          return { TextEncoder: globalThis.TextEncoder, TextDecoder: globalThis.TextDecoder };
        case "path":
          return { join: function () { return ""; }, resolve: function () { return ""; } };
        default:
          globalThis.__conduit_note_missing("require('" + name + "')");
          return {};
      }
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
