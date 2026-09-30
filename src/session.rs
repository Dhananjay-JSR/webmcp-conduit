//! Named, persistent sessions.
//!
//! A session is the equivalent of a browser profile: the storage a site treats
//! as belonging to *this* visitor. Two sessions against the same URL are two
//! unrelated visitors, and neither can see the other's data — which is the
//! whole point. A single shared store would make "log in as A, then as B"
//! impossible, and that is the ordinary case for an agent driving a site on
//! behalf of more than one person.
//!
//! State lives in one directory per session under the platform's data
//! directory, overridable with `CONDUIT_SESSION_DIR`. Each holds a single
//! gzipped JSON document, `state.json.gz`.
//!
//! It is compressed because of what it holds. A session is a dump of
//! `localStorage` and every IndexedDB a site wrote, which is JSON describing
//! JSON — long repeated keys, base64 payloads, structured-clone envelopes
//! around small values. It is the most compressible thing conduit produces,
//! and the deployments that accumulate the most sessions are the ones least
//! able to spend disk on whitespace.
//!
//! There is deliberately no management surface here — no listing, no deletion.
//! conduit serves pages; what a fleet of sessions looks like and when one should
//! be discarded are questions for whatever operates it. Gzip is chosen over a
//! denser codec to keep that true: `ls` and `rm` are unaffected, and `cat`
//! becomes `zcat`, which is everywhere. Nothing needs conduit to read a
//! session.

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

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

    /// Read a session, or a blank one if it has never been used. A first run is
    /// not an error; it is how every session starts.
    pub fn load(&self, id: &str) -> Result<State> {
        let path = self.path_for(id)?;
        let legacy = self.legacy_path_for(id)?;

        // A session written before sessions were compressed is still a session.
        // It is read as it is, and the next save replaces it — no migration
        // step, no version to bump, nothing for an operator to run.
        let (path, raw) = if path.exists() {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("reading session from {}", path.display()))?;
            let mut raw = String::new();
            GzDecoder::new(file)
                .read_to_string(&mut raw)
                .with_context(|| format!("decompressing session at {}", path.display()))?;
            (path, raw)
        } else if legacy.exists() {
            let raw = std::fs::read_to_string(&legacy)
                .with_context(|| format!("reading session from {}", legacy.display()))?;
            (legacy, raw)
        } else {
            return Ok(State {
                version: FORMAT_VERSION,
                ..Default::default()
            });
        };

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

        // Compact rather than pretty. Whitespace is nearly free once gzipped,
        // but it is not free to produce, and `zcat state.json.gz | jq` reads
        // this better than the pretty printer did.
        let body = serde_json::to_vec(state).context("serialising session")?;

        // Written beside the target and renamed: a run interrupted mid-write
        // would otherwise leave a truncated file, and the next run would report
        // a corrupt session rather than simply an older one.
        let temp = path.with_extension("gz.tmp");
        let file =
            std::fs::File::create(&temp).with_context(|| format!("writing {}", temp.display()))?;
        let mut encoder = GzEncoder::new(file, Compression::default());
        encoder
            .write_all(&body)
            .with_context(|| format!("compressing into {}", temp.display()))?;
        encoder
            .finish()
            .with_context(|| format!("finishing {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("replacing {}", path.display()))?;

        // Only once the replacement is in place. Removing it earlier would
        // turn an interrupted save into a lost session rather than an older
        // one, which is the failure this whole dance exists to avoid.
        let legacy = self.legacy_path_for(id)?;
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }

        Ok(path)
    }

    pub fn dir_for(&self, id: &str) -> Result<PathBuf> {
        Ok(self.root.join(validate_id(id)?))
    }

    fn path_for(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir_for(id)?.join("state.json.gz"))
    }

    /// Where sessions lived before they were compressed. Read, never written.
    fn legacy_path_for(&self, id: &str) -> Result<PathBuf> {
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

        // Never used is not an error: it is how every session starts.
        assert!(store.load("demo").unwrap().storage.is_empty());

        let state = State {
            version: FORMAT_VERSION,
            created: now_rfc3339(),
            updated: now_rfc3339(),
            storage: BTreeMap::from([(
                "https://example.com".to_string(),
                serde_json::json!({"localStorage": {"a": "1"}, "databases": []}),
            )]),
            cookies: vec![Cookie {
                name: "sid".into(),
                value: "abc".into(),
                domain: "example.com".into(),
                path: "/".into(),
                expires: Some(i64::MAX),
                secure: true,
            }],
        };
        store.save("demo", &state).unwrap();

        let read = store.load("demo").unwrap();
        assert_eq!(
            read.storage.keys().collect::<Vec<_>>(),
            vec!["https://example.com"]
        );
        assert_eq!(read.cookies.len(), 1);
        assert_eq!(read.cookies[0].value, "abc");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_session_written_before_compression_is_still_readable() {
        let dir = std::env::temp_dir().join(format!("conduit-legacy-{}", std::process::id()));
        let store = Store { root: dir.clone() };
        let session = dir.join("old");
        std::fs::create_dir_all(&session).unwrap();

        // Exactly what an older conduit left behind: uncompressed, pretty.
        std::fs::write(
            session.join("state.json"),
            serde_json::to_string_pretty(&State {
                version: FORMAT_VERSION,
                storage: BTreeMap::from([(
                    "https://example.com".to_string(),
                    serde_json::json!({"localStorage": {"token": "kept"}, "databases": []}),
                )]),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

        let read = store.load("old").unwrap();
        assert_eq!(
            read.storage["https://example.com"]["localStorage"]["token"], "kept",
            "an upgrade must not silently empty a session"
        );

        // Saving replaces it, and does not leave the old copy behind to be
        // read again by a conduit that would then disagree about the contents.
        store.save("old", &read).unwrap();
        assert!(session.join("state.json.gz").exists());
        assert!(!session.join("state.json").exists());
        assert_eq!(
            store.load("old").unwrap().storage["https://example.com"]["localStorage"]["token"],
            "kept"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sessions_are_stored_compressed() {
        let dir = std::env::temp_dir().join(format!("conduit-gzip-{}", std::process::id()));
        let store = Store { root: dir.clone() };

        // Shaped like what a page actually writes: many records under long,
        // repeated keys. This is the case compression is for.
        let records: Vec<serde_json::Value> = (0..200)
            .map(|i| {
                serde_json::json!({
                    "id": i,
                    "createdAt": "2026-09-30T00:00:00.000Z",
                    "updatedAt": "2026-09-30T00:00:00.000Z",
                    "description": "a note whose text repeats across every record",
                })
            })
            .collect();
        let state = State {
            version: FORMAT_VERSION,
            storage: BTreeMap::from([(
                "https://example.com".to_string(),
                serde_json::json!({"localStorage": {}, "databases": [{"name": "notes", "records": records}]}),
            )]),
            ..Default::default()
        };

        store.save("big", &state).unwrap();
        let path = dir.join("big/state.json.gz");
        let on_disk = std::fs::metadata(&path).unwrap().len();
        let uncompressed = serde_json::to_vec(&state).unwrap().len() as u64;

        // Gzip's own framing, so `zcat` and every other tool can read it.
        let head = std::fs::read(&path).unwrap();
        assert_eq!(&head[..2], &[0x1f, 0x8b], "not a gzip file");

        assert!(
            on_disk * 5 < uncompressed,
            "expected better than 5x on repetitive JSON, got {uncompressed} -> {on_disk}"
        );
        assert_eq!(
            store.load("big").unwrap().storage["https://example.com"]["databases"][0]["records"]
                .as_array()
                .unwrap()
                .len(),
            200
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn storage_is_offered_only_to_the_origin_that_wrote_it() {
        let dir = std::env::temp_dir().join(format!("conduit-origin-{}", std::process::id()));
        let store = Store { root: dir.clone() };
        let mut handle = Handle {
            store,
            id: "demo".into(),
            state: State::default(),
        };

        handle
            .commit(
                "https://a.example",
                r#"{"localStorage":{"token":"secret"},"databases":[]}"#,
                Vec::new(),
            )
            .unwrap();

        assert!(handle.storage_for("https://a.example").is_some());
        // The same session visiting a second site must not hand it the first
        // site's storage — that is the leak per-origin keying exists to stop.
        assert!(handle.storage_for("https://b.example").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dates_are_formatted_without_a_date_crate() {
        assert_eq!(format_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
