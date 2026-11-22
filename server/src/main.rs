//! `tsgo-watch-lsp`: a language server that type-checks a whole workspace with TypeScript 7
//! one-shot incremental checks and publishes the results for every file, opened or not.

mod compiler;
mod discover;
mod output;
mod paths;
mod runner;
mod state;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, select};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::{
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    FileChangeType, InitializeParams, MessageType, PublishDiagnosticsParams, ShowMessageParams,
    Uri, notification, request::Request as _,
};
use serde_json::json;

use crate::paths::{RootMap, in_node_modules, path_to_uri, uri_to_path};
use crate::runner::{Done, Job, Runner};
use crate::state::{Change, State};

const DEBOUNCE: Duration = Duration::from_millis(150);
const SOURCE_GLOB: &str = "**/*.{ts,tsx,mts,cts,js,jsx,mjs,cjs,json}";
const LOCKFILE_GLOB: &str = "**/{bun.lock,bun.lockb,package-lock.json,yarn.lock,pnpm-lock.yaml}";
const LOCKFILES: [&str; 5] = [
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
];

fn log(message: &str) {
    eprintln!("tsgo-watch: {message}");
}

struct Options {
    suppress_open: bool,
    concurrency: usize,
}

impl Options {
    fn from_json(value: Option<&serde_json::Value>) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let mut options = Self {
            suppress_open: true,
            concurrency: (cores / 4).clamp(1, 4),
        };
        if let Some(value) = value {
            match value.get("openFiles").and_then(|v| v.as_str()) {
                Some("report") => options.suppress_open = false,
                Some("suppress") | None => {}
                Some(other) => log(&format!(
                    "unknown openFiles value {other:?}, using \"suppress\""
                )),
            }
            if let Some(n) = value.get("concurrency").and_then(|v| v.as_u64()) {
                options.concurrency = (n as usize).max(1);
            }
        }
        options
    }
}

struct Progress {
    token: String,
    started: usize,
    finished: usize,
}

struct Server<'a> {
    connection: &'a Connection,
    root: RootMap,
    state: State,
    runner: Runner,
    fallback: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    concurrency: usize,
    progress_supported: bool,
    progress: Option<Progress>,
    next_id: u64,
    events: Vec<(PathBuf, Change)>,
    deadline: Option<Instant>,
    warned_no_compiler: bool,
    shutting_down: bool,
}

impl<'a> Server<'a> {
    fn send(&self, message: Message) {
        let _ = self.connection.sender.send(message);
    }

    fn notify(&self, method: &str, params: impl serde::Serialize) {
        self.send(Notification::new(method.to_string(), params).into());
    }

    fn request(&mut self, method: &str, params: impl serde::Serialize) {
        self.next_id += 1;
        let id = RequestId::from(format!("tsgo-watch-{}", self.next_id));
        self.send(Request::new(id, method.to_string(), params).into());
    }

    fn register_watchers(&mut self) {
        let registration = json!({
            "registrations": [{
                "id": "tsgo-watch-files",
                "method": notification::DidChangeWatchedFiles::METHOD,
                "registerOptions": {
                    "watchers": [{ "globPattern": SOURCE_GLOB }, { "globPattern": LOCKFILE_GLOB }]
                }
            }]
        });
        self.request(lsp_types::request::RegisterCapability::METHOD, registration);
    }

    fn rediscover(&mut self) {
        let started = Instant::now();
        let configs = discover::discover(&self.root.real);
        let mut missing = Vec::new();
        let projects: Vec<(PathBuf, Option<PathBuf>)> = configs
            .into_iter()
            .map(|config| {
                let dir = config.parent().unwrap_or(Path::new("/"));
                let compiler = compiler::find_compiler(dir).or_else(|| self.fallback.clone());
                if compiler.is_none() {
                    missing.push(self.relative(&config));
                }
                (config, compiler)
            })
            .collect();
        log(&format!(
            "found {} project(s) in {:.0?}",
            projects.len(),
            started.elapsed()
        ));
        for (config, compiler) in &projects {
            if let Some(compiler) = compiler {
                log(&format!(
                    "{} uses {}",
                    self.relative(config),
                    compiler.display()
                ));
            }
        }
        if !missing.is_empty() {
            log(&format!(
                "no TypeScript 7 compiler for {}",
                missing.join(", ")
            ));
            if !self.warned_no_compiler {
                self.warned_no_compiler = true;
                let message = format!(
                    "tsgo-watch: no TypeScript 7 compiler found for {}. Install typescript@7 in the project.",
                    missing.join(", ")
                );
                self.notify(
                    notification::ShowMessage::METHOD,
                    ShowMessageParams {
                        typ: MessageType::WARNING,
                        message,
                    },
                );
            }
        }
        self.state.set_projects(projects);
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root.real)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Starts whatever can run now, updates progress and publishes changed diagnostics.
    fn run(&mut self) {
        if self.shutting_down {
            return;
        }
        for index in self.state.schedule(self.concurrency) {
            let project = &self.state.projects[index];
            let config = project.config.clone();
            let Some(compiler) = project.compiler.clone() else {
                continue;
            };
            let name = compiler::build_info_name(&config);
            let build_info = match &self.cache_dir {
                Some(dir) => dir.join(name),
                None => std::env::temp_dir().join("tsgo-watch").join(name),
            };
            if let Some(parent) = build_info.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let job = Job {
                config: config.clone(),
                compiler,
                build_info,
                root: self.root.real.clone(),
            };
            match self.runner.start(job) {
                Ok(()) => self.progress_started(),
                Err(error) => {
                    log(&format!(
                        "{}: cannot start compiler: {error}",
                        self.relative(&config)
                    ));
                    self.state.finish(&config, None);
                }
            }
        }
        self.publish();
        if self.state.running() == 0 {
            self.progress_end();
        }
    }

