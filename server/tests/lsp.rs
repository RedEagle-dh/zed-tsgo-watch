//! Starts the real server over stdio against fixture workspaces and checks what it publishes.
//!
//! Needs a TypeScript 7 compiler binary in `TSGO_WATCH_TEST_TSC`. Without it the tests are
//! skipped, unless `CI` is set, in which case they fail.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

fn compiler() -> Option<PathBuf> {
    match std::env::var_os("TSGO_WATCH_TEST_TSC") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ if std::env::var_os("CI").is_some() => {
            panic!("TSGO_WATCH_TEST_TSC must point to a TypeScript 7 tsc binary in CI")
        }
        _ => {
            eprintln!("skipping: TSGO_WATCH_TEST_TSC is not set");
            None
        }
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("tsgo-watch-it-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Every file below `dir` with its contents.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                out.insert(
                    entry.path().strip_prefix(dir).unwrap().to_path_buf(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    out
}

fn uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:!$&'()*+,;=@".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

struct Workspace {
    _temp: TempDir,
    _cache: TempDir,
    root: PathBuf,
    cache: PathBuf,
}

impl Workspace {
    fn new(fixture: &str) -> Self {
        let temp = TempDir::new(fixture);
        let cache = TempDir::new(&format!("{fixture}-cache"));
        let root = temp.0.join(fixture);
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
            &root,
        );
        let cache_path = cache.0.clone();
        Self {
            _temp: temp,
            _cache: cache,
            root,
            cache: cache_path,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    root: PathBuf,
    next_id: i64,
    /// Latest diagnostics per URI.
    diagnostics: HashMap<String, Vec<Value>>,
    /// Number of publishDiagnostics per URI.
    publishes: HashMap<String, usize>,
    progress_ends: usize,
    show_messages: Vec<String>,
    registrations: Vec<Value>,
    refresh_requests: usize,
}

impl Client {
    fn start(workspace: &Workspace, options: Value) -> Self {
        Self::start_with(workspace, options, false)
    }

    /// `refresh` advertises support for `workspace/diagnostic/refresh`, as Zed does.
    fn start_with(workspace: &Workspace, options: Value, refresh: bool) -> Self {
        let tsc = compiler().expect("compiler");
        let mut child = Command::new(env!("CARGO_BIN_EXE_tsgo-watch-lsp"))
            .arg("--fallback-compiler")
            .arg(&tsc)
            .env("XDG_CACHE_HOME", &workspace.cache)
            .env("HOME", &workspace.cache)
            .env("LOCALAPPDATA", &workspace.cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start server");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, messages) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length: ") {
                        length = value.parse::<usize>().ok();
                    }
                }
                let mut body = vec![0; length.expect("content length")];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let value: Value = serde_json::from_slice(&body).expect("json");
                if sender.send(value).is_err() {
                    return;
                }
            }
        });
        let mut client = Client {
            child,
            stdin,
            messages,
            root: workspace.root.clone(),
            next_id: 0,
            diagnostics: HashMap::new(),
            publishes: HashMap::new(),
            progress_ends: 0,
            show_messages: Vec::new(),
            registrations: Vec::new(),
            refresh_requests: 0,
        };
        let id = client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": uri(&workspace.root),
                "capabilities": {
                    "window": { "workDoneProgress": true },
                    "workspace": {
                        "didChangeWatchedFiles": { "dynamicRegistration": true },
                        "diagnostics": { "refreshSupport": refresh }
                    }
                },
                "initializationOptions": options
            }),
        );
        let response = client.wait_response(id);
        assert_eq!(
            response["result"]["capabilities"]["textDocumentSync"]["openClose"],
            true
        );
        assert_eq!(
            response["result"]["capabilities"]["diagnosticProvider"]["identifier"],
            "tsgo-watch"
        );
        client.notify("initialized", json!({}));
        client
    }

    fn send(&mut self, value: Value) {
        let body = value.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Handles one message. Returns it, or `None` on timeout.
    fn pump(&mut self, deadline: Instant) -> Option<Value> {
        let wait = deadline.saturating_duration_since(Instant::now());
        let message = match self.messages.recv_timeout(wait) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => panic!("server closed its output"),
        };
        if let (Some(id), Some(method)) = (
            message.get("id"),
            message.get("method").and_then(Value::as_str),
        ) {
            if method == "workspace/diagnostic/refresh" {
                self.refresh_requests += 1;
            }
            if method == "client/registerCapability" {
                self.registrations.push(message["params"].clone());
            }
            let id = id.clone();
            self.send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
            return Some(message);
        }
        match message.get("method").and_then(Value::as_str) {
            Some("textDocument/publishDiagnostics") => {
                let uri = message["params"]["uri"].as_str().unwrap().to_string();
                let list = message["params"]["diagnostics"].as_array().unwrap().clone();
                *self.publishes.entry(uri.clone()).or_default() += 1;
                self.diagnostics.insert(uri, list);
            }
            Some("$/progress") if message["params"]["value"]["kind"] == "end" => {
                self.progress_ends += 1
            }
            Some("window/showMessage") => self
                .show_messages
                .push(message["params"]["message"].as_str().unwrap().to_string()),
            _ => {}
        }
        Some(message)
    }

    fn wait_response(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let message = self
                .pump(deadline)
                .expect("timed out waiting for a response");
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return message;
            }
        }
    }

    fn wait_until(&mut self, what: &str, condition: impl Fn(&Client) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !condition(self) {
            if self.pump(deadline).is_none() {
                panic!(
                    "timed out waiting for {what}; diagnostics: {:#?}",
                    self.summary()
                );
            }
        }
    }

    /// Waits for the end of the next round of checks.
    fn wait_checks(&mut self) {
        let ends = self.progress_ends;
        self.wait_until("checks to finish", |c| c.progress_ends > ends);
    }

    /// Processes messages for a while without expecting anything.
    fn settle(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while self.pump(deadline).is_some() {}
    }

    fn messages_for(&self, rel: &str) -> Vec<String> {
        self.diagnostics
            .get(&uri(&self.root.join(rel)))
            .map(|list| {
                list.iter()
                    .map(|d| {
                        d["message"]
                            .as_str()
                            .unwrap()
                            .lines()
                            .next()
                            .unwrap()
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn diagnostics_for(&self, rel: &str) -> Vec<Value> {
        self.diagnostics
            .get(&uri(&self.root.join(rel)))
            .cloned()
            .unwrap_or_default()
    }

    fn summary(&self) -> BTreeMap<String, Vec<String>> {
        self.diagnostics
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(uri, list)| {
                (
                    uri.clone(),
                    list.iter()
                        .map(|d| d["message"].as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect()
    }

    /// Relative paths of every file that currently has diagnostics.
    fn files_with_diagnostics(&self) -> Vec<String> {
        let prefix = format!("{}/", uri(&self.root));
        let mut files: Vec<String> = self
            .diagnostics
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(uri, _)| uri.strip_prefix(&prefix).unwrap_or(uri).to_string())
            .collect();
        files.sort();
        files
    }

    fn change(&mut self, rel: &str, kind: u32) {
        let uri = uri(&self.root.join(rel));
        self.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": uri, "type": kind }] }),
        );
    }

    fn open(&mut self, rel: &str) {
        let path = self.root.join(rel);
        let text = std::fs::read_to_string(&path).unwrap();
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri(&path), "languageId": "typescript", "version": 1, "text": text } }),
        );
    }

    /// Pulls diagnostics for a file, as Zed does for files shown in an editor.
    fn show(&mut self, rel: &str) {
        let path = self.root.join(rel);
        let id = self.request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri(&path) } }),
        );
        let response = self.wait_response(id);
        assert_eq!(response["result"], json!({ "kind": "full", "items": [] }));
    }

    fn close(&mut self, rel: &str) {
        let path = self.root.join(rel);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri(&path) } }),
        );
    }

    fn shutdown(mut self) {
        let id = self.request("shutdown", Value::Null);
        self.wait_response(id);
        self.notify("exit", Value::Null);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "server exit status {status}");
                return;
            }
            assert!(Instant::now() < deadline, "server did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const CHANGED: u32 = 2;
const CREATED: u32 = 1;

#[test]
fn single_config_reports_unopened_files() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("single");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(client.files_with_diagnostics(), vec!["src/broken.ts"]);
    let diagnostic = &client.diagnostics_for("src/broken.ts")[0];
    // Column 30 counts UTF-16 code units; the range covers the identifier `n`.
    assert_eq!(
        diagnostic["range"],
        json!({ "start": { "line": 0, "character": 29 }, "end": { "line": 0, "character": 30 } })
    );
    assert_eq!(diagnostic["severity"], 1);
    assert_eq!(diagnostic["code"], 2322);
    assert_eq!(diagnostic["source"], "tsgo-watch");
    assert_eq!(client.registrations.len(), 1, "watched files registration");
    client.shutdown();
}

