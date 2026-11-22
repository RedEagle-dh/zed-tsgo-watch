//! Path normalisation and conversion between paths and `file://` URIs.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use lsp_types::Uri;

/// Resolves `.` and `..` without touching the file system. On Windows it also turns `/` into `\`
/// and upper-cases the drive letter, so that paths printed by tsc compare equal to ours.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Prefix(prefix) => {
                let text = prefix.as_os_str().to_string_lossy();
                out.push(upper_drive(&text));
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn upper_drive(prefix: &str) -> String {
    let mut chars = prefix.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
            format!("{}{}", letter.to_ascii_uppercase(), &prefix[1..])
        }
        _ => prefix.to_string(),
    }
}

/// The canonical path without the `\\?\` prefix that Windows adds.
pub fn realpath(path: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    let text = real.to_string_lossy();
    let stripped = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        real.clone()
    };
    Some(normalize(&stripped))
}

/// Joins a path printed by tsc (relative to `cwd` or absolute) onto `cwd`.
pub fn resolve(cwd: &Path, printed: &str) -> PathBuf {
    normalize(&cwd.join(printed))
}

pub fn path_to_uri(path: &Path) -> Uri {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(text.len() + 8);
    out.push_str("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric() || b"-._~/:!$&'()*+,;=@".contains(&byte);
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    Uri::from_str(&out).unwrap_or_else(|_| Uri::from_str("file:///").expect("valid uri"))
}

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let text = uri.as_str();
    let rest = text.strip_prefix("file://")?;
    // Skip an authority ("file://host/..."); local files have an empty one.
    let path_part = &rest[rest.find('/')?..];
    let path_part = path_part.split(['?', '#']).next().unwrap_or(path_part);
    let decoded = percent_decode(path_part)?;
    let bytes = decoded.as_bytes();
    // "/C:/x" -> "C:/x"
    let local = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        &decoded[1..]
    } else {
        &decoded[..]
    };
    Some(normalize(Path::new(local)))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Maps paths between the workspace root as the client sees it and its real path, which is what
/// tsc prints when it runs inside it.
#[derive(Debug, Clone)]
pub struct RootMap {
    pub client: PathBuf,
    pub real: PathBuf,
}

impl RootMap {
    pub fn new(client: PathBuf) -> Self {
        let client = normalize(&client);
        let real = realpath(&client).unwrap_or_else(|| client.clone());
        Self { client, real }
    }

    pub fn to_real(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.client) {
            Ok(rest) if self.client != self.real => self.real.join(rest),
            _ => path.to_path_buf(),
        }
    }

    pub fn to_client(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.real) {
            Ok(rest) if self.client != self.real => self.client.join(rest),
            _ => path.to_path_buf(),
        }
    }

    pub fn contains_real(&self, path: &Path) -> bool {
        path.starts_with(&self.real)
    }
}

pub fn in_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dot_segments() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
        assert_eq!(
            resolve(Path::new("/w/apps/web"), "../../packages/ui/x.ts"),
            PathBuf::from("/w/packages/ui/x.ts")
        );
        assert_eq!(
            resolve(Path::new("/w"), "/abs/x.ts"),
            PathBuf::from("/abs/x.ts")
        );
    }

    #[cfg(unix)]
    #[test]
    fn uri_round_trip() {
        let path = PathBuf::from("/tmp/a b/äö/#x%.ts");
        let uri = path_to_uri(&path);
        assert_eq!(uri.as_str(), "file:///tmp/a%20b/%C3%A4%C3%B6/%23x%25.ts");
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    fn decodes_windows_uris() {
        let uri = Uri::from_str("file:///c%3A/Users/x/a.ts").unwrap();
        let path = uri_to_path(&uri).unwrap();
        if cfg!(windows) {
            assert_eq!(path, PathBuf::from(r"C:\Users\x\a.ts"));
        } else {
            assert_eq!(path, PathBuf::from("c:/Users/x/a.ts"));
        }
    }

    #[test]
    fn rejects_other_schemes() {
        assert_eq!(
            uri_to_path(&Uri::from_str("untitled:Untitled-1").unwrap()),
            None
        );
    }

    #[test]
    fn maps_between_client_and_real_root() {
        let map = RootMap {
            client: PathBuf::from("/tmp/w"),
            real: PathBuf::from("/private/tmp/w"),
        };
        assert_eq!(
            map.to_real(Path::new("/tmp/w/a.ts")),
            PathBuf::from("/private/tmp/w/a.ts")
        );
        assert_eq!(
            map.to_client(Path::new("/private/tmp/w/a.ts")),
            PathBuf::from("/tmp/w/a.ts")
        );
        assert_eq!(
            map.to_client(Path::new("/elsewhere/a.ts")),
            PathBuf::from("/elsewhere/a.ts")
        );
    }

    #[test]
    fn detects_node_modules() {
        assert!(in_node_modules(Path::new("/w/node_modules/x/index.d.ts")));
        assert!(!in_node_modules(Path::new("/w/src/node_modules.ts")));
    }
}
