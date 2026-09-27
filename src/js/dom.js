// conduit micro-DOM — enough of the web platform for WebMCP tool registration
// to run, with instrumentation for everything it is missing.
//
// This is deliberately NOT a browser. Its job is to let a page's
// `document.modelContext.registerTool(...)` calls execute, and to record
// precisely which platform APIs a page reached for that we do not provide.
// That miss-list is the roadmap for what to implement next.
(function (globalThis) {
  "use strict";

  var missing = [];
  var seenMissing = Object.create(null);
  function noteMissing(what) {
    if (seenMissing[what]) return;
    seenMissing[what] = true;
    missing.push(what);
  }

  // ---------------------------------------------------------------- Events
  function Event(type, init) {
    init = init || {};
    this.type = String(type);
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.defaultPrevented = false;
    this.target = null;
    this.currentTarget = null;
  }
  Event.prototype.preventDefault = function () {
    if (this.cancelable) this.defaultPrevented = true;
  };
  Event.prototype.stopPropagation = function () {};
  Event.prototype.stopImmediatePropagation = function () {};

  function CustomEvent(type, init) {
    Event.call(this, type, init);
    this.detail = init && "detail" in init ? init.detail : null;
  }
  CustomEvent.prototype = Object.create(Event.prototype);
  CustomEvent.prototype.constructor = CustomEvent;

  function EventTarget() {
    Object.defineProperty(this, "__listeners", {
      value: Object.create(null),
      enumerable: false,
      writable: true,
    });
  }
  EventTarget.prototype.addEventListener = function (type, fn, opts) {
    if (!fn) return;
    var list = this.__listeners[type] || (this.__listeners[type] = []);
    list.push({ fn: fn, once: !!(opts && opts.once) });
    if (opts && opts.signal && typeof opts.signal.addEventListener === "function") {
      var self = this;
      opts.signal.addEventListener("abort", function () {
        self.removeEventListener(type, fn);
      });
    }
  };
  EventTarget.prototype.removeEventListener = function (type, fn) {
    var list = this.__listeners[type];
    if (!list) return;
    for (var i = list.length - 1; i >= 0; i--) {
      if (list[i].fn === fn) list.splice(i, 1);
    }
  };
  EventTarget.prototype.dispatchEvent = function (ev) {
    ev.target = ev.target || this;
    ev.currentTarget = this;
    // on<type> handler property, per IDL attributes like ontoolchange
    var on = this["on" + ev.type];
    if (typeof on === "function") {
      try { on.call(this, ev); } catch (e) { reportError(e); }
    }
    var list = this.__listeners[ev.type];
    if (list) {
      var copy = list.slice();
      for (var i = 0; i < copy.length; i++) {
        try { copy[i].fn.call(this, ev); } catch (e) { reportError(e); }
        if (copy[i].once) this.removeEventListener(ev.type, copy[i].fn);
      }
    }
    return !ev.defaultPrevented;
  };

  // --------------------------------------------------------- AbortSignal
  function AbortSignal() {
    EventTarget.call(this);
    this.aborted = false;
    this.reason = undefined;
    this.onabort = null;
  }
  AbortSignal.prototype = Object.create(EventTarget.prototype);
  AbortSignal.prototype.constructor = AbortSignal;
  AbortSignal.prototype.throwIfAborted = function () {
    if (this.aborted) throw this.reason;
  };

  function AbortController() {
    this.signal = new AbortSignal();
  }
  AbortController.prototype.abort = function (reason) {
    if (this.signal.aborted) return;
    this.signal.aborted = true;
    this.signal.reason =
      reason !== undefined ? reason : new Error("AbortError");
    this.signal.dispatchEvent(new Event("abort"));
  };

  // ----------------------------------------------------------------- DOM
  var ELEMENT_NODE = 1;
  var TEXT_NODE = 3;

  function ClassList(el) {
    this._el = el;
  }
  ClassList.prototype._read = function () {
    var v = this._el.getAttribute("class") || "";
    return v.split(/\s+/).filter(Boolean);
  };
  ClassList.prototype._write = function (arr) {
    this._el.setAttribute("class", arr.join(" "));
  };
  ClassList.prototype.contains = function (c) {
    return this._read().indexOf(c) !== -1;
  };
  ClassList.prototype.add = function () {
    var a = this._read();
    for (var i = 0; i < arguments.length; i++) {
      if (a.indexOf(arguments[i]) === -1) a.push(arguments[i]);
    }
    this._write(a);
  };
  ClassList.prototype.remove = function () {
    var a = this._read();
    for (var i = 0; i < arguments.length; i++) {
      var ix = a.indexOf(arguments[i]);
      if (ix !== -1) a.splice(ix, 1);
    }
    this._write(a);
  };
  ClassList.prototype.toggle = function (c, force) {
    var has = this.contains(c);
    var want = force === undefined ? !has : !!force;
    if (want) this.add(c); else this.remove(c);
    return want;
  };

  function Node(type) {
    EventTarget.call(this);
    this.nodeType = type;
    this.childNodes = [];
    this.parentNode = null;
    this.ownerDocument = null;
  }
  Node.prototype = Object.create(EventTarget.prototype);
  Node.prototype.constructor = Node;

  function TextNode(text) {
    Node.call(this, TEXT_NODE);
    this.nodeValue = String(text);
  }
  TextNode.prototype = Object.create(Node.prototype);
  TextNode.prototype.constructor = TextNode;
  Object.defineProperty(TextNode.prototype, "textContent", {
    get: function () { return this.nodeValue; },
    set: function (v) { this.nodeValue = String(v); },
  });

  function Element(tagName) {
    Node.call(this, ELEMENT_NODE);
    this.tagName = String(tagName).toUpperCase();
    this.attributes = Object.create(null);
    this.style = {};
    this._classList = null;
  }
  Element.prototype = Object.create(Node.prototype);
  Element.prototype.constructor = Element;

  Object.defineProperty(Element.prototype, "nodeName", {
    get: function () { return this.tagName; },
  });
  Object.defineProperty(Element.prototype, "localName", {
    get: function () { return this.tagName.toLowerCase(); },
  });
  Object.defineProperty(Element.prototype, "classList", {
    get: function () {
      if (!this._classList) this._classList = new ClassList(this);
      return this._classList;
    },
  });
  Object.defineProperty(Element.prototype, "className", {
    get: function () { return this.getAttribute("class") || ""; },
    set: function (v) { this.setAttribute("class", v); },
  });
  Object.defineProperty(Element.prototype, "id", {
    get: function () { return this.getAttribute("id") || ""; },
    set: function (v) { this.setAttribute("id", v); },
  });
  Object.defineProperty(Element.prototype, "children", {
    get: function () {
      return this.childNodes.filter(function (n) {
        return n.nodeType === ELEMENT_NODE;
      });
    },
  });
  Object.defineProperty(Element.prototype, "textContent", {
    get: function () {
      var out = "";
      for (var i = 0; i < this.childNodes.length; i++) {
        var c = this.childNodes[i];
        out += c.nodeType === TEXT_NODE ? c.nodeValue : c.textContent;
      }
      return out;
    },
    set: function (v) {
      this.childNodes = [];
      if (v !== "" && v != null) this.appendChild(new TextNode(v));
    },
  });
  Object.defineProperty(Element.prototype, "value", {
    get: function () { return this.getAttribute("value") || ""; },
    set: function (v) { this.setAttribute("value", v); },
  });

  Element.prototype.getAttribute = function (n) {
    var v = this.attributes[String(n).toLowerCase()];
    return v === undefined ? null : v;
  };
  Element.prototype.setAttribute = function (n, v) {
    this.attributes[String(n).toLowerCase()] = String(v);
  };
  Element.prototype.removeAttribute = function (n) {
    delete this.attributes[String(n).toLowerCase()];
  };
  Element.prototype.hasAttribute = function (n) {
    return String(n).toLowerCase() in this.attributes;
  };
  Element.prototype.appendChild = function (child) {
    child.parentNode = this;
    child.ownerDocument = this.ownerDocument;
    this.childNodes.push(child);
    return child;
  };
  Element.prototype.removeChild = function (child) {
    var ix = this.childNodes.indexOf(child);
    if (ix !== -1) this.childNodes.splice(ix, 1);
    child.parentNode = null;
    return child;
  };
  Element.prototype.insertBefore = function (child, ref) {
    var ix = ref ? this.childNodes.indexOf(ref) : -1;
    child.parentNode = this;
    child.ownerDocument = this.ownerDocument;
    if (ix === -1) this.childNodes.push(child);
    else this.childNodes.splice(ix, 0, child);
    return child;
  };
  Element.prototype.contains = function (other) {
    var n = other;
    while (n) { if (n === this) return true; n = n.parentNode; }
    return false;
  };
  // Layout is not simulated. Return zeros and record the reach, so `probe`
  // can tell the user this page depends on geometry we do not have.
  Element.prototype.getBoundingClientRect = function () {
    noteMissing("Element.getBoundingClientRect (layout not simulated)");
    return { x:0, y:0, top:0, left:0, right:0, bottom:0, width:0, height:0 };
  };
  Element.prototype.focus = function () {};
  Element.prototype.blur = function () {};
  Element.prototype.click = function () {
    this.dispatchEvent(new Event("click", { bubbles: true }));
  };

  function walk(node, visit) {
    if (node.nodeType === ELEMENT_NODE) visit(node);
    for (var i = 0; i < node.childNodes.length; i++) walk(node.childNodes[i], visit);
  }

  // A deliberately small selector engine: #id, .class, tag, [attr], [attr=v]
  // and comma lists. Anything fancier is recorded as a miss rather than
  // silently returning nothing.
  // Split "#list .item > a" into compounds plus the combinator that joins
  // each to the one before it, left to right.
  function splitComplex(sel) {
    var parts = [];
    var buf = "";
    var comb = null;
    var i = 0;
    while (i < sel.length) {
      var ch = sel[i];
      if (ch === " " || ch === ">" || ch === "+" || ch === "~") {
        var seen = null;
        while (i < sel.length && /[\s>+~]/.test(sel[i])) {
          if (sel[i] !== " " && sel[i] !== "\t" && sel[i] !== "\n") seen = sel[i];
          i++;
        }
        if (buf) {
          parts.push({ cmp: buf, comb: comb });
          buf = "";
          comb = seen || " ";
        }
        continue;
      }
      buf += ch;
      i++;
    }
    if (buf) parts.push({ cmp: buf, comb: comb });
    return parts;
  }

  function matches(el, sel) {
    sel = sel.trim();
    if (!sel) return false;
    if (sel.indexOf(",") !== -1) {
      return sel.split(",").some(function (s) { return matches(el, s); });
    }
    var parts = splitComplex(sel);
    if (parts.length === 1) return matchesCompound(el, parts[0].cmp);

    // Match right to left: the rightmost compound must match `el`, then walk
    // outward honouring each combinator.
    if (!matchesCompound(el, parts[parts.length - 1].cmp)) return false;
    var node = el;
    for (var pi = parts.length - 1; pi > 0; pi--) {
      var joiner = parts[pi].comb;
      var target = parts[pi - 1].cmp;
      if (joiner === ">") {
        node = node.parentNode;
        if (!node || node.nodeType !== ELEMENT_NODE) return false;
        if (!matchesCompound(node, target)) return false;
      } else if (joiner === " ") {
        var found = false;
        node = node.parentNode;
        while (node && node.nodeType === ELEMENT_NODE) {
          if (matchesCompound(node, target)) { found = true; break; }
          node = node.parentNode;
        }
        if (!found) return false;
      } else {
        noteMissing("CSS sibling combinator '" + joiner + "': " + sel);
        return false;
      }
    }
    return true;
  }

  function matchesCompound(el, sel) {
    var m, rest = sel;
    if (rest === "*") return true;
    var tag = null, id = null, classes = [], attrs = [];
    while (rest.length) {
      if ((m = /^\[([^\]=]+)(?:=["']?([^\]"']*)["']?)?\]/.exec(rest))) {
        attrs.push([m[1], m[2]]); rest = rest.slice(m[0].length);
      } else if ((m = /^#([\w-]+)/.exec(rest))) {
        id = m[1]; rest = rest.slice(m[0].length);
      } else if ((m = /^\.([\w-]+)/.exec(rest))) {
        classes.push(m[1]); rest = rest.slice(m[0].length);
      } else if ((m = /^([\w-]+)/.exec(rest))) {
        tag = m[1]; rest = rest.slice(m[0].length);
      } else {
        noteMissing("CSS selector syntax: " + sel);
        return false;
      }
    }
    if (tag && el.tagName !== tag.toUpperCase()) return false;
    if (id && el.getAttribute("id") !== id) return false;
    for (var i = 0; i < classes.length; i++) {
      if (!el.classList.contains(classes[i])) return false;
    }
    for (var j = 0; j < attrs.length; j++) {
      if (attrs[j][1] === undefined) {
        if (!el.hasAttribute(attrs[j][0])) return false;
      } else if (el.getAttribute(attrs[j][0]) !== attrs[j][1]) return false;
    }
    return true;
  }

  function querySelectorAll(root, sel) {
    var out = [];
    walk(root, function (el) { if (matches(el, sel)) out.push(el); });
    return out;
  }
  Element.prototype.querySelector = function (s) {
    var r = querySelectorAll(this, s);
    return r.length ? r[0] : null;
  };
  Element.prototype.querySelectorAll = function (s) {
    return querySelectorAll(this, s);
  };
  Element.prototype.closest = function (s) {
    var n = this;
    while (n && n.nodeType === ELEMENT_NODE) {
      if (matches(n, s)) return n;
      n = n.parentNode;
    }
    return null;
  };

  function Document() {
    Node.call(this, 9);
    this.documentElement = null;
    this.body = null;
    this.head = null;
    this.title = "";
  }
  Document.prototype = Object.create(Node.prototype);
  Document.prototype.constructor = Document;
  Document.prototype.createElement = function (t) {
    var el = new Element(t);
    el.ownerDocument = this;
    return el;
  };
  Document.prototype.createTextNode = function (t) { return new TextNode(t); };
  Document.prototype.createDocumentFragment = function () {
    var f = new Element("#fragment");
    f.ownerDocument = this;
    return f;
  };
  Document.prototype.querySelector = function (s) {
    return this.documentElement ? this.documentElement.querySelector(s) : null;
  };
  Document.prototype.querySelectorAll = function (s) {
    return this.documentElement ? this.documentElement.querySelectorAll(s) : [];
  };
  Document.prototype.getElementById = function (id) {
    return this.querySelector("#" + id);
  };
  Document.prototype.getElementsByTagName = function (t) {
    return this.querySelectorAll(t);
  };
  Document.prototype.getElementsByClassName = function (c) {
    return this.querySelectorAll("." + c);
  };

  // ------------------------------------------------- build tree from Rust
  function build(doc, spec, parent) {
    var node;
    if (spec.t === "text") {
      node = new TextNode(spec.v);
    } else {
      node = new Element(spec.n);
      if (spec.a) {
        for (var k in spec.a) node.attributes[k.toLowerCase()] = spec.a[k];
      }
    }
    node.ownerDocument = doc;
    node.parentNode = parent || null;
    if (parent) parent.childNodes.push(node);
    if (spec.c) {
      for (var i = 0; i < spec.c.length; i++) build(doc, spec.c[i], node);
    }
    return node;
  }

  var document = new Document();
  var domSpec = globalThis.__CONDUIT_DOM__;
  if (domSpec) {
    document.documentElement = build(document, domSpec, null);
    document.documentElement.parentNode = null;
    document.body = document.querySelector("body");
    document.head = document.querySelector("head");
    var t = document.querySelector("title");
    document.title = t ? t.textContent : "";
  }
  if (!document.body) {
    document.body = document.createElement("body");
    document.body.ownerDocument = document;
  }
  delete globalThis.__CONDUIT_DOM__;

  function reportError(e) {
    try {
      // QuickJS's `stack` omits the message, and the message is the part that
      // says what actually went wrong. Lead with it, keep one frame for place.
      var msg = (e && e.message) ? String(e.message) : String(e);
      var frame = "";
      if (e && e.stack) {
        var first = String(e.stack).split("\n").find(function (l) {
          return l.trim();
        });
        if (first) frame = " | " + first.trim();
      }
      globalThis.__conduit_errors.push(msg + frame);
    } catch (_) {}
  }

  // ------------------------------------------------------------- globals
  globalThis.__conduit_errors = [];
  globalThis.__conduit_missing = function () { return missing.slice(); };
  globalThis.__conduit_note_missing = noteMissing;
  globalThis.__conduit_report_error = reportError;

  globalThis.Event = Event;
  globalThis.CustomEvent = CustomEvent;
  globalThis.EventTarget = EventTarget;
  globalThis.AbortController = AbortController;
  globalThis.AbortSignal = AbortSignal;
  globalThis.Node = Node;
  globalThis.Element = Element;
  globalThis.HTMLElement = Element;
  globalThis.Document = Document;
  globalThis.Text = TextNode;
  globalThis.CharacterData = TextNode;
  globalThis.DocumentFragment = Element;
  globalThis.Comment = TextNode;

  // Frameworks branch on `x instanceof HTMLIFrameElement` and friends. A
  // missing constructor is not a quiet `false` — it is a TypeError, "invalid
  // 'instanceof' right operand", which takes down the whole render. These are
  // deliberately all aliases of Element: the identity check is what matters,
  // not a faithful class hierarchy.
  [
    "HTMLAnchorElement", "HTMLAreaElement", "HTMLBodyElement",
    "HTMLButtonElement", "HTMLCanvasElement", "HTMLDivElement",
    "HTMLFormElement", "HTMLHeadElement", "HTMLHtmlElement",
    "HTMLIFrameElement", "HTMLImageElement", "HTMLInputElement",
    "HTMLLabelElement", "HTMLLIElement", "HTMLLinkElement",
    "HTMLOptionElement", "HTMLParagraphElement", "HTMLPreElement",
    "HTMLScriptElement", "HTMLSelectElement", "HTMLSpanElement",
    "HTMLStyleElement", "HTMLTableElement", "HTMLTemplateElement",
    "HTMLTextAreaElement", "HTMLUListElement", "HTMLDialogElement",
    "SVGElement", "SVGSVGElement",
  ].forEach(function (n) {
    if (typeof globalThis[n] === "undefined") globalThis[n] = Element;
  });

  if (typeof globalThis.DOMException === "undefined") {
    globalThis.DOMException = function DOMException(message, name) {
      var e = new Error(message);
      e.name = name || "Error";
      return e;
    };
  }
  globalThis.document = document;

  var loc = globalThis.__CONDUIT_URL__ || "about:blank";
  var locObj = { href: loc, toString: function () { return loc; } };
  try {
    var u = new URL(loc);
    locObj.protocol = u.protocol; locObj.host = u.host;
    locObj.hostname = u.hostname; locObj.port = u.port;
    locObj.pathname = u.pathname; locObj.search = u.search;
    locObj.hash = u.hash; locObj.origin = u.origin;
  } catch (_) {}
  globalThis.location = locObj;
  document.location = locObj;
  document.URL = loc;

  globalThis.navigator = globalThis.navigator || {
    userAgent: "Conduit/0.1 (WebMCP harvester; not a browser)",
    language: "en-US",
    languages: ["en-US"],
    onLine: true,
  };

  // ------------------------------------------------------------- timers
  // QuickJS provides no timers at all — they are a host facility. Without
  // them React's scheduler never flushes, so a component tree is scheduled,
  // never rendered, and any registerTool call inside an effect never runs.
  //
  // Time here is virtual: the queue is ordered by due time and drained as
  // fast as it can be, so a page that waits 500ms does not cost us 500ms.
  var timerSeq = 1;
  var timerQueue = [];
  var virtualNow = 0;

  function schedule(fn, delay, args, repeating) {
    if (typeof fn !== "function") return 0;
    var id = timerSeq++;
    timerQueue.push({
      id: id,
      fn: fn,
      args: args || [],
      due: virtualNow + Math.max(0, delay | 0),
      every: repeating ? Math.max(1, delay | 0) : null,
    });
    return id;
  }

  globalThis.setTimeout = function (fn, delay) {
    return schedule(fn, delay, Array.prototype.slice.call(arguments, 2), false);
  };
  globalThis.setInterval = function (fn, delay) {
    return schedule(fn, delay, Array.prototype.slice.call(arguments, 2), true);
  };
  function clearTimer(id) {
    for (var i = 0; i < timerQueue.length; i++) {
      if (timerQueue[i].id === id) { timerQueue.splice(i, 1); return; }
    }
  }
  globalThis.clearTimeout = clearTimer;
  globalThis.clearInterval = clearTimer;

  if (typeof globalThis.queueMicrotask !== "function") {
    globalThis.queueMicrotask = function (fn) { Promise.resolve().then(fn); };
  }

  // React's scheduler reaches for MessageChannel before it falls back to
  // setTimeout, so it has to exist or the work loop stalls.
  function MessagePort() {
    EventTarget.call(this);
    this.onmessage = null;
    this._peer = null;
  }
  MessagePort.prototype = Object.create(EventTarget.prototype);
  MessagePort.prototype.constructor = MessagePort;
  MessagePort.prototype.postMessage = function (data) {
    var peer = this._peer;
    if (!peer) return;
    globalThis.setTimeout(function () {
      var ev = new Event("message");
      ev.data = data;
      if (typeof peer.onmessage === "function") peer.onmessage(ev);
      peer.dispatchEvent(ev);
    }, 0);
  };
  MessagePort.prototype.start = function () {};
  MessagePort.prototype.close = function () { this._peer = null; };

  globalThis.MessagePort = MessagePort;
  globalThis.MessageChannel = function MessageChannel() {
    this.port1 = new MessagePort();
    this.port2 = new MessagePort();
    this.port1._peer = this.port2;
    this.port2._peer = this.port1;
  };

  // Drain due timers. Returns how many ran so the host knows whether to keep
  // going. Bounded by the host's budget, which is what stops a setInterval
  // from spinning forever.
  globalThis.__conduit_run_timers = function (budget) {
    var ran = 0;
    while (ran < budget && timerQueue.length) {
      timerQueue.sort(function (a, b) { return a.due - b.due || a.id - b.id; });
      var t = timerQueue.shift();
      virtualNow = Math.max(virtualNow, t.due);
      if (t.every !== null) {
        t.due = virtualNow + t.every;
        timerQueue.push(t);
      }
      try {
        t.fn.apply(globalThis, t.args);
      } catch (e) {
        reportError(e);
      }
      ran++;
    }
    return ran;
  };

  // --------------------------------------------------- URL / query strings
  // QuickJS ships neither, and pages reach for them constantly during setup
  // (`new URLSearchParams(location.search)` guarding a registration block is
  // a common pattern in the wild).
  if (typeof globalThis.URLSearchParams !== "function") {
    function URLSearchParams(init) {
      this._p = [];
      if (typeof init === "string") {
        var q = init.charAt(0) === "?" ? init.slice(1) : init;
        if (q) {
          var parts = q.split("&");
          for (var i = 0; i < parts.length; i++) {
            if (!parts[i]) continue;
            var eq = parts[i].indexOf("=");
            var k = eq === -1 ? parts[i] : parts[i].slice(0, eq);
            var v = eq === -1 ? "" : parts[i].slice(eq + 1);
            this._p.push([dec(k), dec(v)]);
          }
        }
      } else if (init && typeof init === "object") {
        for (var key in init) this._p.push([key, String(init[key])]);
      }
    }
    function dec(x) {
      try { return decodeURIComponent(String(x).replace(/\+/g, " ")); }
      catch (e) { return String(x); }
    }
    function enc(x) { return encodeURIComponent(String(x)); }

    URLSearchParams.prototype.get = function (k) {
      for (var i = 0; i < this._p.length; i++) if (this._p[i][0] === k) return this._p[i][1];
      return null;
    };
    URLSearchParams.prototype.getAll = function (k) {
      return this._p.filter(function (e) { return e[0] === k; })
                    .map(function (e) { return e[1]; });
    };
    URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
    URLSearchParams.prototype.append = function (k, v) { this._p.push([String(k), String(v)]); };
    URLSearchParams.prototype.set = function (k, v) {
      this.delete(k); this._p.push([String(k), String(v)]);
    };
    URLSearchParams.prototype.delete = function (k) {
      this._p = this._p.filter(function (e) { return e[0] !== k; });
    };
    URLSearchParams.prototype.forEach = function (fn, self) {
      for (var i = 0; i < this._p.length; i++) fn.call(self, this._p[i][1], this._p[i][0], this);
    };
    URLSearchParams.prototype.keys = function () {
      return this._p.map(function (e) { return e[0]; });
    };
    URLSearchParams.prototype.values = function () {
      return this._p.map(function (e) { return e[1]; });
    };
    URLSearchParams.prototype.entries = function () { return this._p.slice(); };
    URLSearchParams.prototype.toString = function () {
      return this._p.map(function (e) { return enc(e[0]) + "=" + enc(e[1]); }).join("&");
    };
    globalThis.URLSearchParams = URLSearchParams;
  }

  if (typeof globalThis.URL !== "function") {
    var URL_RE = /^([a-zA-Z][a-zA-Z0-9+.-]*:)\/\/([^\/?#:]*)(?::(\d+))?([^?#]*)(\?[^#]*)?(#.*)?$/;
    function URLShim(input, base) {
      var href = String(input);
      if (base && !URL_RE.test(href)) {
        var b = String(base).replace(/[?#].*$/, "");
        if (href.charAt(0) === "/") {
          var m0 = URL_RE.exec(b);
          href = m0 ? m0[1] + "//" + m0[2] + (m0[3] ? ":" + m0[3] : "") + href : href;
        } else {
          href = b.replace(/\/[^\/]*$/, "/") + href;
        }
      }
      var m = URL_RE.exec(href);
      if (!m) throw new TypeError("Invalid URL: " + input);
      this.href = href;
      this.protocol = m[1];
      this.hostname = m[2];
      this.port = m[3] || "";
      this.host = m[2] + (m[3] ? ":" + m[3] : "");
      this.pathname = m[4] || "/";
      this.search = m[5] || "";
      this.hash = m[6] || "";
      this.origin = m[1] + "//" + this.host;
      this.searchParams = new globalThis.URLSearchParams(this.search);
    }
    URLShim.prototype.toString = function () { return this.href; };
    globalThis.URL = URLShim;
  }

  // The window object is an EventTarget. `window` was aliased to globalThis
  // above, but that alias alone leaves `window.addEventListener` undefined,
  // which breaks any page that wires up load or hashchange handlers.
  EventTarget.call(globalThis);
  globalThis.addEventListener = EventTarget.prototype.addEventListener;
  globalThis.removeEventListener = EventTarget.prototype.removeEventListener;
  globalThis.dispatchEvent = EventTarget.prototype.dispatchEvent;

  globalThis.history = {
    length: 1,
    state: null,
    pushState: function (s) { this.state = s; },
    replaceState: function (s) { this.state = s; },
    back: function () {}, forward: function () {}, go: function () {},
  };

  // Pages routinely defer registration until the document is ready. Nothing
  // fires these on its own here, so the host triggers them once every script
  // has run.
  globalThis.__conduit_fire_ready = function () {
    try {
      document.readyState = "interactive";
      document.dispatchEvent(new Event("DOMContentLoaded", { bubbles: true }));
      document.readyState = "complete";
      globalThis.dispatchEvent(new Event("load"));
      if (typeof globalThis.onload === "function") globalThis.onload(new Event("load"));
      globalThis.dispatchEvent(new Event("pageshow"));
    } catch (e) {
      reportError(e);
    }
  };
  document.readyState = "loading";

  globalThis.window = globalThis;
  globalThis.self = globalThis;
  globalThis.localStorage = (function () {
    var store = Object.create(null);
    return {
      getItem: function (k) { return k in store ? store[k] : null; },
      setItem: function (k, v) { store[k] = String(v); },
      removeItem: function (k) { delete store[k]; },
      clear: function () { store = Object.create(null); },
      key: function (i) { return Object.keys(store)[i] || null; },
      get length() { return Object.keys(store).length; },
    };
  })();
  globalThis.sessionStorage = globalThis.localStorage;

  if (typeof globalThis.console === "undefined") {
    globalThis.console = {};
  }
  ["log", "info", "warn", "error", "debug"].forEach(function (m) {
    if (typeof globalThis.console[m] !== "function") {
      globalThis.console[m] = function () {};
    }
  });

  // Observers a page may construct during registration. Inert, but present,
  // so constructing one does not throw and abort the whole script.
  function InertObserver(name) {
    return function () {
      noteMissing(name + " (constructed, inert)");
      this.observe = function () {};
      this.unobserve = function () {};
      this.disconnect = function () {};
      this.takeRecords = function () { return []; };
    };
  }
  globalThis.MutationObserver = InertObserver("MutationObserver");
  globalThis.IntersectionObserver = InertObserver("IntersectionObserver");
  globalThis.ResizeObserver = InertObserver("ResizeObserver");

  globalThis.requestAnimationFrame = function (fn) {
    return globalThis.setTimeout(function () { fn(virtualNow); }, 16);
  };
  globalThis.cancelAnimationFrame = function (id) {
    globalThis.clearTimeout(id);
  };
  globalThis.matchMedia = function (q) {
    noteMissing("matchMedia (always non-matching)");
    return {
      matches: false, media: q,
      addEventListener: function () {}, removeEventListener: function () {},
      addListener: function () {}, removeListener: function () {},
    };
  };
  globalThis.getComputedStyle = function () {
    noteMissing("getComputedStyle (empty style)");
    return { getPropertyValue: function () { return ""; } };
  };
  globalThis.alert = function () {};
  globalThis.scrollTo = function () {};
})(globalThis);
