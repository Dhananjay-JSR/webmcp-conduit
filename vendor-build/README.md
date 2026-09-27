# Vendored happy-dom

`src/js/vendor/happy-dom.js` is a bundled build of
[happy-dom](https://github.com/capricorn86/happy-dom) (MIT), embedded into the
binary with `include_str!`.

It is vendored rather than built from npm at compile time so that
`cargo install webmcp-conduit` needs no Node toolchain.

## Regenerating

```bash
cd vendor-build
npm install
npm run build
```

That rewrites `src/js/vendor/happy-dom.js` and refreshes the bundled LICENSE.

## Why `happy-dom-without-node`

Mainline happy-dom pulls in `ws` and real Node streams for WebSocket support,
which does not load under QuickJS. The `-without-node` build drops those.

It does lag mainline. Moving to current happy-dom is possible — it parses and
loads under QuickJS with `ws` aliased out and the Node builtins stubbed — but
Window construction needs a few more shims than are in `host-pre.js` today.

## Why not a real browser

A headless browser is the obvious way to reach a page's WebMCP tools, and the
expensive one. Lightpanda is the closest purpose-built alternative, but it is
AGPL-3.0 and an external binary, which would cost both the license and the
single-binary property.