#[test]
fn monorepo_with_single_root_config() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("mono");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(
        client.files_with_diagnostics(),
        vec!["packages/app/src/main.ts", "packages/core/src/index.ts"]
    );
    client.shutdown();
}

#[test]
fn workspace_packages_report_each_file_once() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("ws");
    let before = snapshot(&workspace.root);
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(
        client.files_with_diagnostics(),
        vec![
            "apps/web/src/main.ts",
            "packages/ui/src/index.ts",
            "scripts/tool.ts"
        ]
    );
    // The package error is reported by three projects but published once.
    assert_eq!(
        client.messages_for("packages/ui/src/index.ts"),
        vec!["Type 'string' is not assignable to type 'number'."]
    );
    assert_eq!(client.messages_for("apps/web/src/main.ts").len(), 3);
    let continuation = client.diagnostics_for("apps/web/src/main.ts")[2]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        continuation,
        "Type 'B' is not assignable to type 'A'.\nTypes have separate declarations of a private property 'p'."
    );
    // The root-level script is only covered by the root config.
    assert_eq!(
        client.messages_for("scripts/tool.ts"),
        vec!["Type 'number' is not assignable to type 'string'."]
    );
    client.shutdown();
    assert_eq!(
        snapshot(&workspace.root),
        before,
        "nothing written into the workspace"
    );
}

