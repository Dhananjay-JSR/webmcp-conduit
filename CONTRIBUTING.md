# Contributing

Thanks for looking. This is a small project with a few opinions, and knowing
them up front will save you a round trip in review.

## Getting set up

```bash
cargo build
cargo test
```

No Node toolchain is needed. The JavaScript that runs inside the engine is
vendored and checked in, and `build.rs` makes cargo rebuild when it changes.

To try it against something real:

```bash
cargo run -- probe https://example.com
cargo run -- probe ./fixtures/todo.html --eval "document.title"
```

`CONDUIT_LOG=debug` turns on the page's own `console` output, which is usually
the fastest way to find out why a page did not do what you expected.

## The one rule worth knowing

**Platform behaviour gets vendored. Host decisions get written here.**

QuickJS is a bare JavaScript engine, so a lot of the web is missing from it.
The temptation is to implement the missing piece. Don't — that is a treadmill
where every new site finds a new gap, and none of that work is about WebMCP.

If a page needs a web API that does not exist, add a maintained package to
`vendor-build/` and rebuild. Streams, URL, encoding, `Intl`, IndexedDB and the
`fetch` types all arrived that way.

The exceptions are things no library can provide because they are decisions
only the host can make:

| Ours | Why |
|---|---|
| Timers | They run on a virtual clock, so a page that waits 500ms costs nothing |
| Network | This is where the origin policy lives |
| Entropy | `crypto` needs a real source |
| Scheduling | `MessageChannel` delivers through our timer queue |
| `console` | It bridges to the host's tracing |

If you are unsure which side something falls on, ask in the issue before
writing it.

### Regenerating the vendored JavaScript

```bash
cd vendor-build
npm install
npm run build
```

`vendor-build/README.md` records why each stub exists. Some of them are not
obvious — `vm.isContext()` returns `true` on purpose, and the `util` stub
re-exports the real `TextEncoder` rather than defining one, because a naive
stand-in there silently corrupts every non-ASCII character on the page.

## Diagnostics are a feature, not debug output

The hardest failures here are silent: a page runs cleanly, registers nothing,
and says nothing about why. Several of those cost a day each before the
reporting improved.

So when you fix something that was hard to find, ask whether `probe` could have
told you. Unhandled rejections, unresolved modules, missing platform APIs and
GPU context requests are all reported for exactly that reason.

The corollary: **a diagnostic that guesses is worse than one that measures.**
`probe` used to lead with "the site does not use WebMCP" for a site that
demonstrably did. It now counts whether the page read `document.modelContext`
and whether it called `registerTool`, and says only what those support.

## Tests

`cargo test` should pass before and after your change.

Fixtures live in `fixtures/`. Two are worth knowing about because they exist to
rule things out rather than to test a feature:

- `fixtures/react/` — a real React app hydrated with `hydrateRoot(document)`,
  registering from an effect
- `fixtures/rsc/` — the full React Server Components path, including a client
  component resolved from the flight payload by module id

Both have their bundles checked in so the tests need no Node. If you are
chasing a framework-shaped bug, building a fixture like these is usually faster
than debugging against a live site, and it leaves behind a regression test.

If you add a platform capability, add it to the surface test in
`src/isolate.rs` so a dependency bump cannot silently drop it.

## Scope

`conduit` targets the W3C WebMCP specification — `document.modelContext`. The
pre-standard widget API is out of scope.

It is not a browser and is not trying to become one. There is no layout engine
and no GPU. A page that depends on either is a limitation to report clearly,
not a gap to paper over.

## Reporting a site that does not work

Run `conduit probe <url> --json` and include the output. It carries the script
errors, unresolved modules, unhandled rejections and missing APIs, which is
almost always enough to say what happened without anyone having to reproduce
it first.
