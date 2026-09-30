# conduit

[![CI](https://github.com/Dhananjay-JSR/webmcp-conduit/actions/workflows/main.yml/badge.svg)](https://github.com/Dhananjay-JSR/webmcp-conduit/actions/workflows/main.yml)
[![crates.io](https://img.shields.io/crates/v/webmcp-conduit.svg)](https://crates.io/crates/webmcp-conduit)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Turn any WebMCP-enabled website into a standard MCP server. No browser required.**

A website can expose its own functionality to an AI agent as tools, using the
W3C [WebMCP](https://github.com/webmachinelearning/webmcp) API. That works in
browsers with a built-in agent. It does not work in Claude Code, Cursor, Zed,
or CI, because those are not browsers.

`conduit` bridges the two. Point it at a URL, get an ordinary MCP server.

```bash
conduit serve https://example.com
```

The client on the other end has never heard of WebMCP. No widget on the page,
no token to paste, no separate client to install.

## Install

```bash
# curl — downloads the release binary for your platform
curl -fsSL https://raw.githubusercontent.com/Dhananjay-JSR/webmcp-conduit/main/install.sh | sh

# cargo
cargo install webmcp-conduit
```

One static binary. No Node, no Chromium.

## Use

See what a page exposes:

```console
$ conduit probe https://example.com
https://example.com
  engine: isolate   scripts: 3/3 ran   tools: 3

  example_com__search [read-only]
      Search the catalogue and return matching items
  example_com__add-to-cart
      Add an item to the shopping cart
  example_com__checkout [consequential]
      Place the order
```

Add it to an MCP client — Claude Desktop, Claude Code, Cursor, anything that
takes a command. There is no port and no daemon: the client launches `conduit`
as a child process and talks to it over stdin and stdout.

```json
{
  "mcpServers": {
    "example": {
      "command": "conduit",
      "args": ["serve", "https://example.com"]
    }
  }
}
```

### Commands

| | |
|---|---|
| `conduit serve <url>` | Speak MCP over stdio — what an MCP client spawns |
| `conduit serve --transport http --site <name>=<url>` | Serve declared sites over HTTP, each at `/<name>` |
| `conduit probe <url>` | Report what the page exposes, and what failed |
| `--session <id>` | Remember cookies and storage between runs |
| `--json` | Machine-readable output |
| `--eval '<js>'` | Evaluate an expression in the loaded page |
| `--no-scripts` | Declarative `<form>` tools only, no JavaScript |

`<url>` may also be a path to a local HTML file.

### Sessions

By default a page boots with empty storage every time, which makes a local-first
app look permanently new and makes "sign in, then do the thing" impossible to
express. A named session remembers cookies, `localStorage` and IndexedDB between
runs:

```
conduit serve https://example.com --session alice
```

Two ids are two unrelated visitors, and neither can see the other's data.

Sessions are directories of JSON under `CONDUIT_SESSION_DIR`, or the platform
data directory. conduit has no commands for listing or deleting them: it serves
pages, and the format is plain enough that `ls` and `rm` are the management
tools.

### Over HTTP

Served sites are declared up front. A caller picks which site to talk to, never
which URL to fetch and never which session to speak for — so the server is not
an open proxy, and nobody can borrow someone else's signed-in state by naming
it. Mounting one site twice gives it two independent profiles:

```
conduit serve --transport http \
  --site alice=https://example.com \
  --site bob=https://example.com
```

```
POST /alice     MCP, as alice
POST /bob       MCP, as bob
GET  /          {"sites":["alice","bob"]}
GET  /healthz
```

## How it works

Two engines, neither of which is a browser:

| Tier | Mechanism | Finds |
|---|---|---|
| **L0 static** | HTML parse | Declarative `<form toolname=...>` tools |
| **L1 isolate** | QuickJS + happy-dom | Imperative `registerTool()` tools |

L1 is the interesting one. Imperative tools are registered by *running code* —
`execute` is a closure over live page state, so there is nothing in the HTML to
parse. `conduit` executes the page's own scripts in a QuickJS isolate against a
real DOM, then harvests what got registered. The spec sanctions this path:
*"In-page agents implemented in JavaScript can observe the tools that a page
offers by using the ModelContext APIs directly."*

Page state persists across calls. A tool that adds an item and a tool that
lists them see the same page.

For a walk through the code itself, see
[DeepWiki](https://deepwiki.com/Dhananjay-JSR/webmcp-conduit), which reads the
repository directly and so cannot drift out of date.

## When a page comes up empty

`probe` tells you why rather than guessing. It reports script errors with
positions, unhandled promise rejections, modules the page imported that were
never resolved, and platform APIs it reached for that do not exist here:

```console
  Modules the page imported that were not prefetched:
    - https://cdn.example/chunk-a1b2.js

  Platform APIs this page wanted that conduit does not implement:
    - canvas.getContext('webgl') — no GPU, so nothing renders
```

It also distinguishes a page that never looked for `document.modelContext`
from one that looked and never registered, and from one that only *consumes*
tools — for which an empty list is the correct answer.

## Scope

`conduit` targets the **W3C WebMCP specification** — `document.modelContext`,
as implemented in Chrome and ChatGPT.

## Respecting opt-out

The spec gates WebMCP behind a `tools` policy-controlled feature, and a site
disables it with `Permissions-Policy: tools=()`. A browser enforces that before
any script runs. `conduit` is not a browser, so it checks the header itself and
refuses rather than quietly overriding a site's explicit opt-out.

Scripts and network requests are restricted to the page's own origin, beyond a
short list of well-known script CDNs.

## Security

Tool names, descriptions and results are page-authored strings that flow into a
model's context. Treat every site you point this at as untrusted.

`conduit` uses the spec's own annotations as the defence the spec intends:

- `consequentialHint` → surfaced as MCP `destructiveHint` *and* stated in the
  description, so the model sees it even on clients that ignore annotations.
- `untrustedContentHint` → results are fenced in an explicit
  `<untrusted-content>` block marked as data, not instructions.
- `debugging` → filtered out before the model ever sees the tool.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: platform behaviour
gets vendored from a maintained package, host decisions are written here, and
anything a page can fail on should be reported by `probe` rather than guessed
at.

## License

Apache-2.0