#[test]
fn vite_style_references() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("vite");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(
        client.files_with_diagnostics(),
        vec!["src/main.ts", "vite.config.ts"]
    );
    client.shutdown();
    assert!(
        !workspace.path("node_modules").exists(),
        "tsBuildInfoFile from the config is overridden"
    );
}

#[test]
fn changes_on_disk_update_and_clear_diagnostics() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("single");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert!(client.messages_for("src/clean.ts").is_empty());

    // Break a file that the other one imports.
    workspace.write(
        "src/clean.ts",
        "export function add(a: number, b: number): string {\n  return a + b;\n}\n",
    );
    client.change("src/clean.ts", CHANGED);
    client.wait_until("new errors", |c| {
        !c.messages_for("src/clean.ts").is_empty() && !c.messages_for("src/uses.ts").is_empty()
    });

    // Fix it again.
    workspace.write(
        "src/clean.ts",
        "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
    );
    client.change("src/clean.ts", CHANGED);
    client.wait_until("cleared errors", |c| {
        c.messages_for("src/clean.ts").is_empty() && c.messages_for("src/uses.ts").is_empty()
    });
    assert_eq!(client.files_with_diagnostics(), vec!["src/broken.ts"]);

    // A new file is picked up by the include glob.
    workspace.write("src/created.ts", "export const created: boolean = 0;\n");
    client.change("src/created.ts", CREATED);
    client.wait_until("created file", |c| {
        !c.messages_for("src/created.ts").is_empty()
    });
    client.shutdown();
}

