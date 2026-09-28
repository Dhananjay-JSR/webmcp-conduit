//! conduit — turn any WebMCP-enabled website into a standard MCP server.
//!
//! No browser. No widget on the page. No token to paste. The client on the
//! other end is an ordinary MCP client that has never heard of WebMCP.

mod declarative;
mod fetch;
mod isolate;
mod mcp;
mod session;
mod modules;
mod tool;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "conduit",
    version,
    about = "Turn any WebMCP-enabled website into a standard MCP server. No browser required."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect a page: what tools it exposes, and what the engine could not do.
    Probe {
        /// A URL, or a path to a local HTML file.
        target: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Skip JavaScript entirely — declarative `<form>` tools only.
        #[arg(long)]
        no_scripts: bool,
        /// Evaluate an expression in the loaded page and print the result.
        /// For diagnosing why a page did not expose what you expected.
        #[arg(long, value_name = "JS")]
        eval: Option<String>,
        /// Run in a named session, so the page sees the storage it left
        /// behind last time. Two ids are two unrelated visitors.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Serve the page's tools over MCP on stdio.
    Serve {
        /// A URL, or a path to a local HTML file.
        target: String,
        /// Skip JavaScript entirely — declarative `<form>` tools only.
        #[arg(long)]
        no_scripts: bool,
        /// Run in a named session, so the page sees the storage it left
        /// behind last time. Two ids are two unrelated visitors.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Inspect and manage saved sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List saved sessions.
    List,
    /// Show what one session is holding.
    Show {
        id: String,
        /// Print the raw state, including every stored record.
        #[arg(long)]
        json: bool,
    },
    /// Delete a session and everything in it.
    Rm { id: String },
}

/// The engine needs a deep stack. A framework reconciler recurses once per
/// node in the component tree, and React catches the resulting overflow,
/// reports "an error occurred in a React component", and renders nothing — so
/// the symptom is a blank page rather than a crash.
///
/// The main thread's stack is fixed at whatever the OS gave it, so the work
/// runs on a thread we size ourselves.
const ENGINE_STACK: usize = 256 * 1024 * 1024;

fn main() -> Result<()> {
    std::thread::Builder::new()
        .name("conduit-engine".into())
        .stack_size(ENGINE_STACK)
        .spawn(run)?
        .join()
        .map_err(|_| anyhow::anyhow!("engine thread panicked"))?
}

// QuickJS contexts are not `Send`, so everything stays on one thread.
#[tokio::main(flavor = "current_thread")]
async fn run() -> Result<()> {
    // A bare level is scoped to our own targets. Otherwise `CONDUIT_LOG=debug`
    // also turns on html5ever's tree-builder, which drowns the page's console
    // output in tokenizer noise — and the page's console is the entire reason
    // to turn logging up.
    let filter = match std::env::var("CONDUIT_LOG") {
        Ok(v) if v.contains('=') => v,
        Ok(v) if !v.trim().is_empty() => format!("conduit={v},page={v},http={v}"),
        _ => "warn".to_string(),
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Probe {
            target,
            json: as_json,
            no_scripts,
            eval,
            session: session_id,
        } => {
            let mut handle = open_session(session_id.as_deref())?;
            let mut loaded = mcp::Session::load(&target, !no_scripts, handle.as_ref()).await?;

            if let Some(expr) = eval {
                println!("{}", loaded.eval(&expr)?);
                commit_session(handle.as_mut(), &mut loaded)?;
                return Ok(());
            }
            if as_json {
                print_json(&loaded, &target);
            } else {
                print_report(&loaded, &target);
            }
            commit_session(handle.as_mut(), &mut loaded)?;
        }
        Command::Serve {
            target,
            no_scripts,
            session: session_id,
        } => {
            let mut handle = open_session(session_id.as_deref())?;
            let mut loaded = mcp::Session::load(&target, !no_scripts, handle.as_ref()).await?;
            // stdout is the MCP transport; status goes to stderr.
            eprintln!(
                "conduit: serving {} tool(s) from {target} (engine: {}){}",
                loaded.tools().len(),
                loaded.engine.as_str(),
                match &handle {
                    Some(h) => format!(" [session: {}]", h.id()),
                    None => String::new(),
                }
            );
            mcp::serve(&mut loaded, &target).await?;

            // Tools mutate the page: a tool call that writes to IndexedDB is
            // the entire reason to keep a session. Saving only on the way out
            // means a client that disconnects cleanly keeps its work.
            commit_session(handle.as_mut(), &mut loaded)?;
        }
        Command::Session { command } => run_session_command(command)?,
    }

    Ok(())
}

/// Open a session, or none at all. An unusable session id is worth failing on
/// rather than silently falling back to a throwaway run — the caller asked for
/// persistence and would not get it.
fn open_session(id: Option<&str>) -> Result<Option<session::Handle>> {
    match id {
        Some(id) => Ok(Some(session::Handle::open(id)?)),
        None => Ok(None),
    }
}

/// Write the page's storage back. A snapshot failure is reported but does not
/// fail the command: the work the user asked for has already been done, and
/// losing it is bad enough without also hiding the reason.
fn commit_session(handle: Option<&mut session::Handle>, loaded: &mut mcp::Session) -> Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };

    match loaded.snapshot() {
        Ok(Some(storage)) => {
            let origin = loaded.origin();
            match handle.commit(&origin, &storage) {
                Ok(path) => tracing::debug!(target: "conduit", "session saved to {}", path.display()),
                Err(e) => eprintln!("conduit: could not save session {}: {e}", handle.id()),
            }
        }
        Ok(None) => {} // No engine ran; there is nothing to save.
        Err(e) => eprintln!("conduit: could not snapshot session {}: {e}", handle.id()),
    }
    Ok(())
}

