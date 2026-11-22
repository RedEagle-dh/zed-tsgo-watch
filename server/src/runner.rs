//! Runs one-shot compiler checks in the background.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::output::{self, Category, Output, RawDiagnostic};
use crate::paths::{in_node_modules, normalize, resolve};
use crate::state::CheckResult;

pub struct Job {
    pub config: PathBuf,
    pub compiler: PathBuf,
    pub build_info: PathBuf,
    /// Real workspace root; files outside it are dropped.
    pub root: PathBuf,
}

pub struct Done {
    pub config: PathBuf,
    pub duration: Duration,
    pub outcome: Result<Summary, String>,
}

pub struct Summary {
    pub result: CheckResult,
    pub errors: usize,
    pub listed: usize,
}

#[derive(Default)]
struct Children {
    next: u64,
    map: HashMap<u64, Child>,
}

#[derive(Clone)]
pub struct Runner {
    children: Arc<Mutex<Children>>,
    done: Sender<Done>,
}

impl Runner {
    pub fn new(done: Sender<Done>) -> Self {
        Self {
            children: Arc::default(),
            done,
        }
    }

    /// Starts a check. Spawning happens on the caller's thread; reading the output on a new one.
    pub fn start(&self, job: Job) -> std::io::Result<()> {
        let dir = job
            .config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut command = Command::new(&job.compiler);
        command
            .arg("-p")
            .arg(&job.config)
            .args([
                "--noEmit",
                "--pretty",
                "false",
                "--incremental",
                "--tsBuildInfoFile",
            ])
            .arg(&job.build_info)
            .arg("--listFiles")
            .current_dir(&dir)
            .env("PWD", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let started = Instant::now();
        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = {
            let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
            children.next += 1;
            let id = children.next;
            children.map.insert(id, child);
            id
        };
        let runner = self.clone();
        thread::spawn(move || {
            let stderr_reader = thread::spawn(move || {
                let mut text = Vec::new();
                if let Some(mut stderr) = stderr {
                    let _ = stderr.read_to_end(&mut text);
                }
                String::from_utf8_lossy(&text).into_owned()
            });
            let mut text = Vec::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_end(&mut text);
            }
            let status = runner.reap(id);
            let stderr = stderr_reader.join().unwrap_or_default();
            let outcome = match status {
                Some(status) if matches!(status.code(), Some(0..=2)) => {
                    let parsed = output::parse(&String::from_utf8_lossy(&text));
                    Ok(summarize(parsed, &dir, &job.config, &job.root))
                }
                Some(status) => Err(failure(status, &stderr)),
                None => Err("stopped".to_string()),
            };
            let _ = runner.done.send(Done {
                config: job.config,
                duration: started.elapsed(),
                outcome,
            });
        });
        Ok(())
    }

    /// Waits for a child after its output ended. `None` if it was killed by `kill_all`.
    fn reap(&self, id: u64) -> Option<ExitStatus> {
        let child = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .remove(&id);
        child.and_then(|mut child| child.wait().ok())
    }

