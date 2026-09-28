# Vendored JavaScript

Three bundles are embedded into the binary with `include_str!`:

| File | Contents | Why |
|---|---|---|
| `vendor/encoding.js` | TextEncoder / TextDecoder | Must load first — other packages construct one at module scope |
| `vendor/platform.js` | Streams, URL, structuredClone | The web platform QuickJS lacks |
| `vendor/happy-dom.js` | The DOM | |

They are vendored rather than built from npm at compile time so that
`cargo install webmcp-conduit` needs no Node toolchain.

Nothing here is hand-written. Writing a DOM by hand is a treadmill where every
new site finds a new gap; writing the platform layer by hand is the same
mistake one layer down. What `host-pre.js` still owns is only what no library
can provide, because it is a host decision: timers on a virtual clock, the
console bridge to Rust tracing, and egress policy.

## Two choices that are not obvious

**core-js for URL, not whatwg-url.** whatwg-url is the spec-correct choice and
does not work here: it builds its interface objects through webidl2js, and the
result is not constructible under QuickJS — `new URL(...)` throws "not a
constructor". core-js targets old engines and works.

**The browser build of the text encoder, not the Node build.** The Node build
encodes through `Buffer`, which does not meaningfully exist here, and fails at
`encode()` rather than at load — so it looks fine until something uses it.

## Regenerating

```bash
cd vendor-build
npm install
npm run build
```

That rewrites `src/js/vendor/happy-dom.js` and refreshes the bundled LICENSE.

## Mainline happy-dom, and the stubs it needs

This builds mainline happy-dom rather than the `-without-node` fork, which
lagged six major versions behind. Three stubs make it load under QuickJS:

- **`ws`** is aliased out entirely. happy-dom bundles it for WebSocket, it
  needs real Node streams, and a harvest never opens a socket.
- **`vm.isContext()` returns true.** happy-dom makes its Window the global of
  a real VM context; returning true makes that setup a no-op, which is the
  right answer here because `host-post.js` hoists the Window onto globalThis
  anyway — that is what a browser gives you.
- **`util` re-exports the real TextEncoder/TextDecoder** instead of defining
  its own. A naive stand-in there gets hoisted over the correct one and
  corrupts every non-ASCII character on the page, quietly.

v20 also wants `WeakRef`, which QuickJS lacks; `host-pre.js` supplies a
strong-reference stand-in.

## Why not a real browser

A headless browser is the obvious way to reach a page's WebMCP tools, and the
expensive one. Lightpanda is the closest purpose-built alternative, but it is
AGPL-3.0 and an external binary, which would cost both the license and the
single-binary property.