fn run_session_command(command: SessionCommand) -> Result<()> {
    let store = session::Store::open()?;

    match command {
        SessionCommand::List => {
            let ids = store.list()?;
            if ids.is_empty() {
                println!("No sessions yet. Create one with `conduit probe <url> --session <id>`.");
                println!("They will live in {}", store.root().display());
                return Ok(());
            }
            println!("{}", store.root().display());
            println!();
            for id in ids {
                match store.load(&id) {
                    Ok(state) => println!("  {id}\n      {}", state.summary()),
                    Err(e) => println!("  {id}\n      unreadable: {e}"),
                }
            }
        }
        SessionCommand::Show { id, json: as_json } => {
            let state = store.load(&id)?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&state)?);
                return Ok(());
            }
            if state.is_empty() {
                println!("Session {id} is empty.");
                return Ok(());
            }
            println!("Session {id}");
            println!("  stored at  {}", store.dir_for(&id)?.display());
            println!("  created    {}", state.created);
            println!("  updated    {}", state.updated);
            println!("  {}", state.summary());
            for origin in state.origins() {
                println!();
                println!("  {origin}");
                if let Some(blob) = state.storage.get(origin) {
                    if let Some(local) = blob.get("localStorage").and_then(|v| v.as_object()) {
                        for key in local.keys() {
                            println!("      localStorage  {key}");
                        }
                    }
                    if let Some(dbs) = blob.get("databases").and_then(|v| v.as_array()) {
                        for db in dbs {
                            let name = db.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            for store_spec in
                                db.get("stores").and_then(|v| v.as_array()).unwrap_or(&vec![])
                            {
                                let store_name = store_spec
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let count = store_spec
                                    .get("records")
                                    .and_then(|v| v.as_array())
                                    .map(|r| r.len())
                                    .unwrap_or(0);
                                println!("      indexedDB     {name}/{store_name} ({count} record(s))");
                            }
                        }
                    }
                }
            }
        }
        SessionCommand::Rm { id } => {
            if store.remove(&id)? {
                println!("Deleted session {id}.");
            } else {
                println!("No session named {id}.");
            }
        }
    }

    Ok(())
}