#[test]
fn package_change_breaks_dependent_package() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("ws");
    let before = snapshot(&workspace.root);
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    let original = std::fs::read_to_string(workspace.path("packages/ui/src/index.ts")).unwrap();
    workspace.write(
        "packages/ui/src/index.ts",
        &original.replace("export function button", "export function renamedButton"),
    );
    client.change("packages/ui/src/index.ts", CHANGED);
    client.wait_until("import error in web", |c| {
        c.messages_for("apps/web/src/main.ts")
            .iter()
            .any(|m| m.contains("button"))
    });
    assert_eq!(client.messages_for("packages/ui/src/index.ts").len(), 1);

    workspace.write("packages/ui/src/index.ts", &original);
    client.change("packages/ui/src/index.ts", CHANGED);
    client.wait_until("import error gone", |c| {
        let web = c.messages_for("apps/web/src/main.ts");
        web.len() == 3
            && !web
                .iter()
                .any(|m| m.contains("renamedButton") || m.contains("has no exported member"))
    });
    client.shutdown();
    assert_eq!(
        snapshot(&workspace.root),
        before,
        "nothing written into the workspace"
    );
}

#[test]
fn files_in_an_editor_are_suppressed_and_republished_on_close() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("ws");
    let mut client = Client::start_with(&workspace, Value::Null, true);
    client.wait_checks();
    assert_eq!(client.messages_for("apps/web/src/main.ts").len(), 3);

    client.open("apps/web/src/main.ts");
    client.show("apps/web/src/main.ts");
    client.wait_until("suppression", |c| {
        c.messages_for("apps/web/src/main.ts").is_empty()
    });
    // Pulled right away, as for a file opened in an editor: no refresh needed.
    client.settle(Duration::from_millis(600));
    assert_eq!(client.refresh_requests, 0);

    // A change while the file is open publishes nothing for it.
    workspace.write(
        "apps/web/src/main.ts",
        &format!(
            "{}const extra: number = \"e\";\n",
            std::fs::read_to_string(workspace.path("apps/web/src/main.ts")).unwrap()
        ),
    );
    let publishes = client
        .publishes
        .get(&uri(&workspace.path("apps/web/src/main.ts")))
        .copied();
    client.change("apps/web/src/main.ts", CHANGED);
    client.wait_checks();
    assert_eq!(
        client
            .publishes
            .get(&uri(&workspace.path("apps/web/src/main.ts")))
            .copied(),
        publishes
    );

    client.close("apps/web/src/main.ts");
    client.wait_until("republish", |c| {
        c.messages_for("apps/web/src/main.ts").len() == 4
    });
    client.shutdown();
}

#[test]
fn files_opened_without_a_pull_stay_reported() {
    if compiler().is_none() {
        return;
    }
    // Zed opens the files listed in its diagnostics panel without pulling diagnostics for them.
    let workspace = Workspace::new("ws");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    let file = uri(&workspace.path("apps/web/src/main.ts"));
    let publishes = client.publishes[&file];
    client.open("apps/web/src/main.ts");
    client.settle(Duration::from_millis(500));
    assert_eq!(client.publishes[&file], publishes);
    assert_eq!(client.messages_for("apps/web/src/main.ts").len(), 3);
    // Pulls for files that are not open change nothing.
    client.show("packages/ui/src/index.ts");
    client.settle(Duration::from_millis(300));
    assert_eq!(client.messages_for("packages/ui/src/index.ts").len(), 1);
    client.shutdown();
}

#[test]
fn files_opened_without_a_pull_trigger_one_refresh() {
    if compiler().is_none() {
        return;
    }
    // The refresh makes Zed pull the panel's files from every server; the regular TypeScript
    // server then reports them and this server stops.
    let workspace = Workspace::new("ws");
    let mut client = Client::start_with(&workspace, Value::Null, true);
    client.wait_checks();
    client.open("apps/web/src/main.ts");
    client.open("packages/ui/src/index.ts");
    client.open("apps/web/src/clean.ts");
    client.wait_until("refresh request", |c| c.refresh_requests == 1);
    assert_eq!(client.messages_for("apps/web/src/main.ts").len(), 3);
    client.show("apps/web/src/main.ts");
    client.show("packages/ui/src/index.ts");
    client.wait_until("suppression after the pull", |c| {
        c.messages_for("apps/web/src/main.ts").is_empty()
            && c.messages_for("packages/ui/src/index.ts").is_empty()
    });
    client.settle(Duration::from_millis(600));
    assert_eq!(client.refresh_requests, 1, "one refresh for the burst");
    client.shutdown();
}

