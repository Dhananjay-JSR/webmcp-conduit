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
            Regex::new(r#"\bimport\b\s*(?:[\w${},*\s]+?\s+from\s*)?["']([^"']+)["']"#).unwrap(),
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
    page_origin: &str,
) -> (HashMap<String, String>, Vec<String>) {
    let mut sources: HashMap<String, String> = HashMap::new();
    let mut errors = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, String)> = VecDeque::new();

    for (url, src) in entries {
        seen.insert(url.clone());
        queue.push_back((url, src));
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

        sources.insert(url, src);
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
    fn deduplicates() {
        let specs = import_specifiers("import a from './x.js'; import b from './x.js';");
        assert_eq!(specs, vec!["./x.js"]);
    }
}