    fn progress_started(&mut self) {
        if !self.progress_supported {
            return;
        }
        if let Some(progress) = &mut self.progress {
            progress.started += 1;
            return;
        }
        self.next_id += 1;
        let token = format!("tsgo-watch-check-{}", self.next_id);
        self.request("window/workDoneProgress/create", json!({ "token": token }));
        self.notify(
            "$/progress",
            json!({ "token": token, "value": { "kind": "begin", "title": "tsgo-watch", "message": "checking", "cancellable": false } }),
        );
        self.progress = Some(Progress {
            token,
            started: 1,
            finished: 0,
        });
    }

    fn progress_finished(&mut self) {
        if let Some(progress) = &mut self.progress {
            progress.finished += 1;
            let message = format!("{}/{} projects", progress.finished, progress.started);
            let percentage = (progress.finished * 100 / progress.started.max(1)).min(100);
            let token = progress.token.clone();
            self.notify("$/progress", json!({ "token": token, "value": { "kind": "report", "message": message, "percentage": percentage } }));
        }
    }

    fn progress_end(&mut self) {
        if let Some(progress) = self.progress.take() {
            self.notify(
                "$/progress",
                json!({ "token": progress.token, "value": { "kind": "end" } }),
            );
        }
    }

    fn publish(&mut self) {
        for (file, diagnostics) in self.state.take_changes() {
            let uri = path_to_uri(&self.root.to_client(&file));
            self.notify(
                notification::PublishDiagnostics::METHOD,
                PublishDiagnosticsParams {
                    uri,
                    diagnostics,
                    version: None,
                },
            );
        }
    }

    fn on_done(&mut self, done: Done) {
        let name = self.relative(&done.config);
        let seconds = done.duration.as_secs_f64();
        match done.outcome {
            Ok(summary) => {
                log(&format!(
                    "{name}: {seconds:.2}s, {} errors, {} files",
                    summary.errors, summary.listed
                ));
                self.state.finish(&done.config, Some(summary.result));
            }
            Err(error) => {
                log(&format!(
                    "{name}: {seconds:.2}s, check failed, keeping the previous result: {error}"
                ));
                self.state.finish(&done.config, None);
            }
        }
        self.progress_finished();
        self.run();
    }

    fn client_path(&self, uri: &Uri) -> Option<PathBuf> {
        let path = self.root.to_real(&uri_to_path(uri)?);
        self.root.contains_real(&path).then_some(path)
    }

    fn on_watched_files(&mut self, params: DidChangeWatchedFilesParams) {
        for event in params.changes {
            let Some(path) = self.client_path(&event.uri) else {
                continue;
            };
            let change = match event.typ {
                FileChangeType::CREATED => Change::Created,
                FileChangeType::DELETED => Change::Deleted,
                _ => Change::Changed,
            };
            self.events.push((path, change));
        }
        if !self.events.is_empty() {
            self.deadline = Some(Instant::now() + DEBOUNCE);
        }
    }

