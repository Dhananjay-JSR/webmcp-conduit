//! Named, persistent sessions.
//!
//! A session is the equivalent of a browser profile: the storage a site treats
//! as belonging to *this* visitor. Two sessions against the same URL are two
//! unrelated visitors, and neither can see the other's data — which is the
//! whole point. A single shared store would make "log in as A, then as B"
//! impossible, and that is the ordinary case for an agent driving a site on
//! behalf of more than one person.
//!
//! State lives in a plain directory of JSON, one directory per session, under
//! the platform's data directory. JSON rather than a database because a session
//! is something you will want to read, diff and delete by hand.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bumped when the on-disk shape changes incompatibly. An older conduit meeting
/// a newer session should say so rather than silently restore half of it.
const FORMAT_VERSION: u32 = 1;

/// Everything a session remembers between runs.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    /// Storage per origin, because that is how a browser keeps it. One session
    /// visiting two sites must not let either read the other's data, and a
    /// single flat blob would do exactly that the first time a session was
    /// pointed at a second URL.
    ///
    /// The values are opaque to Rust: `session.js` produces and consumes them,
    /// and it is the only thing that knows how a structured-clone value is
    /// encoded.
    #[serde(default)]
    pub storage: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub cookies: Vec<Cookie>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds. `None` is a session cookie, which a browser drops on exit —
    /// and so do we, by never writing it out.
    #[serde(default)]
    pub expires: Option<i64>,
    #[serde(default)]
    pub secure: bool,
}

impl State {
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty() && self.storage.values().all(storage_is_empty)
    }

    pub fn origins(&self) -> Vec<&str> {
        self.storage.keys().map(|s| s.as_str()).collect()
    }

    /// A one-line summary for `conduit session list`.
    pub fn summary(&self) -> String {
        let mut keys = 0usize;
        let mut databases = 0usize;
        let mut records = 0usize;

        for blob in self.storage.values() {
            keys += blob
                .get("localStorage")
                .and_then(|v| v.as_object())
                .map(|m| m.len())
                .unwrap_or(0);
            if let Some(dbs) = blob.get("databases").and_then(|v| v.as_array()) {
                databases += dbs.len();
                records += dbs
                    .iter()
                    .filter_map(|db| db.get("stores").and_then(|s| s.as_array()))
                    .flatten()
                    .filter_map(|store| store.get("records").and_then(|r| r.as_array()))
                    .map(|r| r.len())
                    .sum::<usize>();
            }
        }

        format!(
            "{} origin(s), {keys} localStorage key(s), {databases} database(s), {records} record(s), {} cookie(s)",
            self.storage.len(),
            self.cookies.len()
        )
    }
}

fn storage_is_empty(storage: &serde_json::Value) -> bool {
    let no_local = storage
        .get("localStorage")
        .and_then(|v| v.as_object())
        .map(|m| m.is_empty())
        .unwrap_or(true);
    let no_dbs = storage
        .get("databases")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    no_local && no_dbs
}

/// Where sessions live on disk.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The default location, overridable with `CONDUIT_SESSION_DIR` — which is
    /// what a hosted deployment needs, since it has no home directory worth the
    /// name.
    pub fn open() -> Result<Self> {
        if let Ok(dir) = std::env::var("CONDUIT_SESSION_DIR") {
            if !dir.trim().is_empty() {
                return Ok(Self {
                    root: PathBuf::from(dir),
                });
            }
        }
        Ok(Self {
            root: default_root()?.join("sessions"),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a session, or a blank one if it has never been used. A first run is
    /// not an error; it is how every session starts.
    pub fn load(&self, id: &str) -> Result<State> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(State {
                version: FORMAT_VERSION,
                ..Default::default()
            });
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading session from {}", path.display()))?;
        let state: State = serde_json::from_str(&raw)
            .with_context(|| format!("parsing session at {}", path.display()))?;

        if state.version > FORMAT_VERSION {
            return Err(anyhow!(
                "session {id} was written by a newer conduit (format {} > {FORMAT_VERSION}). \
                 Upgrade conduit, or delete it with `conduit session rm {id}`.",
                state.version
            ));
        }

        Ok(state)
    }

    pub fn save(&self, id: &str, state: &State) -> Result<PathBuf> {
        let path = self.path_for(id)?;
        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("session path has no parent"))?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        let body = serde_json::to_string_pretty(state).context("serialising session")?;

        // Written beside the target and renamed: a run interrupted mid-write
        // would otherwise leave a truncated file, and the next run would report
        // a corrupt session rather than simply an older one.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, body).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("replacing {}", path.display()))?;

        Ok(path)
    }

    pub fn list(&self) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut ids: Vec<String> = std::fs::read_dir(&self.root)
            .with_context(|| format!("reading {}", self.root.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter(|entry| entry.path().join("state.json").exists())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        ids.sort();
        Ok(ids)
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let dir = self.dir_for(id)?;
        if !dir.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(true)
    }

    pub fn dir_for(&self, id: &str) -> Result<PathBuf> {
        Ok(self.root.join(validate_id(id)?))
    }

    fn path_for(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir_for(id)?.join("state.json"))
    }
}

