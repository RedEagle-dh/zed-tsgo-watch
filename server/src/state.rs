//! Projects, file ownership, scheduling and the set of published diagnostics.
//!
//! Pure bookkeeping without I/O, so that it can be unit tested. All paths are real paths.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::Diagnostic;

pub type FileKey = Arc<Path>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Created,
    Changed,
    Deleted,
}

/// What one finished check produced.
#[derive(Debug, Default)]
pub struct CheckResult {
    /// Files of the program inside the workspace, without `node_modules`.
    pub files: Vec<PathBuf>,
    /// Diagnostics in tsc order. Global ones are already moved to the tsconfig.
    pub diagnostics: Vec<(PathBuf, Diagnostic)>,
}

#[derive(Debug)]
pub struct Project {
    pub config: PathBuf,
    pub dir: PathBuf,
    pub depth: usize,
    pub compiler: Option<PathBuf>,
    pub fileset: HashSet<FileKey>,
    pub diagnostics: HashMap<FileKey, Vec<Diagnostic>>,
    pub has_run: bool,
    pub dirty: bool,
    pub forced: bool,
    /// Was dirty while not runnable, so its last result is out of date and not published until
    /// a successful check replaces it.
    pub stale: bool,
}

impl Project {
    fn new(config: PathBuf, compiler: Option<PathBuf>) -> Self {
        let dir = config.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            depth: dir.components().count(),
            dir,
            config,
            compiler,
            fileset: HashSet::new(),
            diagnostics: HashMap::new(),
            has_run: false,
            dirty: true,
            forced: true,
            stale: false,
        }
    }
}

#[derive(Debug)]
pub struct State {
    root: PathBuf,
    pub projects: Vec<Project>,
    /// Configs with a compiler running, also those whose project was removed meanwhile.
    running: HashSet<PathBuf>,
    by_dir: HashMap<PathBuf, Vec<usize>>,
    owns_cache: Vec<Option<bool>>,
    interned: HashSet<FileKey>,
    /// Files the client has open (`didOpen` … `didClose`).
    open: HashSet<PathBuf>,
    /// Open files the client pulled diagnostics for, which Zed does for files in an editor but
    /// not for files it opens only to show them in its diagnostics panel. Only these are
    /// suppressed: the regular TypeScript server reports them, but not the panel-only ones.
    in_editor: HashSet<PathBuf>,
    pub suppress_open: bool,
    published: HashMap<FileKey, Vec<Diagnostic>>,
}

impl State {
    pub fn new(root: PathBuf, suppress_open: bool) -> Self {
        Self {
            root,
            projects: Vec::new(),
            running: HashSet::new(),
            by_dir: HashMap::new(),
            owns_cache: Vec::new(),
            interned: HashSet::new(),
            open: HashSet::new(),
            in_editor: HashSet::new(),
            suppress_open,
            published: HashMap::new(),
        }
    }

    pub fn opened(&mut self, path: PathBuf) {
        self.open.insert(path);
    }

    pub fn closed(&mut self, path: &Path) {
        self.open.remove(path);
        self.in_editor.remove(path);
    }

    /// Whether a file is open and was not pulled yet while this server shows diagnostics for it.
    /// Zed opens such files for its diagnostics panel; a diagnostics refresh makes Zed pull them
    /// from every server, so that the regular TypeScript server takes them over.
    pub fn needs_refresh(&self) -> bool {
        self.suppress_open
            && self.open.iter().any(|path| {
                !self.in_editor.contains(path) && self.published.contains_key(path.as_path())
            })
    }

    /// Records a diagnostics pull for `path`. Returns true when this newly suppresses it.
    pub fn pulled(&mut self, path: &Path) -> bool {
        self.open.contains(path) && self.in_editor.insert(path.to_path_buf())
    }

    fn intern(&mut self, path: PathBuf) -> FileKey {
        if let Some(existing) = self.interned.get(path.as_path()) {
            return existing.clone();
        }
        let key: FileKey = Arc::from(path);
        self.interned.insert(key.clone());
        key
    }

