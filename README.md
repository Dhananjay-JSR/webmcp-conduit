# conduit

**Turn any WebMCP-enabled website into a standard MCP server. No browser required.**

WebMCP lets a website hand its own functionality to an AI agent as tools. It works
today in ChatGPT's browser and in Chrome. It does not work in Claude, Claude Code,
Cursor, Zed, or CI — because those aren't browsers, and the
[W3C spec](https://github.com/webmachinelearning/webmcp) deliberately leaves the
bridge to external clients undefined:

> Despite the name of this API (i.e., Web**MCP**), this specification does not
> prescribe the format in which tools are exposed to the browser agent.

`conduit` is that bridge. Point it at a URL, get an ordinary MCP server.

```bash
conduit serve https://example.com
```

The client on the other end has never heard of WebMCP. No widget on the page, no
token to paste, no separate client to install.

## Install

```bash
cargo install webmcp-conduit
```

A single static binary. No Node, no Chromium, no 300 MB download.

## Use

Inspect what a page exposes:

```console
$ conduit probe https://todo.example
https://todo.example
  engine: isolate   scripts: 1/1 ran   tools: 3

  todo_example__add-todo
      Add a new item to the user's active todo list
  todo_example__list-todos [read-only]
      List every todo item currently on the page
  todo_example__clear-all [consequential]
      Delete every todo permanently
```

Wire it into an MCP client — Claude Desktop, Claude Code, Cursor, anything:

```json
{
  "mcpServers": {
    "todo": {
      "command": "conduit",
      "args": ["serve", "https://todo.example"]
    }
  }
}
```

## How it works

Most tools reach for a headless browser. Browsers are slow, expensive, and on
managed platforms you can't even pass them the flags you need. `conduit` runs a
tiered engine instead and tells you which tier answered.

| Tier | Mechanism | Cost | Finds |
|------|-----------|------|-------|
| **L0 static** | HTML parse | ~free | Declarative `<form toolname=...>` tools |
| **L1 isolate** | QuickJS + a micro-DOM | milliseconds | Imperative `registerTool()` tools |

L1 is the interesting one. Imperative tools are registered by *running code* —
`execute` is a closure over live page state, so there is nothing in the HTML to
parse. `conduit` executes the page's own scripts in a QuickJS isolate against a
minimal DOM, then harvests what got registered. The spec explicitly sanctions
this path: *"In-page agents implemented in JavaScript can observe the tools that
a page offers by using the ModelContext APIs directly."*

## What it is not

It is not a browser. There is no layout, no rendering, no canvas. A page that
gates tool registration behind real geometry or a heavy framework boot will come
up empty.

Rather than paper over that, every platform API a page reaches for and doesn't
find is recorded and reported:

```console
$ conduit probe https://spa.example
  Platform APIs this page wanted that conduit does not implement:
    - Element.getBoundingClientRect (layout not simulated)
    - IntersectionObserver (constructed, inert)
```

That list is the roadmap. If a site you care about fails, run `probe` and open an
issue with the output.

## Honest status

Early. The engine works end to end — page scripts execute, tools register,
`tools/call` mutates real page state that persists across calls. What's missing:

- **Framework commit phases.** React renders without error but does not commit,
  so tools registered inside `useEffect` never run. Vanilla-JS pages work well.
- **`fetch` inside tools.** Tools that call their own backend during `execute`
  will not settle.
- **Authenticated sessions.** No cookie jar yet, so logged-in sites see you
  logged out.

## Scope

`conduit` targets the **W3C WebMCP specification** — `document.modelContext`,
as implemented in Chrome and ChatGPT — and nothing else.

The pre-standard [jasonjmcghee/WebMCP](https://github.com/jasonjmcghee/WebMCP)
widget (`new WebMCP()` with positional `registerTool(name, desc, schema, fn)`,
plus a localhost WebSocket bridge) is **explicitly out of scope**. Its own
README now points to the W3C spec. Supporting a second, incompatible shape
would double the surface area of the engine to chase an API that is being
retired. Sites built on it — `webmcp.dev` among them — will probe empty, by
design rather than by omission.

Also worth saying plainly: very few sites ship WebMCP today. This is
infrastructure for a standard that is still arriving.

## Respecting opt-out

The spec gates WebMCP behind a `tools` policy-controlled feature, and a site
disables it with `Permissions-Policy: tools=()`. A browser enforces that before
any script runs. `conduit` is not a browser, so it checks the header itself and
refuses rather than quietly overriding a site's explicit opt-out.

Cross-origin scripts are not executed. A page's tools should come from the page.

## Security

Tool names, descriptions, and results are page-authored strings that flow into a
model's context. Treat every site you point this at as untrusted.

`conduit` uses the spec's own annotations as the defence the spec intends:

- `consequentialHint` → surfaced as MCP `destructiveHint` *and* stated in the
  description, so the model sees it even on clients that ignore annotations.
- `untrustedContentHint` → results are fenced in an explicit
  `<untrusted-content>` block marked as data, not instructions.
- `debugging` → filtered out before the model ever sees the tool.

## License

Apache-2.0
