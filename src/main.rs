mod channel;
mod clone;
mod diff;
mod gc;
mod inject;
mod replay;
mod serve;
mod signals;
mod store;
mod trial;
mod uninstall;
mod util;
mod verify;
mod wrapper;
mod yarn1;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use store::Store;

#[derive(Parser)]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("MATCHAPACK_BUILD"), ")"), about = "Materialize node_modules from package-lock.json with copy-on-write clones")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run `npm ci` with every package it can clone cloned from the store
    ///
    /// npm's own `npm ci` runs, and only the extraction of each package is
    /// replaced by a clone (the inject engine); npm's arguments go after `--`.
    Install {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Store root (default $MATCHAPACK_STORE or ~/.cache/matchapack/store).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Default: `dir` on macOS, `file` elsewhere (Linux has no directory reflink).
        #[arg(long, value_enum)]
        clone_mode: Option<clone::Mode>,
        /// Run no install scripts (`npm ci --ignore-scripts`).
        #[arg(long)]
        ignore_scripts: bool,
        /// Arguments for npm's own `npm ci`, after `--` (e.g. `matchapack
        /// install . -- --no-audit --omit=dev`).
        #[arg(last = true)]
        npm_args: Vec<String>,
    },
    /// For scripts/verify-sample.sh: the state of a project now, as a run's
    /// log record has it (src/replay.rs): its node_modules, the SHA-256 of
    /// its lockfile and manifests, and whether that file set is complete.
    #[command(hide = true)]
    ReplayState {
        /// npm or yarn1: which lockfiles.
        engine: String,
        root: PathBuf,
    },
    /// Stand in for `npm <args>`: run `npm ci`, `npm install`, `npm update` or
    /// `npm uninstall` (or an alias) with clones from the store when it is
    /// safe to, otherwise the real npm with the same arguments. Meant to be
    /// called by the shell wrapper (shell/matchapack.sh).
    Npm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stand in for `yarn <args>` (Yarn 1, macOS): a bare `yarn`, `yarn
    /// install`, `add`, `upgrade`, `remove`, `upgrade-interactive` and `yarn
    /// workspace <name> add|remove|upgrade|upgrade-interactive` run the user's
    /// Yarn with each file of its link step cloned from Yarn's cache; anything
    /// else, and every command matchapack declines, runs plain Yarn with the
    /// same arguments.
    Yarn {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the shell integration for an rc file (routes `npm ci`, `npm install`, `npm update` and `npm uninstall`, and on macOS Yarn 1's installs and `yarn add` / `upgrade` / `remove`, through matchapack)
    ///
    /// zsh: ~/.zshrc, bash: ~/.bashrc, after the line that puts matchapack on
    /// PATH (otherwise the guard skips it silently):
    ///
    ///     command -v matchapack >/dev/null && eval "$(matchapack init)"
    ///
    /// fish: ~/.config/fish/config.fish:
    ///
    ///     type -q matchapack; and matchapack init | source
    ///
    /// Without SHELL, the shell that runs the command (the parent process) is
    /// used; name it to be explicit.
    ///
    /// It also defines `mp` as a short name for matchapack (a shell
    /// function: `mp npm ci`, `mp store gc`), but only
    /// when nothing of that name exists yet (an alias, function, builtin or
    /// command on PATH), checked at every shell start. --alias NAME picks
    /// another name, --no-alias none. Run by hand on a terminal, it says
    /// whether the name would be defined.
    #[command(verbatim_doc_comment)]
    Init {
        #[arg(value_parser = INIT_SHELLS)]
        shell: Option<String>,
        /// The short name for matchapack instead of `mp` (letters, digits,
        /// `-`, `_`; not a builtin or keyword of the shell, not npm, yarn or
        /// matchapack); defined only when the name is free.
        #[arg(long, value_name = "NAME")]
        alias: Option<String>,
        /// Define no short name for matchapack.
        #[arg(long, conflicts_with = "alias")]
        no_alias: bool,
    },
    /// Remove matchapack's own state: the store, the trial logs, the settings directory
    ///
    /// Without --yes, prints what it would remove and how much space that
    /// frees, and changes nothing. It never edits shell startup files: it
    /// prints the lines that mention matchapack (file:line) for you to
    /// delete, and the command that removes the binary. Installed projects
    /// keep working (their files are clones, independent of the store).
    /// Refused while an install uses the store, and for any directory it
    /// cannot recognise as matchapack's (`/`, the home directory or a parent
    /// of it, a symbolic link, a store without its layout directory, or a
    /// directory a MATCHAPACK_* variable names that holds other files).
    #[command(verbatim_doc_comment)]
    Uninstall {
        /// Remove; without it, a dry run that writes nothing.
        #[arg(long)]
        yes: bool,
    },
    /// Compare two project trees. Exit status: 0 identical, 1 they differ, 2 an error (nothing was decided).
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Relative paths to ignore (repeatable).
        #[arg(long)]
        ignore: Vec<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Print only the differing paths, one per line (for scripts).
        #[arg(long)]
        paths: bool,
        /// Print each difference as `<class>\t<path>` (for scripts; with
        /// --meta the classes of the report, otherwise only-a, only-b, differ).
        #[arg(long, conflicts_with = "paths")]
        classes: bool,
        /// Also compare, per path: mtime to the nanosecond, uid, gid and
        /// extended attribute names; report each class of difference.
        #[arg(long)]
        meta: bool,
        /// With --meta: A's install run, START:END in ns since the epoch. A
        /// time inside it may differ from B's when B's is inside B's run and
        /// of the same kind (see src/diff.rs, MetaOpts).
        #[arg(long, value_name = "START:END", requires = "meta", requires = "b_run")]
        a_run: Option<String>,
        /// With --meta: B's install run (as --a-run).
        #[arg(long, value_name = "START:END", requires = "meta", requires = "a_run")]
        b_run: Option<String>,
        /// With --a-run: the package manager cache A's run used; times its
        /// run wrote into it are told apart from other in-run times.
        #[arg(long, requires = "a_run")]
        a_cache: Option<PathBuf>,
        /// With --b-run: the cache B's run used.
        #[arg(long, requires = "b_run")]
        b_cache: Option<PathBuf>,
        /// In each tree's relative paths, write the tree's own root path
        /// (absolute, and its realpath) as `<root>` before comparing, so a
        /// file an install script named after its own directory (node-gyp's
        /// `.deps/<absolute path>/…`) is the same path in both trees. Only
        /// the whole root, as whole components; --ignore paths are given as
        /// reported (with `<root>`). See src/diff.rs, RootForms.
        #[arg(long)]
        normalize_roots: bool,
    },
    /// Summarise the trial logs: outcomes, reasons, time, space, verification samples.
    Report {
        /// Only the last N days.
        #[arg(long)]
        days: Option<f64>,
    },
    /// Print the store location and size; maintain it (gc, verify).
    Store {
        /// Store root (default $MATCHAPACK_STORE or ~/.cache/matchapack/store).
        #[arg(long, global = true)]
        store: Option<PathBuf>,
        #[command(subcommand)]
        action: Option<StoreCmd>,
    },
}