fn print_json(session: &mcp::Session, target: &str) {
    let out = json!({
        "target": target,
        "engine": session.engine.as_str(),
        "prefix": session.prefix,
        "tools": session.tools(),
        "diagnostics": session.diagnostics,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}

fn print_report(session: &mcp::Session, target: &str) {
    let tools = session.tools();
    let d = &session.diagnostics;

    println!("{target}");
    println!(
        "  engine: {}   scripts: {}/{} ran   tools: {}",
        session.engine.as_str(),
        d.scripts_total.saturating_sub(d.scripts_failed),
        d.scripts_total,
        tools.len()
    );
    println!();

    if tools.is_empty() {
        println!("  No WebMCP tools found.");
        println!();
        // Say what the evidence supports rather than listing every
        // possibility. The page either looked for modelContext or it did not,
        // and it either called registerTool or it did not; those two facts
        // separate the cases that actually need different responses.
        if d.register_calls > 0 {
            println!(
                "  The page called registerTool {} time(s), and every call was",
                d.register_calls
            );
            println!("  rejected or later unregistered. Check the tool definitions:");
            println!("  `name` and `description` are required, and `name` is capped");
            println!("  at 128 characters.");
        } else if d.consume_calls > 0 {
            println!("  This page uses WebMCP as a client, not a provider: it called");
            println!(
                "  getTools/executeTool {} time(s) and registerTool none.",
                d.consume_calls
            );
            println!("  It drives other sites' tools rather than exposing its own, so");
            println!("  an empty list is the correct result here, not a failure.");
        } else if d.model_context_lookups > 0 {
            println!(
                "  The page read document.modelContext {} time(s) but never called",
                d.model_context_lookups
            );
            println!("  registerTool. So it IS a WebMCP site — it just never registered.");
            println!("  Either registration sits behind something that did not happen");
            println!("  here (framework hydration, a route change, user interaction), or");
            println!("  the page is a client that drives other sites' tools rather than");
            println!("  providing its own, in which case an empty list is correct.");
        } else if !d.script_errors.is_empty() || !d.unhandled_rejections.is_empty() {
            // Counting only evaluation failures misses the commonest case of
            // all: a script that never arrived. Telling someone their site
            // "probably does not use WebMCP" when its entry point 404'd is
            // worse than saying nothing.
            println!("  The page never looked for document.modelContext, and something");
            println!("  failed below. Registration was most likely lost with it.");
        } else if d.scripts_total == 0 {
            println!("  The page has no scripts, and no declarative <form toolname> tools.");
        } else {
            println!("  The page ran cleanly and never looked for document.modelContext,");
            println!("  so it most likely does not use WebMCP. Most sites do not, yet.");
        }
    } else {
        for t in tools {
            let name = t["name"].as_str().unwrap_or("?");
            let desc = t["description"].as_str().unwrap_or("");
            let ro = t["annotations"]["readOnlyHint"].as_bool().unwrap_or(false);
            let destructive = t["annotations"]["destructiveHint"]
                .as_bool()
                .unwrap_or(false);
            let flag = if destructive {
                " [consequential]"
            } else if ro {
                " [read-only]"
            } else {
                ""
            };
            println!("  {name}{flag}");
            // Keep the first line only; descriptions can be long.
            println!("      {}", desc.lines().next().unwrap_or("").trim());
        }
    }

    if !d.script_errors.is_empty() {
        println!();
        println!("  Script errors ({}):", d.script_errors.len());
        for e in d.script_errors.iter().take(10) {
            println!("    - {e}");
        }
        if d.script_errors.len() > 10 {
            println!("    ... and {} more", d.script_errors.len() - 10);
        }
    }

    if d.settle_exhausted {
        println!();
        println!("  The page was still scheduling work when the engine stopped.");
        println!("  Whatever it was building may simply not have finished, which");
        println!("  is different from finishing and registering nothing.");
    }

    if !d.unhandled_rejections.is_empty() {
        println!();
        println!(
            "  Unhandled promise rejections ({}):",
            d.unhandled_rejections.len()
        );
        for r in d.unhandled_rejections.iter().take(8) {
            println!("    - {}", r.lines().next().unwrap_or("").trim());
        }
        if d.unhandled_rejections.len() > 8 {
            println!("    ... and {} more", d.unhandled_rejections.len() - 8);
        }
        println!();
        println!("  An async chain gave up here. This is often the only trace of");
        println!("  a bootstrap that failed without throwing anywhere visible.");
    }

    if !d.unresolved_modules.is_empty() {
        println!();
        println!("  Modules the page imported that were not prefetched:");
        for m in d.unresolved_modules.iter().take(10) {
            println!("    - {m}");
        }
        if d.unresolved_modules.len() > 10 {
            println!("    ... and {} more", d.unresolved_modules.len() - 10);
        }
        println!();
        println!("  These are resolved at runtime, so static scanning cannot see");
        println!("  them. That is usually why a page loads without error but");
        println!("  registers nothing.");
    }

    if !d.missing_apis.is_empty() {
        println!();
        println!("  Platform APIs this page wanted that conduit does not implement:");
        for m in &d.missing_apis {
            println!("    - {m}");
        }
        println!();
        println!("  These are the gap between conduit and a real browser, measured");
        println!("  rather than guessed. Please report them as issues.");
    }
}