    /// Kills every running compiler and waits for it to exit.
    pub fn kill_all(&self) {
        let children: Vec<Child> = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .drain()
            .map(|(_, c)| c)
            .collect();
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn failure(status: ExitStatus, stderr: &str) -> String {
    let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    if tail.is_empty() {
        format!("compiler exited with {status}")
    } else {
        format!("compiler exited with {status}: {}", tail.join(" | "))
    }
}

fn severity(category: Category) -> DiagnosticSeverity {
    match category {
        Category::Error => DiagnosticSeverity::ERROR,
        Category::Warning => DiagnosticSeverity::WARNING,
        Category::Suggestion => DiagnosticSeverity::HINT,
        Category::Message => DiagnosticSeverity::INFORMATION,
    }
}

/// TS2589, "Type instantiation is excessively deep and possibly infinite."
const INSTANTIATION_TOO_DEEP: u32 = 2589;

/// Whether a diagnostic comes from the incremental bookkeeping instead of the check.
///
/// Starting from an older build info, the compiler computes a declaration signature for every
/// changed file. For a file that exports a very large inferred type (an Elysia app), that can
/// report TS2589 although checking the file does not: a check without build info is clean, and
/// so is the next incremental run. Such a TS2589 has no location, because no node is being
/// checked. One that the check itself reports has a location and is kept.
fn is_bookkeeping_noise(diagnostic: &RawDiagnostic) -> bool {
    diagnostic.file.is_none() && diagnostic.code == INSTANTIATION_TOO_DEEP
}

/// Converts parsed output into a check result, with ranges that end at the end of the word at
/// the reported position.
pub fn summarize(mut parsed: Output, cwd: &Path, config: &Path, root: &Path) -> Summary {
    parsed.diagnostics.retain(|d| !is_bookkeeping_noise(d));
    let listed = parsed.files.len();
    let files = parsed
        .files
        .iter()
        .map(|f| normalize(Path::new(f)))
        .filter(|f| f.starts_with(root) && !in_node_modules(f))
        .collect();
    let errors = parsed
        .diagnostics
        .iter()
        .filter(|d| d.category == Category::Error)
        .count();
    let mut lines: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    let mut diagnostics = Vec::with_capacity(parsed.diagnostics.len());
    for raw in parsed.diagnostics {
        let (path, range) = match &raw.file {
            None => (config.to_path_buf(), Range::default()),
            Some(file) => {
                let path = resolve(cwd, file);
                let line = raw.line.saturating_sub(1);
                let start = raw.column.saturating_sub(1);
                let file_lines = lines
                    .entry(path.clone())
                    .or_insert_with(|| read_lines(&path));
                let end = file_lines
                    .as_ref()
                    .and_then(|l| l.get(line as usize))
                    .map(|text| output::word_end(text, start))
                    .unwrap_or(start);
                (
                    path,
                    Range::new(Position::new(line, start), Position::new(line, end)),
                )
            }
        };
        if !path.starts_with(root) {
            continue;
        }
        diagnostics.push((
            path,
            Diagnostic {
                range,
                severity: Some(severity(raw.category)),
                code: Some(NumberOrString::Number(raw.code as i32)),
                source: Some("tsgo-watch".to_string()),
                message: raw.message,
                ..Default::default()
            },
        ));
    }
    Summary {
        result: CheckResult { files, diagnostics },
        errors,
        listed,
    }
}

fn read_lines(path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Some(
        text.split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_resolves_paths_ranges_and_global_errors() {
        let root = std::env::temp_dir().join(format!("tsgo-watch-runner-{}", std::process::id()));
        let root = normalize(&root);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::write(
            root.join("app/src/main.ts"),
            "\u{feff}let x = 1;\r\nconst value: number = \"s\";\n",
        )
        .unwrap();
        let cwd = root.join("app");
        let config = cwd.join("tsconfig.json");
        let main = root.join("app/src/main.ts");
        let stdout = format!(
            "error TS5083: Cannot read file 'x'.\n\
src/main.ts(2,7): error TS2322: Type 'string' is not assignable to type 'number'.\n\
../../outside.ts(1,1): error TS1: outside the workspace\n\
src/main.ts(1,1): warning TS6133: unused\n\
{}\n{}\n/elsewhere/lib.d.ts\n",
            main.to_string_lossy(),
            root.join("node_modules/x/index.d.ts").to_string_lossy()
        );
        let summary = summarize(output::parse(&stdout), &cwd, &config, &root);
        assert_eq!(summary.errors, 3);
        assert_eq!(summary.listed, 3);
        assert_eq!(summary.result.files, vec![main.clone()]);
        let d = &summary.result.diagnostics;
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].0, config);
        assert_eq!(d[0].1.range, Range::default());
        assert_eq!(d[1].0, main);
        assert_eq!(
            d[1].1.range,
            Range::new(Position::new(1, 6), Position::new(1, 11))
        );
        assert_eq!(d[1].1.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d[1].1.code, Some(NumberOrString::Number(2322)));
        assert_eq!(d[1].1.source.as_deref(), Some("tsgo-watch"));
        assert_eq!(
            d[2].1.range,
            Range::new(Position::new(0, 0), Position::new(0, 3))
        );
        assert_eq!(d[2].1.severity, Some(DiagnosticSeverity::WARNING));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn summarize_drops_instantiation_depth_without_location() {
        let root = normalize(Path::new("/w"));
        let cwd = root.join("app");
        let config = cwd.join("tsconfig.json");
        let stdout = "\
error TS2589: Type instantiation is excessively deep and possibly infinite.
error TS2688: Cannot find type definition file for 'bun'.
src/main.ts(3,5): error TS2589: Type instantiation is excessively deep and possibly infinite.
";
        let summary = summarize(output::parse(stdout), &cwd, &config, &root);
        assert_eq!(summary.errors, 2);
        let d = &summary.result.diagnostics;
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].0, config);
        assert_eq!(d[0].1.code, Some(NumberOrString::Number(2688)));
        assert_eq!(d[1].0, root.join("app/src/main.ts"));
        assert_eq!(d[1].1.code, Some(NumberOrString::Number(2589)));
    }
}