#[derive(Subcommand)]
enum StoreCmd {
    /// Remove the entries no existing project uses
    ///
    /// An entry is kept while a project installed from it still has its
    /// node_modules: every install records its directory and entries in
    /// <store>/projects/; projects installed before records existed are found
    /// through the trial log and their node_modules/.package-lock.json, or
    /// recorded by `store adopt` (run it first on a store older builds filled).
    /// Removing an entry never breaks an installed node_modules (clones are
    /// independent), but space comes back only for entries no node_modules
    /// still shares, and a later install re-extracts what it needs. Entries
    /// extracted in the last hour are kept. Refused while an install runs.
    /// Lists the records whose path has no node_modules any more (a project
    /// moved, renamed, or copied and the original deleted, or deleted): run
    /// `store adopt` where a moved project is now, before gc, to keep its
    /// entries.
    #[command(verbatim_doc_comment)]
    Gc {
        /// Report what would be removed; remove nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Record the npm projects under DIR so that gc keeps their entries (run before the first gc)
    ///
    /// For projects installed before installs wrote records: finds every
    /// directory under each DIR with npm's hidden lockfile
    /// (node_modules/.package-lock.json) and records it with the store
    /// entries that lockfile lists, as an install would. A project installed
    /// by plain npm is recorded too: its entries are kept, which only means
    /// less space comes back. An existing record is added to, never
    /// narrowed. Symlinks are not followed; node_modules, .git and the store
    /// are not entered; a symlinked node_modules is skipped. Yarn projects
    /// need no record (the Yarn engine clones from Yarn's cache, not the
    /// store). Refused while an install runs, unless --dry-run.
    #[command(verbatim_doc_comment)]
    Adopt {
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
        /// Report what would be recorded; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// List each skipped path and why.
        #[arg(long)]
        verbose: bool,
    },
    /// Compare every entry with npm's own extraction of its tarball; exit 1 when one differs
    ///
    /// Each tarball is taken from the npm cache and extracted by the pacote of
    /// the npm on PATH, as `npm ci` (and the inject engine) would extract it,
    /// and compared with the entry: the same paths, contents, file and
    /// directory modes. A match records that extraction code (its key:
    /// pacote's lib/fetcher.js, tar's code, libuv) on the entry; a difference
    /// is reported with who made the entry.
    #[command(verbatim_doc_comment)]
    Verify {
        /// Replace the entries that differ with npm's extraction (entries
        /// that are not directories are removed).
        #[arg(long)]
        fix: bool,
        /// npm content cache (default ~/.npm/_cacache).
        #[arg(long)]
        npm_cache: Option<PathBuf>,
    },
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn default_store(s: Option<PathBuf>) -> PathBuf {
    s.or_else(|| std::env::var_os("MATCHAPACK_STORE").map(PathBuf::from))
        .unwrap_or_else(|| home().join(".cache/matchapack/store"))
}

fn default_clone_mode() -> clone::Mode {
    if cfg!(target_os = "macos") {
        clone::Mode::Dir
    } else {
        clone::Mode::File
    }
}

pub struct Summary {
    pub placed: usize,
    pub secs: f64,
    /// Store entries extracted during this install.
    pub store_new: usize,
    /// File bytes this install placed as clones (what npm would have written).
    pub bytes_logical: u64,
    /// File bytes newly added to the store by this install.
    pub bytes_store_new: u64,
    /// The inject engine's counts (clones by placement, packages npm
    /// extracted itself and why), for the log.
    pub inject: Option<serde_json::Value>,
}

/// How an install ended when it did not succeed.
pub enum InstallError {
    /// Refused before anything in the project changed: npm can take over as is.
    Declined(anyhow::Error),
    /// matchapack itself failed after node_modules was changed (a panic, a
    /// lost child status); the run's counts and npm's settings when known.
    Broke { error: anyhow::Error, inject: Option<serde_json::Value> },
    /// npm read a key of what the hook returned that pacote's own result
    /// does not have (src/inject.js `resultTrap`): npm's files may differ
    /// from what npm alone would have written. npm's exit status, and the
    /// run's counts.
    Trap { error: anyhow::Error, code: i32, inject: serde_json::Value },
    /// npm ran and exited with this status, having reported the failure
    /// itself; `npm ci` would have failed the same way.
    Npm { code: i32, inject: serde_json::Value },
}

impl InstallError {
    pub fn into_inner(self) -> anyhow::Error {
        match self {
            InstallError::Declined(e) | InstallError::Broke { error: e, .. } | InstallError::Trap { error: e, .. } => e,
            InstallError::Npm { code, .. } => anyhow::anyhow!("npm exited with status {code}"),
        }
    }
}

const INIT_SHELLS: [&str; 3] = ["zsh", "bash", "fish"];

/// What `matchapack init <shell>` prints: the `npm` function, and on macOS
/// the `yarn` one after it (on Linux plain Yarn 1 already clones, so no
/// `yarn` function is defined there). The files in shell/ are the one
/// definition; the binary carries them so the rc line needs no checkout of
/// this repository, and users who already have the line get the `yarn`
/// function with the next `cargo install`. Then the short name `alias`
/// (`mp` unless --alias or --no-alias), guarded so that it is defined only
/// when the name is free in the shell that runs it.
fn init_script(shell: &str, macos: bool, alias: Option<&str>) -> String {
    let (npm, yarn, short) = if shell == "fish" {
        (include_str!("../shell/matchapack.fish"), include_str!("../shell/matchapack-yarn.fish"), include_str!("../shell/matchapack-alias.fish"))
    } else {
        (include_str!("../shell/matchapack.sh"), include_str!("../shell/matchapack-yarn.sh"), include_str!("../shell/matchapack-alias.sh"))
    };
    let mut s = if macos { format!("{npm}{yarn}") } else { npm.to_string() };
    if let Some(name) = alias {
        s.push_str(&short.replace("__MATCHAPACK_ALIAS__", name));
    }
    s
}

/// Builtins and reserved words (bash 5.2 `compgen -k -b`, zsh 5.9
/// `${(k)builtins} ${(k)reswords}`, fish 3.6 `builtin -n` plus 4.0's
/// additions; scripts/test-shells.sh checks the lists against the shells): an alias or function of
/// such a name would shadow the shell's own, or not be callable at all.
/// bash refuses SH_WORDS; zsh SH_WORDS and ZSH_WORDS (a few bash-only names
/// too, kept strict for startup files shared by both); fish FISH_WORDS.
const SH_WORDS: &[&str] = &[
    "alias", "bg", "bind", "break", "builtin", "caller", "case", "cd", "command", "compgen", "complete", "compopt", "continue", "coproc", "declare", "dirs",
    "disown", "do", "done", "echo", "elif", "else", "enable", "esac", "eval", "exec", "exit", "export", "false", "fc", "fg", "fi", "for", "function",
    "getopts", "hash", "help", "history", "if", "in", "jobs", "kill", "let", "local", "logout", "mapfile", "popd", "printf", "pushd", "pwd", "read",
    "readarray", "readonly", "return", "select", "set", "shift", "shopt", "source", "suspend", "test", "then", "time", "times", "trap", "true", "type",
    "typeset", "ulimit", "umask", "unalias", "unset", "until", "wait", "while",
];
const ZSH_WORDS: &[&str] = &[
    "autoload", "bindkey", "bye", "chdir", "compadd", "comparguments", "compcall", "compctl", "compdescribe", "compfiles", "compgroups", "compquote",
    "compset", "comptags", "comptry", "compvalues", "disable", "echotc", "echoti", "emulate", "end", "float", "foreach", "functions", "getln", "integer",
    "limit", "log", "noglob", "nocorrect", "print", "private", "pushln", "r", "rehash", "repeat", "sched", "setopt", "ttyctl", "unfunction", "unhash",
    "unlimit", "unsetopt", "vared", "whence", "where", "which", "zcompile", "zformat", "zle", "zmodload", "zparseopts", "zregexparse", "zstyle",
];
const FISH_WORDS: &[&str] = &[
    "_", "abbr", "and", "argparse", "begin", "bg", "bind", "block", "break", "breakpoint", "builtin", "case", "cd", "command", "commandline", "complete",
    "contains", "continue", "count", "disown", "echo", "else", "emit", "end", "eval", "exec", "exit", "false", "fg", "fish_indent", "fish_key_reader",
    "for", "function", "functions", "history", "if", "jobs", "math", "not", "or", "path", "printf", "pwd", "random", "read", "realpath", "return",
    "set", "set_color", "source", "status", "string", "switch", "test", "time", "true", "type", "ulimit", "wait", "while",
];

/// Whether `name` may be `init --alias`'s name in `shell`.
fn check_alias(name: &str, shell: &str) -> Result<(), String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("--alias {name:?}: use letters, digits, `-` and `_` only"));
    }
    if name.starts_with('-') {
        return Err(format!("--alias {name}: must not start with `-`"));
    }
    if ["npm", "yarn", "matchapack"].contains(&name) {
        return Err(format!("--alias {name}: that name is matchapack's own or one it wraps; choose another (e.g. mp)"));
    }
    let words: Vec<&str> = match shell {
        "fish" => FISH_WORDS.to_vec(),
        "zsh" => SH_WORDS.iter().chain(ZSH_WORDS).copied().collect(),
        _ => SH_WORDS.to_vec(),
    };
    if words.contains(&name) {
        return Err(format!("--alias {name}: a builtin or reserved word of {shell}; choose another (e.g. mp)"));
    }
    Ok(())
}

