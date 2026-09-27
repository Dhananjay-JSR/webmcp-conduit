//! ES module support.
//!
//! `<script type="module">` cannot be evaluated as a classic script — QuickJS
//! sees `import X from "..."` and reports `expecting '('`, because it is
//! looking for a dynamic `import(`. Modules need their own evaluation path and
//! a loader that can satisfy the import graph.
//!
//! QuickJS resolves imports synchronously, but fetching them is async, so the
//! whole graph is walked and fetched up front and handed to the loader as a
//! map.

use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;
use url::Url;

/// Extract the module specifiers a source file imports.
///
/// This is a scanner, not a parser. It over-matches on specifiers inside
/// strings or comments, which costs a wasted fetch at worst, and under-matches
/// nothing that matters for reaching `registerTool`.
pub fn import_specifiers(source: &str) -> Vec<String> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            // `import "x"`, `import d from "x"`, `import {a, b} from "x"`,
            // `import * as ns from "x"`. \bimport\b will not match
            // `important`, because there is no word boundary mid-word.
            //
            // The whitespace before `from` is optional on purpose: minified
            // bundles emit `import{X as y}from"z"`, and requiring a space
            // there makes every such import invisible to the graph walk.
            //
            // A static import only appears at statement position, so require
            // a plausible token before it. Without that, the word `import`
            // inside a string literal — a tool named "from-dynamic-import",
            // say — matches, and the scanner tries to fetch the rest of the
            // line as a module.
            Regex::new(
                r#"(?m)(?:^|[;{}()\[\]=>,]|\bexport\b)\s*import\b\s*(?:[\w${},*\s]+?\s*from\s*)?["']([^"']+)["']"#,
            )
            .unwrap(),
            // `export { x } from "y"` — only the re-export form has a specifier.
            Regex::new(r#"\bexport\b[^;]*?\bfrom\b\s*["']([^"']+)["']"#).unwrap(),
            // `export * from "y"`
            Regex::new(r#"\bexport\b\s*\*\s*(?:as\s+\w+\s*)?from\s*["']([^"']+)["']"#).unwrap(),
            // dynamic `import("x")`
            Regex::new(r#"\bimport\b\s*\(\s*["']([^"']+)["']"#).unwrap(),
        ]
    });

    let mut out: Vec<String> = Vec::new();
    for re in patterns {
        for caps in re.captures_iter(source) {
            let spec = caps[1].to_string();
            if !spec.is_empty() && !out.contains(&spec) {
                out.push(spec);
            }
        }
    }
    out
}

/// Walk and fetch the full import graph reachable from `entries`.
///
/// Returns a map of absolute URL to source. Modules that cannot be fetched are
/// reported rather than aborting: one unreachable dependency should not cost
/// the whole page.
pub async fn prefetch_graph(
    client: &reqwest::Client,
    entries: Vec<(String, String)>,
    preloaded: Vec<(String, String)>,
    page_origin: &str,
) -> (HashMap<String, String>, Vec<String>) {
    let mut sources: HashMap<String, String> = HashMap::new();
    let mut errors = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    let mut roots: HashSet<String> = HashSet::new();

    for (url, src) in entries {
        seen.insert(url.clone());
        roots.insert(url.clone());
        queue.push_back((url, src));
    }
    // Preloaded modules are real modules: they belong in the loader map, and
    // their own imports have to be walked too.
    for (url, src) in preloaded {
        if seen.insert(url.clone()) {
            queue.push_back((url, src));
        }
    }

    while let Some((url, src)) = queue.pop_front() {
        let base = match Url::parse(&url) {
            Ok(u) => u,
            Err(_) => continue,
        };

        for spec in import_specifiers(&src) {
            let Ok(resolved) = base.join(&spec) else {
                errors.push(format!("{url}: cannot resolve import '{spec}'"));
                continue;
            };
            let key = resolved.to_string();
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key.clone());

            match crate::fetch::script(client, &resolved, page_origin).await {
                Ok(dep) => queue.push_back((key, dep)),
                Err(e) => errors.push(format!("{key}: {e}")),
            }
        }

        // Roots are seeded only to discover what they import; the host
        // evaluates them directly. Registering a classic script as a module
        // would let an import resolve to something that was never a module.
        if !roots.contains(&url) {
            sources.insert(url, src);
        }
    }

    (sources, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_static_imports() {
        let src = r#"
            import DOMPurify from 'https://esm.sh/dompurify';
            import { a, b } from "./util.js";
            import "./side-effect.css";
        "#;
        let specs = import_specifiers(src);
        assert!(specs.contains(&"https://esm.sh/dompurify".to_string()));
        assert!(specs.contains(&"./util.js".to_string()));
        assert!(specs.contains(&"./side-effect.css".to_string()));
    }

    #[test]
    fn finds_re_exports() {
        let specs = import_specifiers(r#"export { x } from "./x.js";"#);
        assert_eq!(specs, vec!["./x.js"]);
    }

    #[test]
    fn ignores_plain_exports_and_identifiers() {
        // `export const` has no specifier; `important` is not `import`.
        let specs = import_specifiers("export const important = 'no';");
        assert!(specs.is_empty(), "got {specs:?}");
    }

    #[test]
    fn finds_minified_imports() {
        // Real-world CDN output has no whitespace to lean on. esm.sh serves
        // exactly this shape, and missing it means the module graph comes up
        // short and QuickJS fails to link.
        let src = r#"import{EventEmitter as g}from"/node/events.mjs"
import{ReadStream as f,WriteStream as p}from"/node/tty.mjs"
import"/node/buffer.mjs"
export{a as b}from"/node/util.mjs""#;
        let specs = import_specifiers(src);
        assert!(specs.contains(&"/node/events.mjs".to_string()), "got {specs:?}");
        assert!(specs.contains(&"/node/tty.mjs".to_string()), "got {specs:?}");
        assert!(specs.contains(&"/node/buffer.mjs".to_string()), "got {specs:?}");
        assert!(specs.contains(&"/node/util.mjs".to_string()), "got {specs:?}");
    }

    #[test]
    fn ignores_the_word_import_inside_strings() {
        // A tool named "from-dynamic-import" used to make the scanner capture
        // the remainder of the line as a specifier and fetch it.
        let src = r#"
            document.modelContext.registerTool({
              name: "from-dynamic-import",
              description: "Registered by a dynamically imported module",
              execute: function(){ return { ok: true }; }
            });
        "#;
        let specs = import_specifiers(src);
        assert!(specs.is_empty(), "false positives: {specs:?}");
    }

    #[test]
    fn deduplicates() {
        let specs = import_specifiers("import a from './x.js'; import b from './x.js';");
        assert_eq!(specs, vec!["./x.js"]);
    }
}