    /// Replaces the project list. Projects that still exist keep their last result. Every
    /// project is marked to be checked again.
    pub fn set_projects(&mut self, configs: Vec<(PathBuf, Option<PathBuf>)>) {
        let mut old: HashMap<PathBuf, Project> = self
            .projects
            .drain(..)
            .map(|p| (p.config.clone(), p))
            .collect();
        let mut projects: Vec<Project> = configs
            .into_iter()
            .map(|(config, compiler)| match old.remove(&config) {
                Some(mut project) => {
                    project.compiler = compiler;
                    project.dirty = true;
                    project.forced = true;
                    project
                }
                None => Project::new(config, compiler),
            })
            .collect();
        projects.sort_by(|a, b| a.config.cmp(&b.config));
        self.by_dir.clear();
        for (index, project) in projects.iter().enumerate() {
            self.by_dir
                .entry(project.dir.clone())
                .or_default()
                .push(index);
        }
        self.projects = projects;
        self.owns_cache = vec![None; self.projects.len()];
        self.prune();
    }

    /// Whether the last file list of any project contains `path`.
    pub fn in_fileset(&self, path: &Path) -> bool {
        self.projects.iter().any(|p| p.fileset.contains(path))
    }

    pub fn index_of(&self, config: &Path) -> Option<usize> {
        self.projects.iter().position(|p| p.config == config)
    }

    /// Marks projects dirty for a change of `path` on disk.
    pub fn file_changed(&mut self, path: &Path, change: Change) {
        let mut in_any = false;
        for project in &mut self.projects {
            if project.fileset.contains(path) {
                project.dirty = true;
                in_any = true;
            }
        }
        if !in_any || change != Change::Changed {
            for project in &mut self.projects {
                if path.starts_with(&project.dir) || project.fileset.contains(path) {
                    project.dirty = true;
                    project.forced = true;
                }
            }
        }
    }

    /// The project that reports `file`: the deepest project directory above it whose last file
    /// list contains it; on a tie the smallest config path.
    pub fn owner(&self, file: &Path) -> Option<usize> {
        for dir in file.ancestors().skip(1) {
            if let Some(indices) = self.by_dir.get(dir) {
                for &index in indices {
                    if self.projects[index].fileset.contains(file) {
                        return Some(index);
                    }
                }
            }
            if dir == self.root {
                break;
            }
        }
        None
    }

    fn owns_any(&mut self, index: usize) -> bool {
        if let Some(cached) = self.owns_cache[index] {
            return cached;
        }
        let owns = self.projects[index]
            .fileset
            .iter()
            .any(|file| self.owner(file) == Some(index));
        self.owns_cache[index] = Some(owns);
        owns
    }

    /// Picks the projects to start now, at most `concurrency` running at once, deepest first.
    /// The returned projects are marked running and clean.
    pub fn schedule(&mut self, concurrency: usize) -> Vec<usize> {
        let mut runnable = Vec::new();
        for index in 0..self.projects.len() {
            let project = &self.projects[index];
            let wants = project.dirty && !self.running.contains(&project.config);
            let has_compiler = project.compiler.is_some();
            let urgent = project.forced || !project.has_run;
            let ok = wants && has_compiler && (urgent || self.owns_any(index));
            let project = &mut self.projects[index];
            if wants && project.has_run && !ok {
                project.stale = true;
            }
            if ok {
                runnable.push(index);
            }
        }
        runnable.sort_by(|&a, &b| {
            let (a, b) = (&self.projects[a], &self.projects[b]);
            b.depth.cmp(&a.depth).then_with(|| a.config.cmp(&b.config))
        });
        runnable.truncate(concurrency.saturating_sub(self.running.len()));
        for &index in &runnable {
            let project = &mut self.projects[index];
            self.running.insert(project.config.clone());
            project.dirty = false;
            project.forced = false;
        }
        runnable
    }

    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Records the end of a check. `None` (crash) keeps the previous result, hidden if stale.
    /// The result of a project that no longer exists is dropped.
    pub fn finish(&mut self, config: &Path, result: Option<CheckResult>) {
        self.running.remove(config);
        let Some(index) = self.index_of(config) else {
            return;
        };
        let Some(result) = result else { return };
        let fileset: HashSet<FileKey> = result.files.into_iter().map(|f| self.intern(f)).collect();
        let mut diagnostics: HashMap<FileKey, Vec<Diagnostic>> = HashMap::new();
        for (file, diagnostic) in result.diagnostics {
            let key = self.intern(file);
            diagnostics.entry(key).or_default().push(diagnostic);
        }
        let project = &mut self.projects[index];
        project.fileset = fileset;
        project.diagnostics = diagnostics;
        project.has_run = true;
        project.stale = false;
        self.owns_cache = vec![None; self.projects.len()];
        self.prune();
    }