/// The short name `matchapack init` defines without --alias or --no-alias.
const DEFAULT_ALIAS: &str = "mp";

/// What `init` says about the short name, only when `tty` (stdout is a
/// terminal: someone ran `matchapack init` by hand). From the rc line stdout
/// is a pipe into eval / source, and the note would print at every shell
/// start. The printed integration defines the name only when nothing of
/// that name exists when it runs; of those, matchapack can see a command on
/// PATH, not the shell's own aliases and functions, so for those it names
/// the command that shows them.
fn alias_report(name: Option<&str>, tty: bool) -> Option<String> {
    if !tty {
        return None;
    }
    let Some(name) = name else {
        return Some("matchapack init: no short name for matchapack (--no-alias); run it as `matchapack`".to_string());
    };
    Some(match on_path(name) {
        Some(p) => format!(
            "matchapack init: `{name}` will NOT be defined: `{name}` is already {} on PATH, and the short name is defined only when the name is free. Run matchapack as `matchapack`, or pick another name with --alias NAME.",
            p.display()
        ),
        None => format!(
            "matchapack init: `{name}` will be a short name for matchapack (a shell function: `{name} npm ci`, `{name} store gc`), unless an alias or function named `{name}` already exists when this line runs; `type {name}` shows what `{name}` is now."
        ),
    })
}

