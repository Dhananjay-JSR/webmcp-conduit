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

Sessions are directories under `CONDUIT_SESSION_DIR`, or the platform data
directory, each holding one gzipped JSON document. A session is a dump of
`localStorage` and every IndexedDB a site wrote — JSON describing JSON, which
compresses by one to two orders of magnitude, and the deployments holding the
most sessions are the ones least able to spend disk on whitespace.

conduit has no commands for listing or deleting them: it serves pages, and the
format is plain enough that `ls`, `zcat` and `rm` are the management tools.

### Over HTTP

Served sites are declared up front. A caller picks which site to talk to, never
which URL to fetch, so the server is not an open proxy.

```
conduit serve --transport http --site notes=https://example.com
```

```
POST /notes     MCP
GET  /          {"sites":["notes"]}
GET  /healthz
```

**Each MCP connection gets its own page.** Two clients talking to the same site
do not see each other's cookies or storage, and nothing is written to disk —
the page is dropped when the connection ends. That isolation comes from the
transport: the protocol issues a session id at `initialize` and clients return
it, so conduit knows which connection a request belongs to without the caller
arranging anything.

To keep state across connections, name a session:

```
POST /notes?session=<secret>
```

Cookies, `localStorage` and IndexedDB then persist, and any caller presenting
the same value joins the same page. It is a **credential rather than a name**:
whoever knows it gets whatever it is signed into, so it should be random.
`alice` means the first person to try `alice` is alice.

### Letting a caller name the page

Off unless asked for:

```
conduit serve --transport http --allow-any-site
POST /connect?url=https%3A%2F%2Fexample.com%2Fapp
```

Declared sites are optional once this is on, so a deployment that only ever
takes `/connect?url=` needs no `--site` at all.

The engine will load a page it was not configured with, which is a capability
and not a permission: it refuses `file://`, loopback, private networks,
link-local — `169.254.169.254` included — and anything that is not http or
https. What it does not do is decide *who* may ask, or which sites are
reasonable. That belongs to whatever is in front of it, along with
authentication and rate limiting, because the engine has no idea who you are
and should not acquire one.

Pages are held in memory and the least recently used is retired beyond
`--max-engines` (16 by default), which puts a ceiling on memory rather than
letting it follow how many people turned up. Retiring writes a named session
back first, so nothing is lost — the next request reloads the page.

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
