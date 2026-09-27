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
    });
  } catch (e) {
    throw new Error(
      "constructing happy-dom Window failed: " + String((e && e.message) || e) +
      " [Window is " + typeof Window +
      ", URL is " + typeof globalThis.URL +
      ", url=" + String(globalThis.__CONDUIT_URL__) + "]"
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

  ["window", "self", "top", "parent"].forEach(function (n) {
    try {
      Object.defineProperty(globalThis, n, {
        value: win, writable: true, configurable: true, enumerable: true,
      });
    } catch (e) {}
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
  // e.g. `document.defaultView.history`. happy-dom wires this itself, but
  // assert it rather than discover later that it is undefined.
  if (!doc.defaultView) {
    try { doc.defaultView = win; } catch (e) {}
  }

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
