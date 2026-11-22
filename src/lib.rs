//! Zed extension that starts `tsgo-watch-lsp`, a language server reporting TypeScript 7
//! diagnostics for every file of the workspace.

use std::fs;

use zed_extension_api::{
    self as zed, LanguageServerId, Result, Worktree, serde_json, settings::LspSettings,
};

const SERVER_ID: &str = "tsgo-watch";
const SERVER_BINARY: &str = "tsgo-watch-lsp";
const SERVER_DIR_PREFIX: &str = "tsgo-watch-lsp-";
const REPOSITORY: &str = "RedEagle-dh/zed-tsgo-watch";
const TYPESCRIPT: &str = "typescript";

/// The Rust target triple of the release binary for a platform.
fn release_target(os: zed::Os, arch: zed::Architecture) -> Option<&'static str> {
    Some(match (os, arch) {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
        (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
        (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-musl",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-musl",
        (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
        _ => return None,
    })
}

fn asset_name(target: &str) -> String {
    format!("{SERVER_BINARY}-{target}.gz")
}

fn executable(name: &str, os: zed::Os) -> String {
    match os {
        zed::Os::Windows => format!("{name}.exe"),
        _ => name.to_string(),
    }
}

/// Major version from the text of a `package.json`.
fn major_version(package_json: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let version = value.get("version")?.as_str()?;
    let digits: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Relative path of the native compiler inside an npm install of `typescript` >= 7.
fn compiler_binary(os: zed::Os, arch: zed::Architecture) -> Option<String> {
    let os_name = match os {
        zed::Os::Mac => "darwin",
        zed::Os::Linux => "linux",
        zed::Os::Windows => "win32",
    };
    let arch_name = match arch {
        zed::Architecture::Aarch64 => "arm64",
        zed::Architecture::X8664 => "x64",
        zed::Architecture::X86 => return None,
    };
    Some(format!(
        "node_modules/@typescript/typescript-{os_name}-{arch_name}/lib/{}",
        executable("tsc", os)
    ))
}

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn absolute(path: &str) -> Result<String> {
    Ok(std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(path)
        .to_string_lossy()
        .into_owned())
}

struct TsgoWatch {
    cached_server: Option<String>,
    cached_compiler: Option<String>,
}

impl TsgoWatch {
    /// A previously downloaded server binary in the extension work dir.
    fn installed_server(os: zed::Os) -> Option<String> {
        let mut dirs: Vec<String> = fs::read_dir(".")
            .ok()?
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| name.starts_with(SERVER_DIR_PREFIX))
            .collect();
        dirs.sort();
        dirs.into_iter()
            .rev()
            .map(|dir| format!("{dir}/{}", executable(SERVER_BINARY, os)))
            .find(|path| is_file(path))
    }

    fn download_server(&mut self, id: &LanguageServerId) -> Result<String> {
        let (os, arch) = zed::current_platform();
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let options = zed::GithubReleaseOptions {
            require_assets: true,
            pre_release: false,
        };
        let release = match zed::latest_github_release(REPOSITORY, options) {
            Ok(release) => release,
            Err(error) => {
                if let Some(path) = Self::installed_server(os) {
                    return Ok(path);
                }
                return Err(missing_server(&format!(
                    "the release lookup failed: {error}"
                )));
            }
        };
        let target = release_target(os, arch)
            .ok_or_else(|| missing_server("there is no release binary for this platform"))?;
        let name = asset_name(target);
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| {
                missing_server(&format!("release {} has no asset {name}", release.version))
            })?;
        let dir = format!("{SERVER_DIR_PREFIX}{}", release.version);
        let path = format!("{dir}/{}", executable(SERVER_BINARY, os));
        if !is_file(&path) {
            zed::set_language_server_installation_status(
                id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );
            fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
            // Only a complete download gets the final name; an interrupted one is replaced on
            // the next start.
            let partial = format!("{path}.download");
            let _ = fs::remove_file(&partial);
            zed::download_file(&asset.download_url, &partial, zed::DownloadedFileType::Gzip)
                .map_err(|e| missing_server(&format!("the download failed: {e}")))?;
            zed::make_file_executable(&partial)?;
            fs::rename(&partial, &path)
                .map_err(|e| format!("cannot move {partial} to {path}: {e}"))?;
            for entry in fs::read_dir(".").into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(SERVER_DIR_PREFIX) && name != dir {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::None,
        );
        Ok(path)
    }

    fn server_command(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<String> {
        if let Some(path) = worktree.which(SERVER_BINARY) {
            return Ok(path);
        }
        if let Some(path) = &self.cached_server
            && is_file(path)
        {
            return absolute(path);
        }
        let path = self.download_server(id)?;
        self.cached_server = Some(path.clone());
        absolute(&path)
    }

    /// Installs the latest `typescript` into the extension work dir, the way the `tsgo`
    /// extension does, and returns the absolute path of its native compiler.
    fn fallback_compiler(&mut self, id: &LanguageServerId) -> Option<String> {
        if let Some(path) = &self.cached_compiler
            && is_file(path)
        {
            return Some(path.clone());
        }
        let (os, arch) = zed::current_platform();
        let binary = compiler_binary(os, arch)?;
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        if let Ok(latest) = zed::npm_package_latest_version(TYPESCRIPT) {
            let installed = zed::npm_package_installed_version(TYPESCRIPT)
                .ok()
                .flatten();
            if !is_file(&binary) || installed.as_deref() != Some(latest.as_str()) {
                zed::set_language_server_installation_status(
                    id,
                    &zed::LanguageServerInstallationStatus::Downloading,
                );
                let _ = zed::npm_install_package(TYPESCRIPT, &latest);
            }
        }
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::None,
        );
        let path = absolute(&binary).ok().filter(|_| is_file(&binary))?;
        self.cached_compiler = Some(path.clone());
        Some(path)
    }
}

fn missing_server(reason: &str) -> String {
    format!(
        "tsgo-watch: cannot get {SERVER_BINARY} ({reason}). Set `lsp.{SERVER_ID}.binary.path` in your settings or put {SERVER_BINARY} on your PATH."
    )
}

/// Whether the worktree root has TypeScript 7 or the native preview installed.
fn worktree_has_compiler(worktree: &Worktree) -> bool {
    let typescript = worktree
        .read_text_file("node_modules/typescript/package.json")
        .ok()
        .and_then(|text| major_version(&text))
        .is_some_and(|major| major >= 7);
    typescript
        || worktree
            .read_text_file("node_modules/@typescript/native-preview/package.json")
            .is_ok()
}

impl zed::Extension for TsgoWatch {
    fn new() -> Self {
        Self {
            cached_server: None,
            cached_compiler: None,
        }
    }

    fn language_server_command(
        &mut self,
        id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command> {
        let binary = LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.binary);
        let env = binary
            .as_ref()
            .and_then(|b| b.env.clone())
            .map(|env| env.into_iter().collect())
            .unwrap_or_default();
        let (command, mut args) = match binary.as_ref().and_then(|b| b.path.clone()) {
            Some(path) => (path, binary.and_then(|b| b.arguments).unwrap_or_default()),
            None => (self.server_command(id, worktree)?, Vec::new()),
        };
        if !worktree_has_compiler(worktree)
            && let Some(compiler) = self.fallback_compiler(id)
        {
            args.push("--fallback-compiler".to_string());
            args.push(compiler);
        }
        Ok(zed::Command { command, args, env })
    }

    fn language_server_initialization_options(
        &mut self,
        _id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.initialization_options))
    }
}

