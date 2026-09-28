// conduit host epilogue — runs after happy-dom has defined __HappyWindow.
//
// happy-dom is designed to *be* the global object inside a Node vm context.
// We have no vm, so the Window is constructed here and its properties are
// hoisted onto globalThis, which is where page scripts actually look for
// `document`, `location`, `Element` and the rest.
(function (globalThis) {
  "use strict";

  var Window = globalThis.__HappyWindow;
  if (typeof Window !== "function") {
    throw new Error("happy-dom did not load: __HappyWindow is not a constructor");
  }

  var win;
  try {
    win = new Window({
      url: globalThis.__CONDUIT_URL__ || "about:blank",
      width: 1280,
      height: 800,
      settings: {
        // conduit fetches and evaluates page scripts itself, under its own
        // origin policy. Leaving happy-dom to also load them means a second,
        // unpoliced network path — and its loader reaches for node:https,
        // which does not exist here.
        disableJavaScriptFileLoading: true,
        disableCSSFileLoading: true,
        // No layout engine, so computed style is guesswork either way.
        disableComputedStyleRendering: true,
        // A blocked resource should look like an empty success rather than an
        // error: pages routinely pull analytics and fonts we do not need, and
        // a rejection there would bury the failures that matter.
        handleDisabledFileLoadingAsSuccess: true,
      },
    });
  } catch (e) {
    var frames = String((e && e.stack) || "").split("\n").map(function (l) {
      return l.trim();
    }).filter(Boolean).slice(0, 5).join("  <  ");
    throw new Error(
      "constructing happy-dom Window failed: " + String((e && e.message) || e) +
      (frames ? "  |  " + frames : "")
    );
  }
  globalThis.__conduit_window = win;

  // Methods that genuinely need `this` to be the window. Copying these across
  // unbound gives them globalThis as the receiver, and they break in ways that
  // are hard to trace back here.
  var BIND = {
    addEventListener: 1, removeEventListener: 1, dispatchEvent: 1,
    getComputedStyle: 1, matchMedia: 1, scrollTo: 1, scroll: 1, scrollBy: 1,
    fetch: 1, alert: 1, confirm: 1, prompt: 1, open: 1, close: 1, focus: 1, blur: 1,
    postMessage: 1, requestAnimationFrame: 1, cancelAnimationFrame: 1,
    setTimeout: 1, clearTimeout: 1, setInterval: 1, clearInterval: 1,
    btoa: 1, atob: 1, queueMicrotask: 1,
  };

  // Ours, and not to be replaced by happy-dom's equivalents: the timer queue
  // the host drains, and the diagnostics channel.
  var KEEP = {
    __conduit_errors: 1, __conduit_missing: 1, __conduit_note_missing: 1,
    __conduit_report_error: 1, __conduit_run_timers: 1, __conduit_log: 1,
    __CONDUIT_URL__: 1, __HappyWindow: 1, __conduit_window: 1,
    console: 1, globalThis: 1, global: 1,
    // Scheduling is host-owned and runs on the virtual clock. happy-dom's
    // equivalents would take the page off it.
    setTimeout: 1, clearTimeout: 1, setInterval: 1, clearInterval: 1,
    queueMicrotask: 1, requestAnimationFrame: 1, cancelAnimationFrame: 1,
    requestIdleCallback: 1, cancelIdleCallback: 1,
    MessageChannel: 1, MessagePort: 1,
  };

  function hoist(source) {
    var levels = [];
    var o = source;
    while (o && o !== Object.prototype) {
      levels.push(o);
      o = Object.getPrototypeOf(o);
    }
    levels.forEach(function (level) {
      Object.getOwnPropertyNames(level).forEach(function (k) {
        if (k === "constructor" || KEEP[k]) return;
        if (k === "window" || k === "self" || k === "top" || k === "parent") return;
        try {
          var d = Object.getOwnPropertyDescriptor(level, k);
          if (!d) return;
          if (d.get || d.set) {
            // Accessors must stay bound to the window or they read the wrong
            // object; `document` is one of these.
            Object.defineProperty(globalThis, k, {
              get: d.get ? d.get.bind(source) : undefined,
              set: d.set ? d.set.bind(source) : undefined,
              configurable: true,
            });
          } else {
            var v = source[k];
            globalThis[k] = (typeof v === "function" && BIND[k]) ? v.bind(source) : v;
          }
        } catch (e) { /* read-only globals are expected; skip them */ }
      });
    });
  }

  hoist(win);

  // window, self and globalThis must be the SAME object, as they are in a
  // browser. Pointing them at happy-dom's Window instance creates two global
  // namespaces: a script doing `self.x = 1` writes to one, and another script
  // reading bare `x` reads the other and sees nothing.
  //
  // That is not a corner case. Mixing `window.foo` with bare `foo` is
  // everywhere, and it is how React Server Components hand their payload from
  // an inline script to the module that hydrates it.
  ["window", "self", "top", "parent", "frames"].forEach(function (n) {
    try {
      Object.defineProperty(globalThis, n, {
        value: globalThis, writable: true, configurable: true, enumerable: true,
      });
    } catch (e) {
      globalThis.__conduit_note_missing(
        "cannot alias globalThis." + n + ": " + String((e && e.message) || e)
      );
    }
  });

  // Make sure the essentials landed, whatever the hoist did or did not reach.
  // These may already exist as getter-only accessors from the hoist above, so
  // plain assignment would throw "no setter for property".
  function force(name, value) {
    try {
      Object.defineProperty(globalThis, name, {
        value: value, writable: true, configurable: true, enumerable: true,
      });
    } catch (e) {
      try { globalThis[name] = value; } catch (e2) {}
    }
  }

  var doc = win.document;
  force("document", doc);
  force("location", win.location);
  force("navigator", win.navigator);
  force("history", win.history);
  force("localStorage", win.localStorage);
  force("sessionStorage", win.sessionStorage);

  // Frameworks reach the window through the document rather than the global,
  // e.g. `document.defaultView.history`. It has to be the same object the
  // page sees as `window`, or the two-namespace problem comes back through
  // the document.
  try {
    Object.defineProperty(doc, "defaultView", {
      get: function () { return globalThis; },
      configurable: true,
    });
  } catch (e) {
    try { doc.defaultView = globalThis; } catch (e2) {}
  }

  // ----------------------------------------------------- error capture
  // React routes errors it handles through reportError and the window error
  // event rather than throwing. Without these, a framework that catches an
  // error and renders a fallback looks identical to one that succeeded and
  // simply had nothing to do.
  globalThis.reportError = function (e) {
    globalThis.__conduit_errors.push(
      "reportError: " + String((e && e.message) || e) +
      (e && e.stack ? "  |  " + String(e.stack).split("\n")[0].trim() : "")
    );
  };
  globalThis.addEventListener("error", function (ev) {
    var err = (ev && ev.error) || ev;
    var where = "";
    if (err && err.stack) {
      var frames = String(err.stack).split("\n").map(function (l) {
        return l.trim();
      }).filter(Boolean).slice(0, 4);
      if (frames.length) where = "  |  " + frames.join("  <  ");
    } else if (ev && ev.filename) {
      where = "  |  " + ev.filename + ":" + (ev.lineno || 0) + ":" + (ev.colno || 0);
    }
    globalThis.__conduit_errors.push(
      "window.onerror: " + String((err && err.message) || ev.message || err) + where
    );
  });
  globalThis.addEventListener("unhandledrejection", function (ev) {
    var r = ev && ev.reason;
    globalThis.__conduit_errors.push(
      "unhandledrejection: " + String((r && r.message) || r)
    );
  });

  // ------------------------------------------------------ instrumentation
  // happy-dom implements the DOM but is still not a browser: there is no
  // layout. Record the reach rather than silently returning zeros, so
  // `probe` can say why a geometry-dependent page came up empty.
  try {
    var ElementProto = win.Element && win.Element.prototype;
    if (ElementProto && ElementProto.getBoundingClientRect) {
      var realRect = ElementProto.getBoundingClientRect;
      ElementProto.getBoundingClientRect = function () {
        globalThis.__conduit_note_missing("Element.getBoundingClientRect (layout not simulated)");
        return realRect.apply(this, arguments);
      };
    }
  } catch (e) {}

  // ---------------------------------------------------------------- HTML
  globalThis.__conduit_load_html = function (html) {
    try {
      doc.write(html);
      doc.close();
      return "";
    } catch (e) {
      // Fall back to setting the tree directly; some documents reject write().
      try {
        doc.documentElement.innerHTML = html;
        return "";
      } catch (e2) {
        return String((e2 && e2.message) || e2);
      }
    }
  };

  // Pages routinely defer registration until the document is ready, and
  // nothing fires these on its own here.
  globalThis.__conduit_fire_ready = function () {
    try {
      var Ev = win.Event || globalThis.Event;
      doc.dispatchEvent(new Ev("DOMContentLoaded", { bubbles: true }));
      win.dispatchEvent(new Ev("load"));
      if (typeof win.onload === "function") win.onload(new Ev("load"));
      win.dispatchEvent(new Ev("pageshow"));
    } catch (e) {
      globalThis.__conduit_report_error(e);
    }
  };

  globalThis.__conduit_eval = function (expr) {
    return eval(expr);
  };
})(globalThis);