    fn prune(&mut self) {
        self.interned.retain(|key| Arc::strong_count(key) > 1);
    }

    /// The diagnostics every file should show right now.
    fn desired(&self) -> HashMap<FileKey, Vec<Diagnostic>> {
        let visible: Vec<&Project> = self.projects.iter().filter(|p| !p.stale).collect();
        let mut files: HashSet<&FileKey> = HashSet::new();
        for project in &visible {
            files.extend(project.diagnostics.keys());
        }
        let mut desired = HashMap::new();
        for file in files {
            if self.suppress_open && self.in_editor.contains(&**file) {
                continue;
            }
            let diagnostics = match self.owner(file) {
                // A stale owner shows nothing until a successful check replaces its result.
                Some(owner) if self.projects[owner].stale => continue,
                Some(owner) => self.projects[owner]
                    .diagnostics
                    .get(file)
                    .cloned()
                    .unwrap_or_default(),
                None => {
                    let mut union: Vec<Diagnostic> = Vec::new();
                    for project in &visible {
                        for diagnostic in project.diagnostics.get(file).into_iter().flatten() {
                            if !union.contains(diagnostic) {
                                union.push(diagnostic.clone());
                            }
                        }
                    }
                    union
                }
            };
            if !diagnostics.is_empty() {
                desired.insert(file.clone(), diagnostics);
            }
        }
        desired
    }

