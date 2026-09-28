# React hydration fixture

A real React 19 app, server-rendered and hydrated with
`hydrateRoot(document, ...)`, registering a WebMCP tool from inside a
`useEffect` on mount.

That shape is deliberate: it is what React Server Components apps do, and it
is the exact path that fails on OpenAI's Margin demo. This fixture exists to
answer whether the failure is React hydration itself — it is not. Both
`hydrateRoot(document)` and `hydrateRoot(rootDiv)` hydrate and register
correctly here, which localises Margin's problem to the RSC payload path
(`createFromReadableStream`) rather than to hydration or effects.

## Rebuilding

The bundle is checked in so `cargo test` needs no Node toolchain.

```bash
cd fixtures/react
npm install react react-dom esbuild
node build.mjs      # client bundle
node ssr.mjs        # server HTML that the client hydrates
```

The server HTML has to match what the client renders, or React discards it and
client-renders instead — which would quietly defeat the point of the fixture.
