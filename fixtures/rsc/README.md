# React Server Components fixture

The full RSC client path, in the shape OpenAI's Margin demo uses:

1. classic inline scripts push flight rows onto a global (`self.__RSC_CHUNKS__`)
2. a module turns them into a `ReadableStream`
3. `createFromReadableStream` decodes it
4. `use()` unwraps the promise inside a Suspense boundary
5. `hydrateRoot(document, ...)` hydrates the whole document
6. a **client component resolved from the payload by module id** registers a
   WebMCP tool from `useEffect`

Step 6 is the part that matters. A flight payload refers to client components
by module id (`1:I["cmod",[],"Widget"]`) and the client resolves them through a
module runtime — webpack's here, vite's in Margin. Registration happening
inside such a component is exactly Margin's shape.

All of it works, which is the point: it rules out RSC as the explanation for
Margin registering nothing, the same way `fixtures/react/` ruled out plain
hydration.

`self` rather than `globalThis` in the chunk handoff is deliberate: those were
two different objects once, and it is how the payload went missing.

## Rebuilding

The bundle is checked in so `cargo test` needs no Node toolchain.

```bash
cd fixtures/rsc
npm install react react-dom react-server-dom-webpack esbuild
node buildrsc.mjs
```
