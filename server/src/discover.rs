//! Finds the tsconfig files of a workspace.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::paths::{normalize, realpath};

/// Every `tsconfig.json` under `root` (respecting `.gitignore`, skipping hidden directories and
/// `node_modules`) plus every config reachable through `references`. Paths are real paths.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    let mut queue = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != "node_modules")
        .build();
    for entry in walker.flatten() {
        if entry.file_name() == "tsconfig.json" && entry.file_type().is_some_and(|t| t.is_file()) {
            queue.push(entry.into_path());
        }
    }
    while let Some(config) = queue.pop() {
        let Some(config) = realpath(&config) else {
            continue;
        };
        if !config.starts_with(root) || !found.insert(config.clone()) {
            continue;
        }
        queue.extend(references(&config));
    }
    found.into_iter().collect()
}

/// The configs that `config` references, resolved the way tsc does: a path ending in `.json` is
/// the config itself, anything else is a directory containing `tsconfig.json`.
pub fn references(config: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(config) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(&text)) else {
        return Vec::new();
    };
    let dir = config.parent().unwrap_or(Path::new(""));
    let Some(list) = value.get("references").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|reference| reference.get("path")?.as_str())
        .map(|path| {
            let target = dir.join(path);
            if path.ends_with(".json") {
                normalize(&target)
            } else {
                normalize(&target.join("tsconfig.json"))
            }
        })
        .collect()
}

/// Turns JSON with comments and trailing commas into plain JSON.
pub fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                out.push(b);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b',' => {
                let mut j = i + 1;
                // Look past whitespace and comments for a closing bracket.
                loop {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'/') {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                    } else if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j < bytes.len()
                            && !(bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/'))
                        {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                if !matches!(bytes.get(j), Some(b'}') | Some(b']')) {
                    out.push(b',');
                }
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn strips_comments_and_trailing_commas() {
        let text = r#"{
  // line comment
  "a": "http://x // not a comment", /* block */
  "b": [1, 2, /* c */ ],
  "c": "quote \" inside, }",
}"#;
        let value: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["a"], "http://x // not a comment");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
        assert_eq!(value["c"], "quote \" inside, }");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tsgo-watch-discover-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        realpath(&dir).unwrap()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn finds_configs_follows_references_and_skips_ignored_dirs() {
        let root = temp_dir("refs");
        write(
            &root.join("tsconfig.json"),
            r#"{ "files": [], "references": [ { "path": "./tsconfig.app.json" }, { "path": "./tsconfig.node.json" }, ], // vite
            }"#,
        );
        write(
            &root.join("tsconfig.app.json"),
            r#"{ "include": ["src"], "references": [{ "path": "./lib" }] }"#,
        );
        write(
            &root.join("tsconfig.node.json"),
            r#"{ "include": ["vite.config.ts"] }"#,
        );
        write(&root.join("lib/tsconfig.json"), "{}");
        write(&root.join("packages/a/tsconfig.json"), "{}");
        write(&root.join("packages/a/tsconfig.build.json"), "{}");
        write(&root.join("node_modules/dep/tsconfig.json"), "{}");
        write(
            &root.join("packages/a/node_modules/dep/tsconfig.json"),
            "{}",
        );
        write(&root.join("dist/tsconfig.json"), "{}");
        write(&root.join(".gitignore"), "dist/\n");
        write(&root.join(".cache/tsconfig.json"), "{}");
        // A reference that leaves the workspace and one that does not exist.
        write(
            &root.join("packages/b/tsconfig.json"),
            r#"{ "references": [{ "path": "../../.." }, { "path": "../missing" }] }"#,
        );

        let found = discover(&root);
        let rel: Vec<_> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            rel,
            vec![
                "lib/tsconfig.json",
                "packages/a/tsconfig.json",
                "packages/b/tsconfig.json",
                "tsconfig.app.json",
                "tsconfig.json",
                "tsconfig.node.json",
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn broken_config_is_still_a_project() {
        let root = temp_dir("broken");
        write(&root.join("tsconfig.json"), "{ \"compilerOptions\": { ");
        assert_eq!(discover(&root), vec![root.join("tsconfig.json")]);
        fs::remove_dir_all(&root).unwrap();
    }
}