    fn flush_events(&mut self) {
        self.deadline = None;
        let mut rediscover = false;
        for (path, change) in std::mem::take(&mut self.events) {
            if in_node_modules(&path) {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if name == "package.json"
                || LOCKFILES.contains(&name.as_ref())
                || (name.starts_with("tsconfig") && name.ends_with(".json"))
            {
                rediscover = true;
                continue;
            }
            self.state.file_changed(&path, change);
        }
        if rediscover {
            self.rediscover();
        }
        self.run();
    }

    /// Returns false when the server should exit.
    fn handle(&mut self, message: Message) -> bool {
        match message {
            Message::Request(request) => {
                if request.method == lsp_types::request::Shutdown::METHOD {
                    self.shutting_down = true;
                    self.runner.kill_all();
                    self.send(Response::new_ok(request.id, ()).into());
                } else {
                    self.send(
                        Response::new_err(
                            request.id,
                            lsp_server::ErrorCode::MethodNotFound as i32,
                            format!("unhandled method {}", request.method),
                        )
                        .into(),
                    );
                }
            }
            Message::Response(response) => {
                if let Err(error) = response.response_result {
                    log(&format!(
                        "client returned an error for request {}: {}",
                        response.id, error.message
                    ));
                }
            }
            Message::Notification(notification) => match notification.method.as_str() {
                notification::Exit::METHOD => return false,
                notification::DidOpenTextDocument::METHOD => {
                    if let Ok(params) =
                        serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
                        && let Some(path) = self.client_path(&params.text_document.uri)
                    {
                        self.state.open.insert(path);
                        self.publish();
                    }
                }
                notification::DidCloseTextDocument::METHOD => {
                    if let Ok(params) =
                        serde_json::from_value::<DidCloseTextDocumentParams>(notification.params)
                        && let Some(path) = self.client_path(&params.text_document.uri)
                    {
                        self.state.open.remove(&path);
                        self.publish();
                    }
                }
                notification::DidChangeWatchedFiles::METHOD => {
                    if let Ok(params) =
                        serde_json::from_value::<DidChangeWatchedFilesParams>(notification.params)
                    {
                        self.on_watched_files(params);
                    }
                }
                _ => {}
            },
        }
        true
    }

    fn main_loop(&mut self, done: &Receiver<Done>) {
        loop {
            let timeout = match self.deadline {
                Some(deadline) => crossbeam_channel::at(deadline),
                None => crossbeam_channel::never(),
            };
            select! {
                recv(self.connection.receiver) -> message => match message {
                    Ok(message) => if !self.handle(message) { break },
                    Err(_) => break,
                },
                recv(done) -> finished => if let Ok(finished) = finished && !self.shutting_down {
                    self.on_done(finished);
                },
                recv(timeout) -> _ => self.flush_events(),
            }
        }
        self.runner.kill_all();
    }
}

struct Args {
    fallback: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args { fallback: None };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fallback-compiler" => {
                let value = args.next().ok_or("--fallback-compiler needs a path")?;
                parsed.fallback = Some(PathBuf::from(value));
            }
            "--stdio" => {}
            "--version" => {
                println!("tsgo-watch-lsp {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(parsed)
}

#[allow(deprecated)]
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    params
        .root_uri
        .as_ref()
        .and_then(uri_to_path)
        .or_else(|| {
            params
                .workspace_folders
                .as_ref()?
                .first()
                .and_then(|f| uri_to_path(&f.uri))
        })
        .or_else(|| params.root_path.as_ref().map(PathBuf::from))
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            log(&error);
            std::process::exit(2);
        }
    };
    let (connection, io_threads) = Connection::stdio();
    let Ok((id, params)) = connection.initialize_start() else {
        log("connection closed before initialize");
        return;
    };
    let params: InitializeParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            log(&format!("invalid initialize params: {error}"));
            return;
        }
    };
    let options = Options::from_json(params.initialization_options.as_ref());
    let progress_supported = params
        .capabilities
        .window
        .as_ref()
        .and_then(|w| w.work_done_progress)
        .unwrap_or(false);
    let result = json!({
        "capabilities": { "textDocumentSync": { "openClose": true, "change": 0 } },
        "serverInfo": { "name": "tsgo-watch", "version": env!("CARGO_PKG_VERSION") }
    });
    if connection.initialize_finish(id, result).is_err() {
        log("connection closed during initialize");
        return;
    }
    let Some(root) = workspace_root(&params) else {
        log("no workspace root, staying idle");
        for message in &connection.receiver {
            if let Message::Request(request) = message {
                let _ = connection
                    .sender
                    .send(Response::new_ok(request.id, ()).into());
            }
        }
        return;
    };
    let root = RootMap::new(root);
    log(&format!(
        "workspace {} (concurrency {}, open files {})",
        root.client.display(),
        options.concurrency,
        if options.suppress_open {
            "suppressed"
        } else {
            "reported"
        }
    ));
    if let Some(fallback) = &args.fallback {
        log(&format!("fallback compiler {}", fallback.display()));
    }

    let (done_sender, done_receiver) = crossbeam_channel::unbounded();
    let mut server = Server {
        connection: &connection,
        state: State::new(root.real.clone(), options.suppress_open),
        root,
        runner: Runner::new(done_sender),
        fallback: args.fallback,
        cache_dir: compiler::cache_dir(),
        concurrency: options.concurrency,
        progress_supported,
        progress: None,
        next_id: 0,
        events: Vec::new(),
        deadline: None,
        warned_no_compiler: false,
        shutting_down: false,
    };
    server.register_watchers();
    server.rediscover();
    server.run();
    server.main_loop(&done_receiver);
    drop(server);
    drop(connection);
    let _ = io_threads.join();
}
