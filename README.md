<img src="assets/icon.svg" width="64" height="64" alt="">

# tsgo-watch

A Zed extension that keeps the whole codebase type-checked with TypeScript 7 and reports the
errors as ordinary diagnostics, also for files you never opened. After a refactoring, the project
panel and the project diagnostics panel (`diagnostics: deploy`) show which files broke.

Zed's TypeScript language servers only check open buffers, and TypeScript 7 has no workspace
diagnostics. tsgo-watch adds a small language server, `tsgo-watch-lsp`, that runs one-shot
incremental `tsc` checks per tsconfig whenever files change on disk and publishes the results for
every file. Keep your regular TypeScript server (the `tsgo` extension or Zed's built-in vtsls) for
editing; tsgo-watch only adds the files it does not cover.

## Install

tsgo-watch is not in the Zed extension registry. Install it as a dev extension:

1. Install Rust with [rustup](https://rustup.rs) (Zed compiles the extension with it).
2. Clone this repository and run `zed: install dev extension` from the command palette, choosing
   the repository directory.
3. Open a TypeScript file. The language server starts with it.

### The helper binary

The extension looks for `tsgo-watch-lsp` in this order:

1. `lsp.tsgo-watch.binary.path` in your Zed settings (with optional `arguments` and `env`).
2. `tsgo-watch-lsp` on your `PATH`.
3. The latest release of this repository on GitHub: the asset
   `tsgo-watch-lsp-<target>.gz` for your platform (`aarch64-apple-darwin`, `x86_64-apple-darwin`,
   `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-msvc`,
   `aarch64-pc-windows-msvc`), cached in the extension's work directory. If GitHub cannot be
   reached, a previously downloaded binary is used.

Until there is a release, build it yourself and use option 1 or 2:

```sh
cargo build --release --manifest-path server/Cargo.toml
```

Then put `server/target/release` on the `PATH` that Zed sees, or point the setting at the binary:

```json
{ "lsp": { "tsgo-watch": { "binary": { "path": "/path/to/tsgo-watch-lsp" } } } }
```

### The compiler

For every tsconfig the server uses the nearest TypeScript 7 above it: `typescript` >= 7 (the
native binary in `@typescript/typescript-<os>-<arch>`), otherwise `@typescript/native-preview`.
Hoisted installs and bun's isolated installs (`node_modules/.bun/…`) both work; `node` is not
needed. If the worktree root has neither, the extension installs the latest `typescript` into its
own work directory (like the `tsgo` extension) and passes it to the server as a fallback. Without
any compiler the server says so once and stays idle.

## Settings

```json
{
  "lsp": {
    "tsgo-watch": {
      "binary": { "path": "…", "arguments": [], "env": {} },
      "initialization_options": {
        "openFiles": "suppress",
        "concurrency": 2
      }
    }
  }
}
```

- `openFiles`: `"suppress"` (default) leaves files that are open in an editor to your regular
  TypeScript server, so errors are not shown twice. `"report"` reports them too. Use `"report"`
  if you run no other TypeScript server.
- `concurrency`: how many compiler processes may run at once. Default: a quarter of the CPU
  cores, between 1 and 4.

## How it works

- **Projects.** Every `tsconfig.json` in the worktree is a project (`.gitignore` is respected;
  `node_modules` and hidden directories are skipped), plus every config reachable through
  `references` (for example Vite's `tsconfig.app.json` and `tsconfig.node.json`). A change to a
  `tsconfig*.json`, `package.json` or lockfile re-discovers the projects and re-checks all of them.
- **Checks.** `tsc -p <tsconfig> --noEmit --pretty false --incremental --tsBuildInfoFile <cache>
  --listFiles`, run in the tsconfig's directory. The build info goes to the user cache directory
  (`$XDG_CACHE_HOME` or `~/.cache`, `~/Library/Caches`, `%LOCALAPPDATA%`) under `tsgo-watch/`, so
  nothing is written into the repository. No compiler process stays alive between checks.
- **Triggers.** Zed reports changes on disk through `workspace/didChangeWatchedFiles` (saves,
  `git checkout`, formatters, other tools). Events are debounced for 150 ms. A changed file
  re-checks the projects whose last file list contains it; a new, deleted or unknown file also
  re-checks the projects whose directory contains it. Unsaved edits are not checked.
- **Ownership.** When configs overlap (a root config that is also the base of package configs),
  each file is reported by one project: the deepest config directory above the file whose program
  contains it. Files that no such project contains get the de-duplicated union. A dirty project
  that owns no file (a root config that only shadows the packages) is not re-checked on every
  save, and its outdated results are hidden until it runs again.
- **Open files.** The server advertises pull diagnostics. Zed pulls them for files in an editor;
  the server answers with an empty list and stops publishing for those files until they are
  closed. Zed also opens the files listed in its diagnostics panel, without pulling them; for
  those the server keeps reporting and, after 250 ms, asks Zed to refresh pulled diagnostics, so
  that the regular TypeScript server takes them over.

## Measured

Synthetic project with 5000 files / 460k lines, TypeScript 7.0.2, one tsconfig, Linux:

| | |
| --- | --- |
| First complete result after start, no build info yet | 3.5 s |
| First complete result after start, with build info | 1.0 s |
| File saved → diagnostics updated (incl. 150 ms debounce) | 1.5–1.9 s |
| Server memory while idle | 7 MB |
| Compiler processes while idle | 0 |
| Compiler peak memory during a check | 1.3–1.9 GB |

`tsc --watch` re-checks faster (0.16 s) but keeps about 2 GB resident per project for as long as
Zed runs, which is why tsgo-watch runs one-shot checks instead.

## Limits

- Composite project references are not built: a project that references a composite project
  whose `.d.ts` files are not built reports TS6305. References are only followed to find configs.
- Only `tsconfig.json` files and their references are discovered; a config with another name that
  nothing references is not checked.
- Diagnostics reflect the files on disk, not unsaved buffers.
- With `"suppress"`, files open in an editor and files shown in the diagnostics panel rely on your
  regular TypeScript server. With vtsls (which pushes diagnostics) the error count can jump for
  about a second while files change hands; with the `tsgo` extension it does not.
- JavaScript files with `checkJs` are reported, but the server is registered for TypeScript and
  TSX only, so an open JavaScript file is not suppressed.
- Old build info files stay in the cache directory after a project is gone.

## Development

```sh
cd server
cargo fmt --check
cargo clippy --all-targets -- -D warnings
TSGO_WATCH_TEST_TSC=/path/to/@typescript/typescript-<os>-<arch>/lib/tsc cargo test
```

The integration tests start the real server against fixture workspaces with the real compiler
from `TSGO_WATCH_TEST_TSC`. Without it they are skipped, unless `CI` is set.

The extension (repository root) builds with `cargo build --release --target wasm32-wasip2`.

## License

MIT