#[test]
fn open_files_can_be_reported() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("ws");
    let mut client = Client::start(
        &workspace,
        json!({ "openFiles": "report", "concurrency": 1 }),
    );
    client.wait_checks();
    let file = uri(&workspace.path("apps/web/src/main.ts"));
    let publishes = client.publishes[&file];
    client.open("apps/web/src/main.ts");
    client.show("apps/web/src/main.ts");
    client.settle(Duration::from_millis(500));
    assert_eq!(client.publishes[&file], publishes);
    assert_eq!(client.messages_for("apps/web/src/main.ts").len(), 3);
    client.shutdown();
}

#[test]
fn config_errors_go_on_the_tsconfig() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("badconfig");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(client.files_with_diagnostics(), vec!["tsconfig.json"]);
    let diagnostic = &client.diagnostics_for("tsconfig.json")[0];
    assert_eq!(diagnostic["code"], 5083);
    assert_eq!(
        diagnostic["range"],
        json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } })
    );

    // Fixing the config re-discovers and re-checks.
    workspace.write(
        "tsconfig.json",
        "{ \"compilerOptions\": { \"strict\": true }, \"include\": [\"src\"] }\n",
    );
    client.change("tsconfig.json", CHANGED);
    client.wait_until("config error cleared", |c| {
        c.files_with_diagnostics().is_empty()
    });
    client.shutdown();
}

#[cfg(unix)]
#[test]
fn changes_reported_through_a_workspace_symlink_are_followed() {
    if compiler().is_none() {
        return;
    }
    // bun links workspace packages into node_modules, and Zed may report a change under the link.
    let workspace = Workspace::new("ws");
    std::fs::create_dir_all(workspace.path("apps/web/node_modules/@acme")).unwrap();
    std::os::unix::fs::symlink(
        "../../../../packages/ui",
        workspace.path("apps/web/node_modules/@acme/ui"),
    )
    .unwrap();
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(client.messages_for("packages/ui/src/index.ts").len(), 1);
    let original = std::fs::read_to_string(workspace.path("packages/ui/src/index.ts")).unwrap();
    workspace.write(
        "packages/ui/src/index.ts",
        &original.replace(
            "export const uiBroken: number = \"ui\";",
            "export const uiFixed = 1;",
        ),
    );
    client.change("apps/web/node_modules/@acme/ui/src/index.ts", CHANGED);
    client.wait_until("error fixed through the link", |c| {
        c.messages_for("packages/ui/src/index.ts").is_empty()
    });
    client.shutdown();
}

/// Relative path of the native compiler in a hoisted install of `typescript` >= 7.
fn platform_tsc() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let exe = if cfg!(windows) { "tsc.exe" } else { "tsc" };
    format!("node_modules/@typescript/typescript-{os}-{arch}/lib/{exe}")
}

fn run_has_compiler(args: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tsgo-watch-lsp"))
        .arg("--has-compiler")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run --has-compiler")
}

