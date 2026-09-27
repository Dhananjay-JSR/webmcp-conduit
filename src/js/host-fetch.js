// XMLHttpRequest, backed by the host.
//
// This is deliberately ours rather than vendored. The spec types — Headers,
// Request, Response — come from whatwg-fetch; the *transport* does not,
// because the network boundary is where conduit's origin policy lives. A
// vendored transport would either have nothing to talk to or would bypass
// that policy entirely.
//
// whatwg-fetch drives XMLHttpRequest, so implementing XHR is enough to give
// the page a real, spec-shaped fetch.
(function (globalThis) {
  "use strict";

  var UNSENT = 0, OPENED = 1, HEADERS_RECEIVED = 2, LOADING = 3, DONE = 4;

  function XMLHttpRequest() {
    this.readyState = UNSENT;
    this.status = 0;
    this.statusText = "";
    this.response = "";
    this.responseText = "";
    this.responseType = "";
    this.responseURL = "";
    this.timeout = 0;
    this.withCredentials = false;
    this.onload = null;
    this.onerror = null;
    this.onabort = null;
    this.ontimeout = null;
    this.onreadystatechange = null;
    this._method = "GET";
    this._url = "";
    this._headers = {};
    this._responseHeaders = {};
    this._aborted = false;
  }

  XMLHttpRequest.prototype.open = function (method, url) {
    this._method = String(method || "GET").toUpperCase();
    // Pages request relative URLs constantly. The host needs an absolute one,
    // so resolve against the document here rather than making every caller
    // remember to.
    var raw = String(url);
    try {
      var base = (globalThis.location && globalThis.location.href) || undefined;
      this._url = base ? new globalThis.URL(raw, base).href : raw;
    } catch (e) {
      this._url = raw;
    }
    this.readyState = OPENED;
    this._fireReadyState();
  };

  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    this._headers[String(name)] = String(value);
  };

  XMLHttpRequest.prototype.getAllResponseHeaders = function () {
    var out = [];
    for (var k in this._responseHeaders) {
      out.push(k.toLowerCase() + ": " + this._responseHeaders[k]);
    }
    return out.join("\r\n");
  };

  XMLHttpRequest.prototype.getResponseHeader = function (name) {
    var want = String(name).toLowerCase();
    for (var k in this._responseHeaders) {
      if (k.toLowerCase() === want) return this._responseHeaders[k];
    }
    return null;
  };

  XMLHttpRequest.prototype.abort = function () {
    this._aborted = true;
    if (typeof this.onabort === "function") this.onabort();
  };

  XMLHttpRequest.prototype._fireReadyState = function () {
    if (typeof this.onreadystatechange === "function") {
      try { this.onreadystatechange(); } catch (e) { globalThis.__conduit_report_error(e); }
    }
  };

  XMLHttpRequest.prototype.send = function (body) {
    var self = this;

    if (typeof globalThis.__conduit_http !== "function") {
      globalThis.setTimeout(function () {
        if (typeof self.onerror === "function") self.onerror(new Error("no transport"));
      }, 0);
      return;
    }

    // The host call is synchronous — conduit harvests a page rather than
    // driving a live one, so there is nothing to interleave with. The
    // callbacks are still dispatched on a timer so callers see the
    // asynchronous behaviour the API promises.
    var raw;
    try {
      raw = globalThis.__conduit_http(
        self._method,
        self._url,
        JSON.stringify(self._headers),
        body == null ? "" : String(body)
      );
    } catch (e) {
      globalThis.setTimeout(function () {
        if (typeof self.onerror === "function") self.onerror(e);
      }, 0);
      return;
    }

    globalThis.setTimeout(function () {
      if (self._aborted) return;
      var result;
      try {
        result = JSON.parse(raw);
      } catch (e) {
        if (typeof self.onerror === "function") self.onerror(e);
        return;
      }

      if (result.error) {
        globalThis.__conduit_note_missing("fetch blocked: " + result.error);
        if (typeof self.onerror === "function") self.onerror(new Error(result.error));
        return;
      }

      self.status = result.status || 0;
      self.statusText = result.statusText || "";
      self.responseURL = result.url || self._url;
      self._responseHeaders = result.headers || {};
      self.responseText = result.body || "";
      self.response = self.responseText;
      self.readyState = DONE;
      self._fireReadyState();
      if (typeof self.onload === "function") self.onload();
    }, 0);
  };

  globalThis.XMLHttpRequest = XMLHttpRequest;
  if (globalThis.window && globalThis.window !== globalThis) {
    try { globalThis.window.XMLHttpRequest = XMLHttpRequest; } catch (e) { /* sealed */ }
  }
})(globalThis);
