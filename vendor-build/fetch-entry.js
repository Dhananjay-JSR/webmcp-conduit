// Spec types for fetch. The transport is deliberately NOT from here:
// whatwg-fetch drives XMLHttpRequest, and conduit supplies that itself,
// because the network boundary is where the origin allowlist lives.
import { fetch, Headers, Request, Response } from 'whatwg-fetch';

// Forced, not conditional. happy-dom-without-node ships fetch, Headers,
// Request and Response as empty shells — Headers.prototype carries only
// `constructor` — and host-post.js has already hoisted those onto globalThis
// by the time this runs.
function force(name, value) {
  try {
    Object.defineProperty(globalThis, name, {
      value, writable: true, configurable: true, enumerable: false,
    });
  } catch (e) { globalThis[name] = value; }
  if (globalThis.window && globalThis.window !== globalThis) {
    try { globalThis.window[name] = value; } catch (e) { /* sealed */ }
  }
}

force('Headers', Headers);
force('Request', Request);
force('Response', Response);
force('fetch', fetch);