/// A session opened for one run: the store, the id, and the state read from
/// disk. Callers restore from it before the page boots and commit to it after.
pub struct Handle {
    store: Store,
    id: String,
    state: State,
}

impl Handle {
    pub fn open(id: &str) -> Result<Self> {
        let store = Store::open()?;
        let state = store.load(validate_id(id)?)?;
        Ok(Self {
            store,
            id: id.to_string(),
            state,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Used by `conduit session` and, shortly, by the hosted API.
    #[allow(dead_code)]
    pub fn state(&self) -> &State {
        &self.state
    }

    #[allow(dead_code)]
    pub fn path(&self) -> Result<PathBuf> {
        self.store.dir_for(&self.id)
    }

    /// The storage this session holds for one origin, as the JSON blob
    /// `session.js` expects — or `None` if this session has never visited it.
    pub fn storage_for(&self, origin: &str) -> Option<String> {
        let blob = self.state.storage.get(origin)?;
        if storage_is_empty(blob) {
            return None;
        }
        serde_json::to_string(blob).ok()
    }

    /// Record what the page ended up holding, and write it out.
    pub fn commit(
        &mut self,
        origin: &str,
        storage_json: &str,
        cookies: Vec<Cookie>,
    ) -> Result<PathBuf> {
        self.state.cookies = cookies;

        let blob: serde_json::Value = serde_json::from_str(storage_json)
            .context("parsing the storage snapshot the page produced")?;

        if storage_is_empty(&blob) {
            // A page that stored nothing should not erase what an earlier run
            // saved — it may simply have failed to boot this time.
            self.state.storage.entry(origin.to_string()).or_insert(blob);
        } else {
            self.state.storage.insert(origin.to_string(), blob);
        }

        let now = now_rfc3339();
        if self.state.created.is_empty() {
            self.state.created = now.clone();
        }
        self.state.updated = now;
        self.state.version = FORMAT_VERSION;

        self.store.save(&self.id, &self.state)
    }
}

/// Session ids become directory names, so they are restricted rather than
/// escaped. A hosted deployment will hand this whatever arrives in a query
/// string, and `../../` in a path segment is the oldest trick there is.
pub fn validate_id(id: &str) -> Result<&str> {
    if id.is_empty() || id.len() > 64 {
        return Err(anyhow!(
            "session id must be between 1 and 64 characters (got {})",
            id.len()
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "session id {id:?} may only contain letters, digits, '-' and '_'"
        ));
    }
    Ok(id)
}

pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_unix(secs)
}

/// A date without pulling in a date library. Sessions only need something
/// sortable and readable in a file a human will open.
fn format_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil-from-days, Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn default_root() -> Result<PathBuf> {
    // Resolved by hand rather than with a crate: it is three branches, and a
    // dependency in a published binary is a cost paid by everyone installing it.
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join("Library/Application Support/conduit"))
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(base).join("conduit"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            if !xdg.trim().is_empty() {
                return Ok(PathBuf::from(xdg).join("conduit"));
            }
        }
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".local/share/conduit"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_that_would_escape_the_store_are_rejected() {
        assert!(validate_id("ok-1_A").is_ok());
        for bad in ["", "../etc", "a/b", "a\\b", "a b", "."] {
            assert!(validate_id(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn a_session_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("conduit-test-{}", std::process::id()));
        let store = Store { root: dir.clone() };

        // Never used is not an error.
        let fresh = store.load("demo").unwrap();
        assert!(fresh.is_empty());

        let state = State {
            version: FORMAT_VERSION,
            created: now_rfc3339(),
            updated: now_rfc3339(),
            storage: BTreeMap::from([(
                "https://example.com".to_string(),
                serde_json::json!({"localStorage": {"a": "1"}, "databases": []}),
            )]),
            cookies: vec![],
        };
        store.save("demo", &state).unwrap();

        let read = store.load("demo").unwrap();
        assert_eq!(read.origins(), vec!["https://example.com"]);
        assert!(!read.is_empty());
        assert_eq!(store.list().unwrap(), vec!["demo".to_string()]);

        assert!(store.remove("demo").unwrap());
        assert!(!store.remove("demo").unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dates_are_formatted_without_a_date_crate() {
        assert_eq!(format_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