#[test]
fn has_compiler_prints_the_compiler_for_a_directory() {
    // Only the lookup runs, so no real compiler is needed.
    let workspace = Workspace::new("ws");

    // The fixture has no node_modules. The lookup continues above the workspace, so only assert
    // that nothing below it is reported.
    let output = run_has_compiler(&[&workspace.root]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    if output.status.code() == Some(0) {
        assert!(!Path::new(stdout.trim_end()).starts_with(&workspace.root));
    } else {
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(stdout, "");
    }

    workspace.write(
        "node_modules/typescript/package.json",
        r#"{ "name": "typescript", "version": "7.0.2" }"#,
    );
    workspace.write(&platform_tsc(), "");
    let output = run_has_compiler(&[&workspace.root]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let printed = stdout.strip_suffix('\n').expect("one line");
    assert!(!printed.contains('\n'));
    assert_eq!(
        std::fs::canonicalize(printed).unwrap(),
        std::fs::canonicalize(workspace.path(&platform_tsc())).unwrap()
    );

    let output = run_has_compiler(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn a_compiler_that_cannot_start_does_not_hold_up_other_projects() {
    if compiler().is_none() {
        return;
    }
    // apps/bad has its own TypeScript 7 whose binary cannot be executed. It is first in line, and
    // with concurrency 1 the other projects must still be checked right away.
    let workspace = Workspace::new("ws");
    workspace.write("apps/bad/tsconfig.json", r#"{ "include": ["src"] }"#);
    workspace.write(
        "apps/bad/node_modules/typescript/package.json",
        r#"{ "name": "typescript", "version": "7.0.2" }"#,
    );
    workspace.write(&format!("apps/bad/{}", platform_tsc()), "not a compiler\n");
    let mut client = Client::start(&workspace, json!({ "concurrency": 1 }));
    client.wait_checks();
    assert_eq!(
        client.files_with_diagnostics(),
        vec![
            "apps/web/src/main.ts",
            "packages/ui/src/index.ts",
            "scripts/tool.ts"
        ]
    );
    client.shutdown();
}

#[test]
fn other_json_files_outside_every_program_count_as_configs() {
    if compiler().is_none() {
        return;
    }
    // web extends a shared base config outside its directory (Turborepo style); docs references
    // a config with another name that does not exist yet.
    let workspace = Workspace::new("ws");
    workspace.write(
        "packages/config/base.json",
        r#"{ "extends": "../../tsconfig.json" }"#,
    );
    workspace.write(
        "apps/web/tsconfig.json",
        r#"{ "extends": "../../packages/config/base.json", "include": ["src"] }"#,
    );
    workspace.write(
        "apps/docs/tsconfig.json",
        r#"{ "files": [], "references": [{ "path": "./extra.json" }] }"#,
    );
    let unused = "export function f(): void {\n  const unused = 1;\n}\n";
    workspace.write("apps/web/src/unused.ts", unused);
    workspace.write("apps/docs/gen/x.ts", unused);
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert!(client.messages_for("apps/web/src/unused.ts").is_empty());
    assert!(client.messages_for("apps/docs/gen/x.ts").is_empty());

    workspace.write(
        "packages/config/base.json",
        r#"{ "extends": "../../tsconfig.json", "compilerOptions": { "noUnusedLocals": true } }"#,
    );
    client.change("packages/config/base.json", CHANGED);
    client.wait_until("error from the changed base config", |c| {
        c.messages_for("apps/web/src/unused.ts")
            .iter()
            .any(|m| m.contains("'unused'"))
    });

    // Only the referenced config reports unused locals in docs.
    workspace.write(
        "apps/docs/extra.json",
        r#"{ "extends": "../../tsconfig.json", "compilerOptions": { "noUnusedLocals": true }, "files": ["gen/x.ts"] }"#,
    );
    client.change("apps/docs/extra.json", CREATED);
    client.wait_until("the created referenced config is checked", |c| {
        c.messages_for("apps/docs/gen/x.ts")
            .iter()
            .any(|m| m.contains("'unused'"))
    });
    client.shutdown();
}

#[test]
fn gitignored_files_count_only_when_a_program_contains_them() {
    if compiler().is_none() {
        return;
    }
    let workspace = Workspace::new("single");
    workspace.write(".gitignore", "dist/\nsrc/generated.ts\n");
    workspace.write("src/generated.ts", "export const generated: number = 1;\n");
    let mut client = Client::start(&workspace, Value::Null);
    client.wait_checks();
    assert_eq!(client.files_with_diagnostics(), vec!["src/broken.ts"]);

    // Build output written by a bundler or dev server: no check, not even for JSON files.
    let ends = client.progress_ends;
    let output = [
        "dist/index.js",
        "dist/index.d.ts",
        "dist/stats.json",
        "dist/package.json",
    ];
    for rel in output {
        workspace.write(rel, "{}\n");
        client.change(rel, CREATED);
    }
    client.settle(Duration::from_millis(1000));
    assert_eq!(client.progress_ends, ends, "no check for build output");

    // A gitignored file that the program contains is checked.
    workspace.write(
        "src/generated.ts",
        "export const generated: number = \"g\";\n",
    );
    client.change("src/generated.ts", CHANGED);
    client.wait_until("error in the generated file", |c| {
        !c.messages_for("src/generated.ts").is_empty()
    });
    client.shutdown();
}
