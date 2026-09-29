//! Engines, and the threads they have to live on.
//!
//! A QuickJS context is not `Send`: it cannot move between threads and cannot
//! be shared. Every MCP server abstraction, including the official SDK's
//! `ServerHandler`, requires `Send + Sync + 'static`. Those two facts decide the
//! architecture — an engine owns a thread for its whole life, and the rest of
//! the program talks to it over a channel.
//!
//! That also makes the concurrency honest. Two callers working on two sites
//! genuinely run at once, rather than taking turns inside one interpreter.

use crate::mcp;
use crate::session;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Mutex};

/// How long an engine may sit unused before it is retired. Long enough that a
/// conversation with pauses in it keeps its page, short enough that an idle
/// deployment is not holding a JavaScript heap per visitor.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A page load is slow, and a caller should be told so rather than left hanging.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// What a loaded page can tell us about itself, for the MCP handshake.
#[derive(Clone)]
pub struct Description {
    pub engine: String,
    pub prefix: String,
    pub tool_count: usize,
}

/// The outcome of a tool call: the text to hand back, and whether it failed.
///
/// A failing tool is a normal result the model should see and adapt to, not a
/// protocol error, so the failure travels as data rather than as `Err`.
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

enum Command {
    Describe(oneshot::Sender<Description>),
    ListTools(oneshot::Sender<Vec<Value>>),
    CallTool {
        name: String,
        arguments: Value,
        reply: oneshot::Sender<ToolOutcome>,
    },
}

/// A handle to one running engine. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
}

impl Handle {
    pub async fn describe(&self) -> Result<Description> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Describe(tx))?;
        rx.await.map_err(|_| anyhow!("the engine stopped"))
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::ListTools(tx))?;
        rx.await.map_err(|_| anyhow!("the engine stopped"))
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutcome> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::CallTool {
            name: name.to_string(),
            arguments,
            reply: tx,
        })?;
        rx.await.map_err(|_| anyhow!("the engine stopped"))
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("the engine for this page has stopped"))
    }
}

/// Start an engine on its own thread and wait until the page has loaded.
///
/// The thread gets a deep stack for the same reason the CLI's does: a framework
/// reconciler recurses once per node, and React turns the resulting overflow
/// into a blank page rather than a crash.
pub async fn spawn(target: String, session_id: Option<String>, no_scripts: bool) -> Result<Handle> {
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel::<Command>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("conduit-engine".into())
        .stack_size(crate::ENGINE_STACK)
        .spawn(move || {
            // Its own runtime, because this thread owns a `!Send` engine and
            // cannot borrow anyone else's.
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("starting a runtime: {e}")));
                    return;
                }
            };

            runtime.block_on(async move {
                let mut handle = match session_id.as_deref().map(session::Handle::open) {
                    Some(Ok(handle)) => Some(handle),
                    Some(Err(e)) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                    None => None,
                };

                let mut page = match mcp::Session::load(&target, !no_scripts, handle.as_ref()).await
                {
                    Ok(page) => page,
                    Err(e) => {
                        // `{:#}` walks the whole chain. `to_string()` keeps only
                        // the outermost context, so "fetching <url>" would
                        // arrive without the reason it failed — a refused
                        // connection and a 500 from the site read identically.
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };

                if ready_tx.send(Ok(())).is_err() {
                    return; // The caller gave up while the page was loading.
                }

                while let Some(command) = commands_rx.recv().await {
                    match command {
                        Command::Describe(reply) => {
                            let _ = reply.send(Description {
                                engine: page.engine.as_str().to_string(),
                                prefix: page.prefix.clone(),
                                tool_count: page.tools().len(),
                            });
                        }
                        Command::ListTools(reply) => {
                            let _ = reply.send(page.tools().to_vec());
                        }
                        Command::CallTool {
                            name,
                            arguments,
                            reply,
                        } => {
                            let untrusted = page.is_untrusted(&name);
                            let outcome = match page.call(&name, &arguments).await {
                                Ok(raw) => ToolOutcome {
                                    text: crate::tool::fence_untrusted(&raw, untrusted),
                                    is_error: false,
                                },
                                Err(e) => ToolOutcome {
                                    text: e.to_string(),
                                    is_error: true,
                                },
                            };

                            // Committed per call rather than only at shutdown. A
                            // hosted process can be killed without warning, and
                            // losing a caller's work because a container was
                            // recycled is not acceptable.
                            commit(handle.as_mut(), &mut page);
                            let _ = reply.send(outcome);
                        }
                    }
                }

                // The channel closed: retired for idleness, or shutting down.
                commit(handle.as_mut(), &mut page);
            });
        })
        .context("spawning an engine thread")?;

    match tokio::time::timeout(LOAD_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(Handle {
            commands: commands_tx,
        }),
        Ok(Ok(Err(e))) => Err(anyhow!("{e}")),
        Ok(Err(_)) => Err(anyhow!("the engine stopped while loading the page")),
        Err(_) => Err(anyhow!(
            "timed out after {}s loading the page",
            LOAD_TIMEOUT.as_secs()
        )),
    }
}

pub fn commit(handle: Option<&mut session::Handle>, page: &mut mcp::Session) {
    let Some(handle) = handle else { return };
    let origin = page.origin();
    let cookies = page.cookies();

    let storage = match page.snapshot() {
        Ok(Some(storage)) => storage,
        Ok(None) => "{}".to_string(),
        Err(e) => {
            tracing::warn!(target: "conduit", "snapshotting session {}: {e}", handle.id());
            "{}".to_string()
        }
    };

    if let Err(e) = handle.commit(&origin, &storage, cookies) {
        tracing::warn!(target: "conduit", "saving session {}: {e}", handle.id());
    }
}

/// Live engines, keyed by what makes them distinct: the page and the session
/// looking at it.
#[derive(Clone, Default)]
pub struct Pool {
    engines: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Clone)]
struct Entry {
    handle: Handle,
    last_used: Instant,
}

impl Pool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn key(target: &str, session_id: Option<&str>) -> String {
        format!("{target}#{}", session_id.unwrap_or(""))
    }

    /// Find a live engine for this page, or start one.
    pub async fn get_or_spawn(
        &self,
        target: &str,
        session_id: Option<&str>,
        no_scripts: bool,
    ) -> Result<Handle> {
        let key = Self::key(target, session_id);

        {
            let mut engines = self.engines.lock().await;
            if let Some(entry) = engines.get_mut(&key) {
                if !entry.handle.commands.is_closed() {
                    entry.last_used = Instant::now();
                    return Ok(entry.handle.clone());
                }
                // The worker died — a page that threw during load, most likely.
                // Drop it, so the next line starts a fresh one rather than
                // handing out a channel nobody is listening to.
                engines.remove(&key);
            }
        }

        let handle = spawn(
            target.to_string(),
            session_id.map(str::to_string),
            no_scripts,
        )
        .await?;

        let mut engines = self.engines.lock().await;
        engines.insert(
            key,
            Entry {
                handle: handle.clone(),
                last_used: Instant::now(),
            },
        );
        Ok(handle)
    }

    /// Retire engines that have gone quiet.
    ///
    /// Dropping the handle closes the channel, which ends the worker's loop,
    /// which writes its session back on the way out. Retirement is not a way to
    /// lose work.
    pub fn start_reaper(&self) {
        let engines = self.engines.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                let mut engines = engines.lock().await;
                engines.retain(|key, entry| {
                    let alive = entry.last_used.elapsed() < IDLE_TIMEOUT;
                    if !alive {
                        tracing::info!(target: "conduit", "retiring idle engine {key}");
                    }
                    alive
                });
            }
        });
    }
}
