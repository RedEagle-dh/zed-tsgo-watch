//! Locates the TypeScript 7 compiler of a project and the build info cache.

use std::path::{Path, PathBuf};

use crate::paths::realpath;

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Major version from a `package.json`, e.g. `7` for `"7.0.2"` or `"7.1.0-dev.1"`.
fn major_version(package_json: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(package_json).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let version = value.get("version")?.as_str()?;
    let digits: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Resolves `package` from `from` (a package directory) the way node does: `from/node_modules`,
/// then the `node_modules` of every ancestor. For a package inside `…/node_modules/x`, the
/// ancestor `…/node_modules` itself is also searched, which finds bun's isolated siblings.
fn resolve_from(from: &Path, package: &str) -> Option<PathBuf> {
    let mut dir = Some(from);
    while let Some(current) = dir {
        let candidate = if current.file_name().is_some_and(|n| n == "node_modules") {
            current.join(package)
        } else {
            current.join("node_modules").join(package)
        };
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

/// The native binary inside a TypeScript 7 or native-preview installation at `package_dir`.
fn native_binary(package_dir: &Path, platform_prefix: &str, binary: &str) -> Option<PathBuf> {
    let real = realpath(package_dir)?;
    let platform_package = format!(
        "@typescript/{platform_prefix}-{}-{}",
        os_name(),
        arch_name()
    );
    let platform_dir = resolve_from(&real, &platform_package)?;
    let path = platform_dir.join("lib").join(exe(binary));
    path.is_file().then_some(path)
}

/// The nearest TypeScript 7 (`typescript` >= 7, else `@typescript/native-preview`) from `dir`
/// upwards.
pub fn find_compiler(dir: &Path) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(d) = current {
        let modules = d.join("node_modules");
        if modules.is_dir() {
            let typescript = modules.join("typescript");
            if major_version(&typescript.join("package.json")).is_some_and(|major| major >= 7)
                && let Some(binary) = native_binary(&typescript, "typescript", "tsc")
            {
                return Some(binary);
            }
            let preview = modules.join("@typescript").join("native-preview");
            if preview.join("package.json").is_file()
                && let Some(binary) = native_binary(&preview, "native-preview", "tsgo")
            {
                return Some(binary);
            }
        }
        current = d.parent();
    }
    None
}

/// The per-user cache directory for build info files.
pub fn cache_dir() -> Option<PathBuf> {
    let env = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| home.join("Library").join("Caches"))
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|home| home.join(".cache")))
    }?;
    Some(base.join("tsgo-watch"))
}

/// A stable file name for the build info of `config`.
pub fn build_info_name(config: &Path) -> String {
    // FNV-1a, stable across builds and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in config.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = config
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect::<String>()
        })
        .unwrap_or_default();
    let stem = config
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{dir}-{stem}-{hash:016x}.tsbuildinfo")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tsgo-watch-compiler-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        realpath(&dir).unwrap()
    }

    fn package(dir: &Path, version: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("package.json"),
            format!(r#"{{ "name": "x", "version": "{version}" }}"#),
        )
        .unwrap();
    }

    fn binary(platform_dir: &Path, name: &str) -> PathBuf {
        let lib = platform_dir.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let path = lib.join(exe(name));
        fs::write(&path, "").unwrap();
        path
    }

    fn platform(prefix: &str) -> String {
        format!("{prefix}-{}-{}", os_name(), arch_name())
    }

    #[test]
    fn finds_hoisted_typescript() {
        let root = temp_dir("hoisted");
        package(&root.join("node_modules/typescript"), "7.0.2");
        let tsc = binary(
            &root
                .join("node_modules/@typescript")
                .join(platform("typescript")),
            "tsc",
        );
        fs::create_dir_all(root.join("packages/a")).unwrap();
        assert_eq!(find_compiler(&root.join("packages/a")), Some(tsc));
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn finds_bun_isolated_install() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("bun");
        let store = root.join("node_modules/.bun/typescript@7.0.2/node_modules");
        package(&store.join("typescript"), "7.0.2");
        let real_platform = root
            .join("node_modules/.bun")
            .join(format!("@typescript+{}@7.0.2", platform("typescript")));
        let real_platform_pkg = real_platform
            .join("node_modules/@typescript")
            .join(platform("typescript"));
        let tsc = binary(&real_platform_pkg, "tsc");
        fs::create_dir_all(store.join("@typescript")).unwrap();
        symlink(
            &real_platform_pkg,
            store.join("@typescript").join(platform("typescript")),
        )
        .unwrap();
        symlink(
            store.join("typescript"),
            root.join("node_modules/typescript"),
        )
        .unwrap();
        let found = find_compiler(&root).unwrap();
        assert_eq!(realpath(&found), Some(tsc));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finds_native_preview() {
        let root = temp_dir("preview");
        package(
            &root.join("node_modules/@typescript/native-preview"),
            "7.0.0-dev.20250101.1",
        );
        let tsgo = binary(
            &root
                .join("node_modules/@typescript")
                .join(platform("native-preview")),
            "tsgo",
        );
        assert_eq!(find_compiler(&root), Some(tsgo));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn skips_old_typescript_and_prefers_typescript_7() {
        let root = temp_dir("old");
        // Root: TypeScript 7 and native-preview; TypeScript 7 wins.
        package(&root.join("node_modules/typescript"), "7.0.2");
        let tsc = binary(
            &root
                .join("node_modules/@typescript")
                .join(platform("typescript")),
            "tsc",
        );
        package(
            &root.join("node_modules/@typescript/native-preview"),
            "7.0.0-dev.1",
        );
        binary(
            &root
                .join("node_modules/@typescript")
                .join(platform("native-preview")),
            "tsgo",
        );
        // Package: TypeScript 5 only, so the lookup continues upwards.
        package(&root.join("packages/old/node_modules/typescript"), "5.9.3");
        assert_eq!(find_compiler(&root.join("packages/old")), Some(tsc.clone()));
        assert_eq!(find_compiler(&root), Some(tsc));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nothing_found() {
        let root = temp_dir("none");
        package(&root.join("node_modules/typescript"), "5.9.3");
        // TypeScript 7 without its platform package.
        package(&root.join("a/node_modules/typescript"), "7.0.2");
        // The lookup walks up to the file system root, so only assert that nothing below `root`
        // is returned.
        if let Some(found) = find_compiler(&root.join("a")) {
            assert!(!found.starts_with(&root));
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn build_info_name_is_stable_and_distinct() {
        let a = build_info_name(Path::new("/w/apps/web/tsconfig.json"));
        let b = build_info_name(Path::new("/w/apps/web/tsconfig.app.json"));
        assert!(a.starts_with("web-tsconfig-") && a.ends_with(".tsbuildinfo"));
        assert_ne!(a, b);
        assert_eq!(a, build_info_name(Path::new("/w/apps/web/tsconfig.json")));
    }
}