    /// The files whose diagnostics differ from what was last returned, with their new
    /// diagnostics (empty when a file became clean).
    pub fn take_changes(&mut self) -> Vec<(FileKey, Vec<Diagnostic>)> {
        let desired = self.desired();
        let mut changes: Vec<(FileKey, Vec<Diagnostic>)> = desired
            .iter()
            .filter(|(file, diagnostics)| self.published.get(*file) != Some(diagnostics))
            .map(|(file, diagnostics)| (file.clone(), diagnostics.clone()))
            .collect();
        changes.extend(
            self.published
                .keys()
                .filter(|file| !desired.contains_key(*file))
                .map(|file| (file.clone(), Vec::new())),
        );
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        self.published = desired;
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{NumberOrString, Position, Range};

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    fn diag(line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            code: Some(NumberOrString::Number(2322)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn result(files: &[&str], diagnostics: &[(&str, u32, &str)]) -> Option<CheckResult> {
        Some(CheckResult {
            files: files.iter().map(|f| p(f)).collect(),
            diagnostics: diagnostics
                .iter()
                .map(|(f, l, m)| (p(f), diag(*l, m)))
                .collect(),
        })
    }

    fn tsc() -> Option<PathBuf> {
        Some(p("/bin/tsc"))
    }

    /// Layout (c): root base config that matches every file, plus one config per package.
    fn workspace() -> State {
        let mut state = State::new(p("/w"), true);
        state.set_projects(vec![
            (p("/w/tsconfig.json"), tsc()),
            (p("/w/packages/ui/tsconfig.json"), tsc()),
            (p("/w/apps/web/tsconfig.json"), tsc()),
        ]);
        state
    }

    fn configs(state: &State, indices: &[usize]) -> Vec<String> {
        indices
            .iter()
            .map(|&i| state.projects[i].config.to_string_lossy().into_owned())
            .collect()
    }

    fn start(state: &mut State, concurrency: usize) -> Vec<String> {
        let started = state.schedule(concurrency);
        configs(state, &started)
    }

    fn run_all(state: &mut State) {
        let started = state.schedule(10);
        assert_eq!(
            configs(state, &started),
            vec![
                "/w/apps/web/tsconfig.json",
                "/w/packages/ui/tsconfig.json",
                "/w/tsconfig.json"
            ],
            "deeper projects first"
        );
        state.finish(
            &p("/w/tsconfig.json"),
            result(
                &[
                    "/w/packages/ui/a.ts",
                    "/w/apps/web/main.ts",
                    "/w/scripts/tool.ts",
                ],
                &[
                    ("/w/packages/ui/a.ts", 2, "ui"),
                    ("/w/apps/web/main.ts", 1, "web"),
                    ("/w/scripts/tool.ts", 0, "tool"),
                ],
            ),
        );
        state.finish(
            &p("/w/packages/ui/tsconfig.json"),
            result(
                &["/w/packages/ui/a.ts"],
                &[("/w/packages/ui/a.ts", 2, "ui")],
            ),
        );
        state.finish(
            &p("/w/apps/web/tsconfig.json"),
            result(
                &["/w/apps/web/main.ts", "/w/packages/ui/a.ts"],
                &[
                    ("/w/apps/web/main.ts", 1, "web"),
                    ("/w/packages/ui/a.ts", 2, "ui"),
                ],
            ),
        );
    }

    fn summary(changes: &[(FileKey, Vec<Diagnostic>)]) -> Vec<(String, Vec<String>)> {
        changes
            .iter()
            .map(|(f, d)| {
                (
                    f.to_string_lossy().into_owned(),
                    d.iter().map(|d| d.message.clone()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn ownership_prefers_deepest_ancestor_and_reports_each_file_once() {
        let mut state = workspace();
        run_all(&mut state);
        let owner = |state: &State, f: &str| {
            state
                .owner(Path::new(f))
                .map(|i| state.projects[i].config.clone())
        };
        assert_eq!(
            owner(&state, "/w/packages/ui/a.ts"),
            Some(p("/w/packages/ui/tsconfig.json"))
        );
        assert_eq!(
            owner(&state, "/w/apps/web/main.ts"),
            Some(p("/w/apps/web/tsconfig.json"))
        );
        assert_eq!(
            owner(&state, "/w/scripts/tool.ts"),
            Some(p("/w/tsconfig.json"))
        );
        assert_eq!(owner(&state, "/w/other.ts"), None);
        assert_eq!(
            summary(&state.take_changes()),
            vec![
                ("/w/apps/web/main.ts".into(), vec!["web".into()]),
                ("/w/packages/ui/a.ts".into(), vec!["ui".into()]),
                ("/w/scripts/tool.ts".into(), vec!["tool".into()]),
            ]
        );
        assert!(
            state.take_changes().is_empty(),
            "nothing changed, nothing sent"
        );
    }

    #[test]
    fn owner_result_wins_over_other_projects() {
        let mut state = workspace();
        run_all(&mut state);
        state.take_changes();
        // The owner fixes the file, while the root (stale view) still lists the error.
        let started = state.schedule(10);
        assert!(started.is_empty());
        state.file_changed(Path::new("/w/packages/ui/a.ts"), Change::Changed);
        let started = state.schedule(10);
        assert_eq!(
            configs(&state, &started),
            vec![
                "/w/apps/web/tsconfig.json",
                "/w/packages/ui/tsconfig.json",
                "/w/tsconfig.json"
            ]
        );
        state.finish(
            &p("/w/packages/ui/tsconfig.json"),
            result(&["/w/packages/ui/a.ts"], &[]),
        );
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/packages/ui/a.ts".into(), vec![])]
        );
    }

    #[test]
    fn unowned_files_get_the_deduplicated_union() {
        let mut state = State::new(p("/w"), true);
        state.set_projects(vec![
            (p("/w/a/tsconfig.json"), tsc()),
            (p("/w/b/tsconfig.json"), tsc()),
        ]);
        state.schedule(10);
        // Both report the same error and one more on the shared base config.
        state.finish(
            &p("/w/a/tsconfig.json"),
            result(
                &[],
                &[
                    ("/w/tsconfig.base.json", 0, "base"),
                    ("/w/tsconfig.base.json", 3, "a only"),
                ],
            ),
        );
        state.finish(
            &p("/w/b/tsconfig.json"),
            result(&[], &[("/w/tsconfig.base.json", 0, "base")]),
        );
        assert_eq!(
            summary(&state.take_changes()),
            vec![(
                "/w/tsconfig.base.json".into(),
                vec!["base".into(), "a only".into()]
            )]
        );
    }

    #[test]
    fn change_marks_projects_by_fileset_without_forcing() {
        let mut state = workspace();
        run_all(&mut state);
        state.file_changed(Path::new("/w/apps/web/main.ts"), Change::Changed);
        let dirty: Vec<_> = state
            .projects
            .iter()
            .filter(|p| p.dirty)
            .map(|p| (p.config.clone(), p.forced))
            .collect();
        assert_eq!(
            dirty,
            vec![
                (p("/w/apps/web/tsconfig.json"), false),
                (p("/w/tsconfig.json"), false)
            ]
        );
    }

    #[test]
    fn unknown_or_created_files_force_ancestor_projects() {
        let mut state = workspace();
        run_all(&mut state);
        state.file_changed(Path::new("/w/packages/ui/new.ts"), Change::Changed);
        let forced: Vec<_> = state
            .projects
            .iter()
            .filter(|p| p.forced)
            .map(|p| p.config.clone())
            .collect();
        assert_eq!(
            forced,
            vec![p("/w/packages/ui/tsconfig.json"), p("/w/tsconfig.json")]
        );

        let mut state = workspace();
        run_all(&mut state);
        state.file_changed(Path::new("/w/packages/ui/a.ts"), Change::Deleted);
        let forced: Vec<_> = state
            .projects
            .iter()
            .filter(|p| p.forced)
            .map(|p| p.config.clone())
            .collect();
        // web is not an ancestor but lists the file.
        assert_eq!(
            forced,
            vec![
                p("/w/apps/web/tsconfig.json"),
                p("/w/packages/ui/tsconfig.json"),
                p("/w/tsconfig.json")
            ]
        );
    }

    #[test]
    fn shadowing_root_stays_dirty_and_hidden_until_it_owns_files() {
        let mut state = State::new(p("/w"), true);
        state.set_projects(vec![
            (p("/w/tsconfig.json"), tsc()),
            (p("/w/packages/ui/tsconfig.json"), tsc()),
        ]);
        state.schedule(10);
        state.finish(
            &p("/w/tsconfig.json"),
            result(
                &["/w/packages/ui/a.ts"],
                &[("/w/tsconfig.json", 0, "root config")],
            ),
        );
        state.finish(
            &p("/w/packages/ui/tsconfig.json"),
            result(&["/w/packages/ui/a.ts"], &[]),
        );
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/tsconfig.json".into(), vec!["root config".into()])]
        );

        state.file_changed(Path::new("/w/packages/ui/a.ts"), Change::Changed);
        let started = state.schedule(10);
        assert_eq!(
            configs(&state, &started),
            vec!["/w/packages/ui/tsconfig.json"],
            "root owns nothing"
        );
        let root = state.index_of(Path::new("/w/tsconfig.json")).unwrap();
        assert!(state.projects[root].dirty && state.projects[root].stale);
        // Its stale result is not published.
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/tsconfig.json".into(), vec![])]
        );

        // The package check now fails to include the file, so the root owns it and runs.
        state.finish(&p("/w/packages/ui/tsconfig.json"), result(&[], &[]));
        let started = state.schedule(10);
        assert_eq!(configs(&state, &started), vec!["/w/tsconfig.json"]);
    }

    #[test]
    fn a_stale_result_stays_hidden_until_a_successful_check() {
        let root = p("/w/tsconfig.json");
        let ui = p("/w/packages/ui/tsconfig.json");
        let mut state = State::new(p("/w"), true);
        state.set_projects(vec![(root.clone(), tsc()), (ui.clone(), tsc())]);
        state.schedule(10);
        let root_result = || {
            result(
                &["/w/packages/ui/a.ts"],
                &[("/w/tsconfig.json", 0, "root config")],
            )
        };
        state.finish(&root, root_result());
        state.finish(&ui, result(&["/w/packages/ui/a.ts"], &[]));
        state.take_changes();

        // The root owns nothing, so it stays dirty and its result is hidden.
        state.file_changed(Path::new("/w/packages/ui/a.ts"), Change::Changed);
        assert_eq!(start(&mut state, 1), vec!["/w/packages/ui/tsconfig.json"]);
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/tsconfig.json".into(), vec![])]
        );

        // A new file forces the root. Queued and then running, it stays hidden.
        state.file_changed(Path::new("/w/new.ts"), Change::Created);
        assert!(state.schedule(1).is_empty(), "ui still runs");
        assert!(state.take_changes().is_empty(), "hidden while queued");
        state.finish(&ui, result(&["/w/packages/ui/a.ts"], &[]));
        assert_eq!(start(&mut state, 1), vec!["/w/tsconfig.json"]);
        assert!(state.take_changes().is_empty(), "hidden while running");
        state.finish(&root, None);
        assert!(state.take_changes().is_empty(), "hidden after a crash");

        // Only a successful check shows it again.
        state.file_changed(Path::new("/w/new.ts"), Change::Changed);
        assert_eq!(start(&mut state, 1), vec!["/w/tsconfig.json"]);
        state.finish(&root, root_result());
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/tsconfig.json".into(), vec!["root config".into()])]
        );
    }

    #[test]
    fn a_stale_owner_shows_nothing_until_its_next_check() {
        let root = (p("/w/tsconfig.json"), tsc());
        let ui = (p("/w/packages/ui/tsconfig.json"), tsc());
        let web = (p("/w/apps/web/tsconfig.json"), tsc());
        let a = "/w/packages/ui/a.ts";
        let mut state = State::new(p("/w"), true);
        state.set_projects(vec![root.clone(), ui.clone(), web.clone()]);
        state.schedule(10);
        state.finish(&root.0, result(&[a], &[(a, 0, "old")]));
        state.finish(&ui.0, result(&[a], &[(a, 0, "old")]));
        // web reports the file while checking its dependency.
        state.finish(
            &web.0,
            result(&["/w/apps/web/main.ts", a], &[(a, 0, "old")]),
        );
        state.take_changes();

        // The root shadows the packages: it owns nothing, so it goes stale.
        state.file_changed(Path::new(a), Change::Changed);
        assert_eq!(
            start(&mut state, 10),
            vec!["/w/apps/web/tsconfig.json", "/w/packages/ui/tsconfig.json"]
        );
        assert!(state.projects[state.index_of(&root.0).unwrap()].stale);
        state.finish(&ui.0, result(&[a], &[(a, 0, "new")]));
        state.finish(
            &web.0,
            result(&["/w/apps/web/main.ts", a], &[(a, 0, "new")]),
        );
        assert_eq!(
            summary(&state.take_changes()),
            vec![(a.into(), vec!["new".into()])]
        );

        // ui's config is removed, so the stale root owns the file. Its old result stays hidden
        // while its check runs, although web still reports the file.
        state.set_projects(vec![root.clone(), web]);
        assert_eq!(state.owner(Path::new(a)), state.index_of(&root.0));
        assert_eq!(
            start(&mut state, 10),
            vec!["/w/apps/web/tsconfig.json", "/w/tsconfig.json"]
        );
        assert_eq!(summary(&state.take_changes()), vec![(a.into(), vec![])]);
        state.finish(&root.0, result(&[a], &[(a, 0, "new")]));
        assert_eq!(
            summary(&state.take_changes()),
            vec![(a.into(), vec!["new".into()])]
        );
    }

    #[test]
    fn never_runs_a_project_twice_and_reruns_when_dirtied_while_running() {
        let mut state = workspace();
        run_all(&mut state);
        state.file_changed(Path::new("/w/scripts/tool.ts"), Change::Changed);
        assert_eq!(start(&mut state, 10), vec!["/w/tsconfig.json"]);
        state.file_changed(Path::new("/w/scripts/tool.ts"), Change::Changed);
        assert!(state.schedule(10).is_empty(), "already running");
        state.finish(&p("/w/tsconfig.json"), result(&["/w/scripts/tool.ts"], &[]));
        assert_eq!(
            start(&mut state, 10),
            vec!["/w/tsconfig.json"],
            "dirty again"
        );
    }

    #[test]
    fn a_job_outlives_rediscovery_and_its_config_never_runs_twice() {
        let root = (p("/w/tsconfig.json"), tsc());
        let ui = (p("/w/packages/ui/tsconfig.json"), tsc());
        let web = (p("/w/apps/web/tsconfig.json"), tsc());
        let mut state = workspace();
        assert_eq!(start(&mut state, 1), vec!["/w/apps/web/tsconfig.json"]);
        // web's config disappears while its compiler runs: the job still takes the slot.
        state.set_projects(vec![root.clone(), ui.clone()]);
        assert_eq!(state.running(), 1);
        assert!(
            state.schedule(1).is_empty(),
            "concurrency counts the old job"
        );
        // It comes back before the old job ends: no second job for it.
        state.set_projects(vec![root, ui, web]);
        assert_eq!(
            start(&mut state, 10),
            vec!["/w/packages/ui/tsconfig.json", "/w/tsconfig.json"]
        );
        assert_eq!(state.running(), 3);
        state.finish(&p("/w/apps/web/tsconfig.json"), result(&[], &[]));
        assert_eq!(state.running(), 2);
        assert_eq!(
            start(&mut state, 10),
            vec!["/w/apps/web/tsconfig.json"],
            "the re-added project runs after the old job"
        );
    }

    #[test]
    fn respects_concurrency() {
        let mut state = workspace();
        assert_eq!(start(&mut state, 1), vec!["/w/apps/web/tsconfig.json"]);
        assert!(state.schedule(1).is_empty());
        state.finish(&p("/w/apps/web/tsconfig.json"), result(&[], &[]));
        assert_eq!(start(&mut state, 1), vec!["/w/packages/ui/tsconfig.json"]);
    }

    #[test]
    fn crash_keeps_previous_result_and_does_not_loop() {
        let mut state = workspace();
        run_all(&mut state);
        state.take_changes();
        state.file_changed(Path::new("/w/scripts/tool.ts"), Change::Changed);
        state.schedule(10);
        state.finish(&p("/w/tsconfig.json"), None);
        assert!(state.take_changes().is_empty());
        assert!(state.schedule(10).is_empty());
    }

    #[test]
    fn projects_without_compiler_never_run() {
        let mut state = State::new(p("/w"), true);
        state.set_projects(vec![(p("/w/tsconfig.json"), None)]);
        assert!(state.schedule(10).is_empty());
        assert!(!state.projects[0].stale);
    }

    #[test]
    fn files_in_an_editor_are_suppressed_and_republished_on_close() {
        let mut state = workspace();
        run_all(&mut state);
        state.take_changes();
        let main = Path::new("/w/apps/web/main.ts");
        // Opened (for example by the diagnostics panel) but not pulled: still reported.
        state.opened(main.to_path_buf());
        assert!(state.take_changes().is_empty());
        assert!(!state.pulled(Path::new("/w/scripts/tool.ts")), "not open");
        assert!(state.pulled(main));
        assert!(!state.pulled(main), "already suppressed");
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/apps/web/main.ts".into(), vec![])]
        );
        state.closed(main);
        assert_eq!(
            summary(&state.take_changes()),
            vec![("/w/apps/web/main.ts".into(), vec!["web".into()])]
        );

        state.suppress_open = false;
        state.opened(main.to_path_buf());
        state.pulled(main);
        assert!(state.take_changes().is_empty());
    }

    #[test]
    fn open_files_with_diagnostics_that_were_not_pulled_need_a_refresh() {
        let mut state = workspace();
        run_all(&mut state);
        state.take_changes();
        let main = Path::new("/w/apps/web/main.ts");
        let clean = Path::new("/w/apps/web/clean.ts");
        state.opened(clean.to_path_buf());
        assert!(
            !state.needs_refresh(),
            "nothing to hand over for a clean file"
        );
        state.opened(main.to_path_buf());
        assert!(state.needs_refresh());
        state.pulled(main);
        state.take_changes();
        assert!(!state.needs_refresh());
        state.closed(main);
        state.take_changes();
        state.suppress_open = false;
        state.opened(main.to_path_buf());
        assert!(
            !state.needs_refresh(),
            "nothing is suppressed in report mode"
        );
    }

    #[test]
    fn rediscovery_keeps_results_and_removes_gone_projects() {
        let mut state = workspace();
        run_all(&mut state);
        state.take_changes();
        state.set_projects(vec![
            (p("/w/tsconfig.json"), tsc()),
            (p("/w/apps/web/tsconfig.json"), tsc()),
        ]);
        assert!(
            state
                .projects
                .iter()
                .all(|p| p.dirty && p.forced && p.has_run)
        );
        // ui is gone; the root now owns its file and reports the same error.
        assert!(state.take_changes().is_empty());
        let started = state.schedule(10);
        assert_eq!(started.len(), 2);
        // A result for a removed project is ignored.
        state.finish(&p("/w/packages/ui/tsconfig.json"), result(&[], &[]));
    }
}