zed::register_extension!(TsgoWatch);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_release_assets() {
        use zed::{Architecture as A, Os};
        let cases = [
            (
                Os::Mac,
                A::Aarch64,
                "tsgo-watch-lsp-aarch64-apple-darwin.gz",
            ),
            (Os::Mac, A::X8664, "tsgo-watch-lsp-x86_64-apple-darwin.gz"),
            (
                Os::Linux,
                A::X8664,
                "tsgo-watch-lsp-x86_64-unknown-linux-musl.gz",
            ),
            (
                Os::Linux,
                A::Aarch64,
                "tsgo-watch-lsp-aarch64-unknown-linux-musl.gz",
            ),
            (
                Os::Windows,
                A::X8664,
                "tsgo-watch-lsp-x86_64-pc-windows-msvc.gz",
            ),
            (
                Os::Windows,
                A::Aarch64,
                "tsgo-watch-lsp-aarch64-pc-windows-msvc.gz",
            ),
        ];
        for (os, arch, expected) in cases {
            assert_eq!(
                release_target(os, arch).map(asset_name).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(release_target(Os::Linux, A::X86), None);
        assert_eq!(executable(SERVER_BINARY, Os::Windows), "tsgo-watch-lsp.exe");
        assert_eq!(executable(SERVER_BINARY, Os::Linux), "tsgo-watch-lsp");
    }

    #[test]
    fn locates_the_fallback_compiler() {
        use zed::{Architecture as A, Os};
        assert_eq!(
            compiler_binary(Os::Linux, A::X8664).as_deref(),
            Some("node_modules/@typescript/typescript-linux-x64/lib/tsc")
        );
        assert_eq!(
            compiler_binary(Os::Windows, A::Aarch64).as_deref(),
            Some("node_modules/@typescript/typescript-win32-arm64/lib/tsc.exe")
        );
        assert_eq!(compiler_binary(Os::Mac, A::X86), None);
    }

    #[test]
    fn reads_typescript_major_version() {
        assert_eq!(major_version(r#"{ "version": "7.0.2" }"#), Some(7));
        assert_eq!(major_version(r#"{ "version": "5.9.3" }"#), Some(5));
        assert_eq!(
            major_version(r#"{ "version": "7.1.0-dev.20260101" }"#),
            Some(7)
        );
        assert_eq!(major_version("{}"), None);
        assert_eq!(major_version("not json"), None);
    }
}