/// The first executable named `name` in PATH, if any.
fn on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The shell `matchapack init` was started from: its parent process. In
/// `eval "$(matchapack init)"` that is the zsh or bash reading the rc file, in
/// `matchapack init | source` the fish. Not $SHELL, which names the login shell
/// even inside another one.
fn parent_shell() -> Option<&'static str> {
    let ppid = std::os::unix::process::parent_id();
    let out = std::process::Command::new("ps").args(["-o", "comm=", "-p", &ppid.to_string()]).output().ok()?;
    shell_from_comm(String::from_utf8_lossy(&out.stdout).trim())
}

/// `ps -o comm=` gives `zsh`, `/bin/zsh`, or `-zsh` for a login shell.
fn shell_from_comm(comm: &str) -> Option<&'static str> {
    let name = comm.rsplit('/').next().unwrap_or(comm).trim_start_matches('-');
    INIT_SHELLS.iter().copied().find(|s| *s == name)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Install { dir, store, clone_mode, ignore_scripts, npm_args: extra } => {
            inject::engine().map_err(|e| anyhow::anyhow!(e))?;
            let mut npm_args = vec!["ci".to_string()];
            if ignore_scripts {
                npm_args.push("--ignore-scripts".into());
            }
            npm_args.extend(extra);
            let o = inject::InjectOpts { dir, trial_scope: false, store: default_store(store), clone_mode: clone_mode.unwrap_or_else(default_clone_mode), npm_args };
            let r = inject::run(&o);
            util::remove_run_dir();
            // Stopped by a signal (src/signals.rs): said, and the process
            // ends as npm did (by its signal when it died of one), as for
            // `matchapack npm`; a broken run's error is still printed.
            if let (Some(sig), Err(e)) = (signals::received(), &r) {
                let code = match e {
                    InstallError::Npm { code, .. } | InstallError::Trap { code, .. } => Some(*code).filter(|c| *c != 0),
                    InstallError::Broke { .. } | InstallError::Declined(_) => None,
                };
                signals::say(sig, "npm", code);
                if let InstallError::Broke { error, .. } | InstallError::Trap { error, .. } = e {
                    eprintln!("matchapack: {error:#}");
                }
                signals::finish(code.unwrap_or(128 + sig));
            }
            match r {
                Ok(_) => {}
                // npm printed its own error; pass its status on.
                Err(InstallError::Npm { code, .. }) => std::process::exit(code),
                Err(e) => return Err(e.into_inner()),
            }
        }
        Cmd::ReplayState { engine, root } => {
            let lockfiles = match engine.as_str() {
                "npm" => replay::NPM_LOCKFILES,
                "yarn1" => replay::YARN_LOCKFILES,
                e => anyhow::bail!("replay-state: npm or yarn1, not {e}"),
            };
            println!("{}", replay::state_with_env(&root, lockfiles, &engine));
        }
        Cmd::Npm { args } => {
            let code = wrapper::run(args);
            util::remove_run_dir();
            signals::finish(code);
        }
        Cmd::Yarn { args } => {
            let code = yarn1::cli(args);
            signals::finish(code);
        }
        Cmd::Init { shell, alias, no_alias } => {
            let shell = match shell {
                Some(s) => s,
                None => parent_shell().map(String::from).with_context(|| {
                    format!(
                        "could not tell which shell runs this; name it: matchapack init <{}>\n  \
                         zsh (~/.zshrc) / bash (~/.bashrc): command -v matchapack >/dev/null && eval \"$(matchapack init zsh)\"\n  \
                         fish (~/.config/fish/config.fish): type -q matchapack; and matchapack init fish | source",
                        INIT_SHELLS.join("|")
                    )
                })?,
            };
            // A refused name still prints the integration, without the alias:
            // the rc line's eval / source runs whatever is printed, and the
            // npm and yarn functions must not go missing over a bad name.
            let name = if no_alias { None } else { Some(alias.unwrap_or_else(|| DEFAULT_ALIAS.to_string())) };
            let refused = name.as_deref().and_then(|n| check_alias(n, &shell).err());
            use std::io::IsTerminal;
            let tty = std::io::stdout().is_terminal();
            if refused.is_none() {
                if let Some(r) = alias_report(name.as_deref(), tty) {
                    eprintln!("{r}");
                }
            }
            // By hand only, as the report above: a global npm package of one
            // Node version is lost when the version changes.
            if let Some(n) = channel::init_note(std::env::current_exe().ok().as_deref(), tty) {
                eprintln!("{n}");
            }
            print!("{}", init_script(&shell, cfg!(target_os = "macos"), if refused.is_some() { None } else { name.as_deref() }));
            if let Some(e) = refused {
                eprintln!("matchapack init: error: {e}; the shell integration was printed without it");
                std::process::exit(2);
            }
        }
        Cmd::Uninstall { yes } => {
            let code = uninstall::run(&uninstall::Env::from_process()?, yes, &mut std::io::stdout().lock())?;
            std::process::exit(code);
        }
        Cmd::Diff { a, b, ignore, limit, paths, classes, meta, a_run, b_run, a_cache, b_cache, normalize_roots } => {
            let opts = if meta {
                let parse = |s: Option<String>| {
                    s.as_deref().map(diff::Run::parse).transpose().unwrap_or_else(|e| {
                        eprintln!("matchapack diff: error: {e:#}");
                        std::process::exit(2)
                    })
                };
                Some(diff::MetaOpts {
                    a_run: parse(a_run),
                    b_run: parse(b_run),
                    a_cache,
                    b_cache,
                })
            } else {
                None
            };
            let r = match diff::diff_with(&a, &b, &ignore, opts.as_ref(), normalize_roots) {
                Ok(r) => r,
                Err(e) => {
                    // Not 1: a script must never read an error as "the trees differ".
                    eprintln!("matchapack diff: error: {e:#}");
                    std::process::exit(2);
                }
            };
            if classes {
                r.print_classes();
            } else if paths {
                r.print_paths();
            } else {
                r.print(limit);
            }
            if !r.ok() {
                std::process::exit(1);
            }
        }
        Cmd::Report { days } => {
            let store = Store::new(default_store(None), home().join(".npm/_cacache"))?;
            trial::report(days, &store)?;
        }
        Cmd::Store { store, action } => {
            let s = default_store(store);
            let cacache = home().join(".npm/_cacache");
            match action {
                None => {
                    let n = walkdir::WalkDir::new(s.join(store::LAYOUT)).min_depth(3).max_depth(3).into_iter().count();
                    let p = std::fs::read_dir(s.join("projects")).map(|d| d.count()).unwrap_or(0);
                    println!("{} ({n} packages, {p} projects recorded)", s.display());
                }
                Some(StoreCmd::Gc { dry_run }) => {
                    let st = if dry_run { Store::new(s, cacache)? } else { Store::exclusive(s, cacache)? };
                    let free0 = store::free_bytes(&st.root);
                    let r = gc::gc(&st, &trial::log_path(), dry_run)?;
                    let free1 = store::free_bytes(&st.root);
                    let gib = |b: u64| format!("{:.2} GiB", b as f64 / 1024f64.powi(3));
                    println!("projects in use: {} ({} recorded, {} from the trial log)", r.projects.0 + r.projects.1, r.projects.0, r.projects.1);
                    for l in gc::dropped_note(&r, dry_run).into_iter().chain(gc::unchecked_note(&r)) {
                        println!("{l}");
                    }
                    println!(
                        "{} {} of {} entries, {} of files{}",
                        if dry_run { "would remove" } else { "removed" },
                        r.removed,
                        r.entries,
                        gib(r.bytes_removed),
                        if r.recent > 0 { format!(" (kept {} unused entries extracted in the last hour)", r.recent) } else { String::new() }
                    );
                    // The file bytes above are not the space freed: a removed
                    // entry whose blocks a node_modules still shares (a clone
                    // of it) frees nothing. The volume's free space tells.
                    if r.removed > 0 {
                        if dry_run {
                            println!("space comes back only for entries no node_modules still shares blocks with; the real run reports the volume's free space before and after");
                        } else if let (Some(a), Some(b)) = (free0, free1) {
                            println!(
                                "free space on the store's volume: {} before, {} after ({:+.2} GiB; other activity on the volume counts too)",
                                gib(a),
                                gib(b),
                                (b as f64 - a as f64) / 1024f64.powi(3)
                            );
                        }
                    }
                }
                Some(StoreCmd::Adopt { dirs, dry_run, verbose }) => {
                    let st = if dry_run { Store::new(s, cacache)? } else { Store::exclusive(s, cacache)? };
                    let t = std::time::Instant::now();
                    let r = gc::adopt(&st, &dirs, dry_run)?;
                    let would = |a: &str, b: &str| if dry_run { a.to_string() } else { b.to_string() };
                    if dry_run || verbose {
                        for (p, what, n, absent) in &r.list {
                            let what = match *what {
                                "new" => would("would record", "recorded"),
                                "added" => would("would add to its record", "added to its record"),
                                "complete" => "already recorded".to_string(),
                                _ => "nothing to record".to_string(),
                            };
                            println!("  {}: {what}, {n} store entries ({absent} listed not in the store)", p.display());
                        }
                    }
                    println!(
                        "looked at {} directories in {:.1}s: {} npm projects (node_modules/.package-lock.json), {} node_modules without one (not recorded)",
                        r.dirs,
                        t.elapsed().as_secs_f64(),
                        r.projects,
                        r.no_hidden_lock
                    );
                    println!(
                        "records {}: {} new, {} added to; {} already complete, {} with no package in the store",
                        would("to write", "written"),
                        r.written,
                        r.extended,
                        r.complete,
                        r.no_entry
                    );
                    println!(
                        "store entries {} by these projects: {} of {} ({} listed integrities not in the store)",
                        would("that would be kept", "now kept"),
                        r.entries,
                        st.entries().len(),
                        r.not_in_store
                    );
                    if !r.skipped.is_empty() {
                        let mut why: std::collections::BTreeMap<&str, usize> = Default::default();
                        for (_, w) in &r.skipped {
                            *why.entry(w.split(':').next().unwrap_or(w)).or_default() += 1;
                        }
                        let parts: Vec<String> = why.iter().map(|(w, n)| format!("{n} {w}")).collect();
                        println!("skipped: {}{}", parts.join(", "), if verbose { "" } else { " (--verbose lists them)" });
                        if verbose {
                            for (p, w) in &r.skipped {
                                println!("  {}: {w}", p.display());
                            }
                        }
                    }
                    println!("a record keeps a project's entries through `store gc` while its node_modules exists; a wrong gc decision never breaks a project (its files are clones, independent of the store), it only loses block sharing until its next install");
                }
                Some(StoreCmd::Verify { fix, npm_cache }) => {
                    let cacache = npm_cache.unwrap_or(cacache);
                    let st = if fix { Store::exclusive(s, cacache.clone())? } else { Store::new(s, cacache.clone())? };
                    let (r, key) = verify::verify_store(&st, &cacache, fix)?;
                    println!(
                        "{} entries compared with npm's extraction ({key}) in {:.1}s: {} match, {} differ, {} not directories, {} not checked (tarball no longer in the npm cache), {} not checked (pacote's extraction failed or is not what a clone gives)",
                        r.entries,
                        r.secs,
                        r.matched,
                        r.differs.len(),
                        r.not_dir.len(),
                        r.no_tarball,
                        r.failed.len()
                    );
                    if r.recorded > 0 {
                        println!("{} matching entries recorded as known to {key} (an install then samples them instead of comparing them first)", r.recorded);
                    }
                    let what = |kind: &str| match kind {
                        "extractor" => "made by the classic engine's extractor",
                        "changed" => "changed after this extraction code had made or checked it",
                        _ => "made under other extraction code",
                    };
                    for (i, kind, why, _) in &r.differs {
                        println!("  {}/{}…: {why} ({})", i.algo, &i.hex[..16], what(kind));
                    }
                    for (i, _) in &r.not_dir {
                        println!("  {}/{}…: the entry is not a directory (a symlink?)", i.algo, &i.hex[..16]);
                    }
                    for (i, e) in &r.failed {
                        println!("  {}/{}…: {e}", i.algo, &i.hex[..16]);
                    }
                    let bad = r.differs.len() + r.not_dir.len();
                    if bad > 0 {
                        if fix {
                            println!(
                                "replaced {} entries with npm's extraction; removed {} that were not directories (the next install re-extracts them)",
                                r.differs.len(),
                                r.not_dir.len()
                            );
                        } else {
                            println!("`matchapack store verify --fix` replaces them with npm's extraction");
                        }
                    }
                    if (bad > 0 && !fix) || !r.failed.is_empty() {
                        std::process::exit(1);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{alias_report, check_alias, init_script, shell_from_comm, DEFAULT_ALIAS};

    /// The by-hand report: only when stdout is a terminal; a name on PATH
    /// (`ls`) is reported as not defined, a free one as defined unless the
    /// shell has its own; --no-alias says so.
    #[test]
    fn alias_report_only_on_a_terminal() {
        assert!(alias_report(Some("ls"), true).is_some_and(|w| w.contains("`ls` will NOT be defined") && w.contains("/ls on PATH")));
        let free = alias_report(Some("matchapack-no-such-command-x"), true).unwrap();
        assert!(free.contains("will be a short name for matchapack (a shell function:") && free.contains("`type matchapack-no-such-command-x`"), "{free}");
        assert!(alias_report(None, true).is_some_and(|w| w.contains("--no-alias")));
        assert_eq!(alias_report(Some("ls"), false), None);
        assert_eq!(alias_report(None, false), None);
        assert_eq!(DEFAULT_ALIAS, "mp");
    }

    /// The short name: the name is checked per shell, and the definition
    /// comes after the functions, on both platforms, guarded by a check that
    /// the name is free; `None` (--no-alias) prints none.
    #[test]
    fn init_alias() {
        for sh in ["zsh", "bash", "fish"] {
            let kws: &[&str] = if sh == "fish" { &["case", "if", "for", "while", "function", "end", "begin", "switch"] } else { &["case", "esac", "if", "then", "fi", "for", "while", "function", "time"] };
            for kw in kws {
                assert!(check_alias(kw, sh).is_err(), "{sh}: reserved word {kw}");
            }
            for ok in ["mp", "m-p", "m_p", "mp2", "MP", "x"] {
                assert_eq!(check_alias(ok, sh), Ok(()), "{sh} {ok}");
            }
            for bad in ["", "-mp", "m p", "m;p", "m$p", "m/p", "mp\n", "mp=x", "npm", "yarn", "matchapack", "cd", "echo", "command", "builtin", "set", "source", "test", "eval"] {
                assert!(check_alias(bad, sh).is_err(), "{sh} {bad:?}");
            }
            for mac in [true, false] {
                let plain = init_script(sh, mac, None);
                let with = init_script(sh, mac, Some("mp"));
                assert!(!plain.contains("__MATCHAPACK_ALIAS__") && !plain.contains("--wraps matchapack") && !plain.lines().any(|l| l.starts_with("alias ")) && !plain.contains("alias mp") && !plain.contains("function mp"), "{sh}: no alias with --no-alias");
                assert!(with.starts_with(&plain), "{sh}: the alias comes after the functions");
                let added = &with[plain.len()..];
                assert!(!added.contains("__MATCHAPACK_ALIAS__"), "{sh}: placeholder replaced");
                let def: Vec<&str> = added.lines().filter(|l| !l.starts_with('#')).collect();
                if sh == "fish" {
                    // Defined only when the name is free (`type -q`: function, builtin, command).
                    assert_eq!(def, ["type -q mp; or function mp --wraps matchapack --description 'matchapack'", "    matchapack $argv", "end"]);
                } else {
                    // Defined only when the name is free (`command -v`: alias, function, builtin, command);
                    // a function, with the `function` keyword (see shell/matchapack-alias.sh).
                    assert_eq!(def, ["command -v mp >/dev/null 2>&1 || function mp { matchapack \"$@\"; }"]);
                }
            }
        }
        // Per shell: zsh's own builtins are refused for zsh only, fish's for fish.
        assert!(check_alias("zstyle", "zsh").is_err() && check_alias("zstyle", "bash").is_ok() && check_alias("zstyle", "fish").is_ok());
        assert!(check_alias("string", "fish").is_err() && check_alias("string", "zsh").is_ok());
        assert!(check_alias("shopt", "bash").is_err() && check_alias("shopt", "zsh").is_err());
    }

    #[test]
    fn shell_names_from_ps() {
        assert_eq!(shell_from_comm("zsh"), Some("zsh"));
        assert_eq!(shell_from_comm("/bin/zsh"), Some("zsh"));
        assert_eq!(shell_from_comm("-zsh"), Some("zsh"));
        assert_eq!(shell_from_comm("/opt/homebrew/bin/fish"), Some("fish"));
        assert_eq!(shell_from_comm("-bash"), Some("bash"));
        assert_eq!(shell_from_comm("sh"), None);
        assert_eq!(shell_from_comm("tmux"), None);
        assert_eq!(shell_from_comm(""), None);
    }

    /// The `yarn` function comes after the `npm` one on macOS only, in every
    /// shell; this platform's binary prints what its platform calls for.
    #[test]
    fn init_defines_yarn_on_macos_only() {
        for sh in ["zsh", "bash", "fish"] {
            let (mac, linux) = (init_script(sh, true, None), init_script(sh, false, None));
            let has = |t: &str, f: &str| t.lines().any(|l| l.starts_with(&format!("function {f} ")));
            assert!(has(&mac, "npm") && has(&mac, "yarn"), "{sh} macOS");
            assert!(has(&linux, "npm") && !has(&linux, "yarn"), "{sh} Linux");
            assert!(mac.starts_with(&linux), "{sh}: the npm part is the same");
            assert!(mac.contains("matchapack yarn"));
            assert_eq!(has(&init_script(sh, cfg!(target_os = "macos"), None), "yarn"), cfg!(target_os = "macos"));
        }
    }
}
