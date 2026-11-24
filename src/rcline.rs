//! matchapack's line in a shell startup file: which file a shell reads it
//! from, the exact line, and whether a file already has it. `matchapack init`
//! run by hand uses this to show the line and the file, and, after a yes,
//! to append the line at the end of the file (`setup`,
//! grimmerk/matchapack#20). It never rewrites or reorders a file.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Where a shell's startup files are: the user's home and the variables
/// that move them. Taken from the process environment by `from_env`, or
/// given by tests.
pub struct Dirs {
    pub home: PathBuf,
    /// `ZDOTDIR`: zsh reads its files (other than ~/.zshenv) there.
    pub zdotdir: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`: fish's configuration directory is `<it>/fish`.
    pub xdg_config_home: Option<PathBuf>,
}

impl Dirs {
    /// None without HOME: then there is no startup file to name.
    pub fn from_env() -> Option<Dirs> {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Some(Dirs { home: var("HOME")?, zdotdir: var("ZDOTDIR"), xdg_config_home: var("XDG_CONFIG_HOME") })
    }

    fn zdot(&self) -> PathBuf {
        self.zdotdir.clone().unwrap_or_else(|| self.home.clone())
    }

    /// fish's `$__fish_config_dir`: `$XDG_CONFIG_HOME/fish`, else `~/.config/fish`.
    fn fish_dir(&self) -> PathBuf {
        self.xdg_config_home.clone().unwrap_or_else(|| self.home.join(".config")).join("fish")
    }

    /// The file the line goes in, for `shell` (zsh, bash or fish).
    pub fn startup_file(&self, shell: &str) -> PathBuf {
        match shell {
            "fish" => self.fish_dir().join("config.fish"),
            "bash" => self.home.join(".bashrc"),
            _ => self.zdot().join(".zshrc"),
        }
    }

    /// The files every interactive `shell` reads, a login shell or not: a
    /// line there reaches every new terminal.
    pub fn nonlogin_files(&self, shell: &str) -> Vec<PathBuf> {
        match shell {
            "fish" => {
                let dir = self.fish_dir();
                let mut v = vec![dir.join("config.fish")];
                if let Ok(rd) = std::fs::read_dir(dir.join("conf.d")) {
                    let mut confd: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "fish")).collect();
                    confd.sort();
                    v.extend(confd);
                }
                v
            }
            "bash" => vec![self.home.join(".bashrc")],
            _ => {
                // ~/.zshenv too when ZDOTDIR is set: ZDOTDIR is usually set in
                // ~/.zshenv itself, which zsh then reads before ZDOTDIR's files.
                let z = self.zdot();
                let mut v = vec![self.home.join(".zshenv"), z.join(".zshenv"), z.join(".zshrc")];
                v.dedup();
                v
            }
        }
    }

    /// The files only a login shell reads. Terminal and iTerm on macOS open
    /// login shells; a terminal on Linux usually does not.
    pub fn login_files(&self, shell: &str) -> Vec<PathBuf> {
        match shell {
            "fish" => vec![],
            // A login bash reads only the first of these that exists.
            "bash" => [".bash_profile", ".bash_login", ".profile"].iter().map(|f| self.home.join(f)).find(|p| p.exists()).into_iter().collect(),
            _ => {
                let z = self.zdot();
                vec![z.join(".zprofile"), z.join(".zlogin")]
            }
        }
    }

    /// `path` with the home directory written as `~`, as a user types it.
    pub fn show(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }

    /// `path` for a command line: `show`'s form when it needs no quoting,
    /// else double-quoted, with the home directory as `$HOME`.
    pub fn shell_path(&self, path: &Path) -> String {
        let shown = self.show(path);
        if shown.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~+,:@".contains(c)) {
            return shown;
        }
        let quote = |s: &str| s.chars().fold(String::new(), |mut q, c| {
            if "\\\"$`".contains(c) {
                q.push('\\');
            }
            q.push(c);
            q
        });
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("\"$HOME/{}\"", quote(&rest.display().to_string())),
            Err(_) => format!("\"{}\"", quote(&path.display().to_string())),
        }
    }
}

/// The short-name arguments as the user gave them to `init`.
#[derive(Debug, PartialEq)]
pub enum Alias<'a> {
    Default,
    Named(&'a str),
    None,
}

impl<'a> Alias<'a> {
    /// From `init`'s options: an explicit `--alias mp` stays in the line.
    pub fn from_args(no_alias: bool, alias: Option<&'a str>) -> Alias<'a> {
        match (no_alias, alias) {
            (true, _) => Alias::None,
            (false, Some(n)) => Alias::Named(n),
            (false, None) => Alias::Default,
        }
    }
}

/// matchapack's line for `shell`'s startup file, the README's form. `named`:
/// the user named the shell to `init`, and the line names it too.
pub fn line(shell: &str, named: bool, alias: &Alias) -> String {
    let mut args = if named { format!(" {shell}") } else { String::new() };
    match alias {
        Alias::Default => {}
        Alias::Named(n) => args.push_str(&format!(" --alias {n}")),
        Alias::None => args.push_str(" --no-alias"),
    }
    match shell {
        "fish" => format!("type -q matchapack; and matchapack init{args} | source"),
        _ => format!("command -v matchapack >/dev/null && eval \"$(matchapack init{args})\""),
    }
}

/// The lines of `files` that run `matchapack init`, outside a comment (a
/// line that starts with `#`, or the part after ` #`). A plain text match:
/// it also accepts the user's own forms of the line (a full path to the
/// binary, `source <(…)`).
pub fn existing(files: &[PathBuf]) -> Vec<(PathBuf, usize, String)> {
    crate::uninstall::rc_lines(files).into_iter().filter(|(_, _, l)| code(l).is_some_and(|c| c.contains("matchapack init"))).collect()
}

/// A startup file's line without its comment: None for a line that is a
/// comment (it starts with `#`), else the part before ` #`, trimmed.
fn code(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let c = t.find(" #").or_else(|| t.find("\t#")).map_or(t, |i| &t[..i]).trim_end();
    (!c.starts_with('#')).then_some(c)
}

/// Where the shell already runs matchapack init: the first such line of the
/// files every interactive shell reads, else, on macOS only (a new terminal
/// there is a login shell, which reads both sets), of the login-only files.
/// Also the login-only files' lines, which a Linux terminal skips.
type Found = (Option<(PathBuf, usize, String)>, Vec<(PathBuf, usize, String)>);
fn found(dirs: &Dirs, shell: &str, macos: bool) -> Found {
    let every = existing(&dirs.nonlogin_files(shell));
    let login = existing(&dirs.login_files(shell));
    let first = every.into_iter().next().or(if macos { login.first().cloned() } else { None });
    (first, login)
}

/// What is said when `file:n` already runs matchapack init.
fn already(dirs: &Dirs, file: &Path, n: usize, macos: bool) -> String {
    format!(
        "matchapack init: {}:{n} already runs matchapack init, so new terminals have the {}. If `type npm` in a new terminal does not say it is a shell function, move that line below the lines that put matchapack and Node on PATH.",
        dirs.show(file),
        if macos { "npm and yarn functions" } else { "npm function" }
    )
}

/// The notes that follow the line to add: a login-only file that has the
/// line already (read by login shells only, and on Linux a terminal starts
/// a non-login shell), and macOS's login bash.
fn notes(dirs: &Dirs, shell: &str, macos: bool, login: &[(PathBuf, usize, String)]) -> String {
    let mut s = String::new();
    if let Some((file, n, _)) = login.first() {
        s.push_str(&format!(
            " {}:{n} runs matchapack init already, but only login shells read that file, and a terminal on Linux usually starts a non-login shell.",
            dirs.show(file)
        ));
    }
    if shell == "bash" && macos {
        s.push_str(" Terminal on macOS opens login shells, which read ~/.bash_profile: make sure it loads ~/.bashrc.");
    }
    s
}

/// The command that appends the line, for the user to paste.
fn command(dirs: &Dirs, shell: &str, the_line: &str) -> String {
    if shell == "fish" {
        format!("mkdir -p $__fish_config_dir; and echo '{the_line}' >> $__fish_config_dir/config.fish")
    } else {
        format!("echo '{the_line}' >> {}", dirs.shell_path(&dirs.startup_file(shell)))
    }
}

/// What `matchapack init`, run by hand, says when it does not write the
/// file: the command that adds the line, or where the line already is.
pub fn hint(dirs: &Dirs, shell: &str, named: bool, alias: &Alias, macos: bool) -> String {
    let (first, login) = found(dirs, shell, macos);
    if let Some((file, n, _)) = first {
        return already(dirs, &file, n, macos);
    }
    format!(
        "matchapack init: its shell code does nothing until your shell runs it at every start. To turn it on, add this line at the end of {}, then open a new terminal:\n\n  {}\n\nKeep it below every line that puts matchapack or Node on PATH.{}",
        dirs.show(&dirs.startup_file(shell)),
        command(dirs, shell, &line(shell, named, alias)),
        notes(dirs, shell, macos, &login)
    )
}

/// Whether a startup file's line puts matchapack itself on PATH: the
/// install script's env.sh (or env.fish) line, Homebrew's `brew shellenv`,
/// or cargo's env file. The init line must come after such a line, or its
/// `command -v matchapack` guard skips it. Node's PATH lines do not matter:
/// the npm function looks npm up when it runs.
pub fn puts_matchapack_on_path(line: &str) -> bool {
    code(line).is_some_and(|c| crate::uninstall::is_installer_line(c) || c.contains("brew shellenv") || c.contains(".cargo/env"))
}

/// The lines of `file` after line `n` that put matchapack on PATH.
fn path_lines_after(file: &Path, n: usize) -> Vec<(usize, String)> {
    let Ok(text) = std::fs::read(file) else { return vec![] };
    String::from_utf8_lossy(&text).lines().enumerate().skip(n).filter(|(_, l)| puts_matchapack_on_path(l)).map(|(i, l)| (i + 1, l.to_string())).collect()
}

/// Why `setup` must not write the file, even after a yes: run as root (under
/// sudo, HOME may still be the user's, and the file would be written by
/// root), or bash on macOS whose login profile never loads ~/.bashrc (so a
/// line there would not run in Terminal).
fn no_write_reason(dirs: &Dirs, shell: &str, macos: bool, root: bool) -> Option<String> {
    if root {
        return Some("matchapack init: running as root, so it does not write the file: under sudo, HOME can still be your own, and a startup file written by root is the wrong thing to leave there. Run `matchapack init` as yourself, or add the line by hand.".to_string());
    }
    if shell == "bash" && macos {
        // A login bash reads only the first of its profiles that exists.
        let profile = dirs.login_files("bash").into_iter().next()?;
        let text = std::fs::read(&profile).ok()?;
        let loads = String::from_utf8_lossy(&text).lines().any(|l| code(l).is_some_and(|c| c.contains(".bashrc")));
        if !loads {
            let p = dirs.show(&profile);
            return Some(format!(
                "matchapack init: Terminal on macOS opens login shells, which read {p}, and {p} does not load ~/.bashrc: a line in ~/.bashrc would not run there, so it does not write it. Load ~/.bashrc from {p} (for example, add `[ -f ~/.bashrc ] && . ~/.bashrc` to it) and run `matchapack init` again, or add matchapack's line to {p} yourself."
            ));
        }
    }
    None
}

/// `file` as shown, and the file it really is when it is a symbolic link.
fn show_with_target(dirs: &Dirs, file: &Path) -> String {
    let shown = dirs.show(file);
    if !std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink()) {
        return shown;
    }
    let Ok(target) = std::fs::canonicalize(file) else { return shown };
    // The home directory as the target spells it (macOS: /private/var/… for /var/…).
    let home = std::fs::canonicalize(&dirs.home).unwrap_or_else(|_| dirs.home.clone());
    let t = match target.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => target.display().to_string(),
    };
    format!("{shown} (a symbolic link to {t})")
}

/// How `setup` gets its answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Ask on the terminal (stdin is one).
    Ask,
    /// `--yes`: write without asking.
    Yes,
    /// `--dry-run`: say what it would do; write nothing, ask nothing.
    DryRun,
    /// Neither a question nor a write: print the command to paste (stdin
    /// is not a terminal, or `--print`).
    Hint,
}

/// `setup`'s inputs besides the files and the line's arguments.
pub struct Setup<'a> {
    pub mode: Mode,
    /// The shell was named to `init`: the line names it too.
    pub named: bool,
    /// The short name the line will define, for the next step's `type`.
    pub short: Option<&'a str>,
    /// Running as root (effective user id 0).
    pub root: bool,
}

/// What `setup` did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The line was appended to this file (the path as given, not a
    /// symbolic link's target), as this line number.
    Written(PathBuf, usize),
    /// A file the shell reads runs matchapack init already, at this line.
    Already(PathBuf, usize),
    /// Asked, and the answer was no.
    Declined,
    /// `--dry-run`: it would append the line to this file.
    WouldWrite(PathBuf),
    /// `Mode::Hint`: the command to paste was printed.
    Hinted,
    /// It must not write (`no_write_reason`): the reason and the command
    /// to paste were printed.
    Refused,
}

/// `matchapack init` run by hand: find whether the shell runs matchapack
/// init already (the rule of `hint`), and if not, show the file and the
/// line, and append it after a yes (`ask` gets the question, and says
/// whether the answer is yes). Messages go to `out`. Err only when the
/// write fails; its message names the file and the command to paste.
pub fn setup(dirs: &Dirs, shell: &str, alias: &Alias, macos: bool, s: &Setup, ask: &mut dyn FnMut(&str) -> bool, out: &mut dyn Write) -> anyhow::Result<Outcome> {
    // Messages are best effort: a closed stderr must not undo a write.
    let mut say = |m: &str| {
        let _ = writeln!(out, "{m}");
    };
    let (first, login) = found(dirs, shell, macos);
    if let Some((file, n, init_line)) = first {
        say(&already(dirs, &file, n, macos));
        let after = path_lines_after(&file, n);
        if let Some((last, _)) = after.last() {
            let shown = dirs.show(&file);
            let mut m = format!(
                "matchapack init: warning: in {shown}, line {n} runs matchapack init before a line that puts matchapack on PATH. When line {n} runs, matchapack may not be on PATH yet, and then the line does nothing:\n\n  {shown}:{n}: {init_line}"
            );
            for (i, l) in &after {
                m.push_str(&format!("\n  {shown}:{i}: {l}"));
            }
            m.push_str(&format!("\n\nMove line {n} below line {last}; matchapack does not move lines."));
            say(&m);
        }
        return Ok(Outcome::Already(file, n));
    }
    if s.mode == Mode::Hint {
        say(&hint(dirs, shell, s.named, alias, macos));
        return Ok(Outcome::Hinted);
    }
    let file = dirs.startup_file(shell);
    let shown = dirs.show(&file);
    let the_line = line(shell, s.named, alias);
    if let Some(why) = no_write_reason(dirs, shell, macos, s.root) {
        say(&format!("{why}\n\n{}", hint(dirs, shell, s.named, alias, macos)));
        return Ok(Outcome::Refused);
    }
    let notes = notes(dirs, shell, macos, &login);
    say(&format!(
        "matchapack init: no file your shell reads at start runs matchapack init yet. This line turns it on; it goes at the end of {}, after a blank line:\n\n  {the_line}\n{}",
        show_with_target(dirs, &file),
        if notes.is_empty() { String::new() } else { format!("\n{}\n", notes.trim_start()) }
    ));
    match s.mode {
        Mode::DryRun => {
            say(&format!("matchapack init: dry run: {shown} was not changed. The same command with --yes in place of --dry-run appends the line."));
            return Ok(Outcome::WouldWrite(file));
        }
        Mode::Ask if !ask(&format!("Add it at the end of {shown}? Nothing else in the file changes. [y/N] ")) => {
            say(&format!("matchapack init: nothing was written.\n\n{}", hint(dirs, shell, s.named, alias, macos)));
            return Ok(Outcome::Declined);
        }
        _ => {}
    }
    let n = append(&file, &the_line).map_err(|e| anyhow::anyhow!("could not add the line to {shown}: {e}. Add it yourself:\n\n  {}", command(dirs, shell, &the_line)))?;
    let mut next = "`type npm` should say it is a shell function".to_string();
    if let Some(name) = s.short {
        next.push_str(&format!(", and `type {name}` likewise"));
    }
    say(&format!(
        "matchapack init: added line {n} at the end of {}:\n\n  {the_line}\n\nNext: open a new terminal; {next}. To undo, delete that line ({shown}:{n}).",
        show_with_target(dirs, &file)
    ));
    Ok(Outcome::Written(file, n))
}

/// Append `the_line` at the end of `file`, after a blank line (and after a
/// newline first when the file's last line has none), and return its line
/// number. It creates the file and its directory when missing, writes
/// through a symbolic link (O_APPEND, never a new file renamed over the
/// old), and never changes what is there.
pub fn append(file: &Path, the_line: &str) -> std::io::Result<usize> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).open(file)?;
    // Read through the same open file: what the write is appended to.
    let mut old = vec![];
    std::io::Read::read_to_end(&mut f, &mut old)?;
    let lead = if old.last().is_some_and(|b| *b != b'\n') { "\n" } else { "" };
    f.write_all(format!("{lead}\n{the_line}\n").as_bytes())?;
    f.flush()?;
    let lines = old.iter().filter(|b| **b == b'\n').count() + lead.len();
    Ok(lines + 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::scratch;
    use std::fs;

    fn dirs(home: &Path) -> Dirs {
        Dirs { home: home.to_path_buf(), zdotdir: None, xdg_config_home: None }
    }

    #[test]
    fn the_line_follows_the_alias_arguments() {
        assert_eq!(line("zsh", false, &Alias::Default), r#"command -v matchapack >/dev/null && eval "$(matchapack init)""#);
        assert_eq!(line("bash", false, &Alias::Named("mpk")), r#"command -v matchapack >/dev/null && eval "$(matchapack init --alias mpk)""#);
        assert_eq!(line("fish", false, &Alias::None), "type -q matchapack; and matchapack init --no-alias | source");
    }

    #[test]
    fn startup_files_follow_zdotdir_and_xdg() {
        let d = Dirs { home: PathBuf::from("/h"), zdotdir: Some(PathBuf::from("/z")), xdg_config_home: Some(PathBuf::from("/x")) };
        assert_eq!(d.startup_file("zsh"), PathBuf::from("/z/.zshrc"));
        assert_eq!(d.startup_file("bash"), PathBuf::from("/h/.bashrc"));
        assert_eq!(d.startup_file("fish"), PathBuf::from("/x/fish/config.fish"));
        assert_eq!(dirs(Path::new("/h")).startup_file("fish"), PathBuf::from("/h/.config/fish/config.fish"));
        assert_eq!(d.nonlogin_files("zsh"), vec![PathBuf::from("/h/.zshenv"), PathBuf::from("/z/.zshenv"), PathBuf::from("/z/.zshrc")]);
        assert_eq!(d.login_files("zsh"), vec![PathBuf::from("/z/.zprofile"), PathBuf::from("/z/.zlogin")]);
        assert_eq!(d.nonlogin_files("bash"), vec![PathBuf::from("/h/.bashrc")]);
    }

    #[test]
    fn hint_gives_the_command_when_no_file_has_the_line() {
        let h = scratch("rcline-none");
        // A line that only mentions matchapack, and a commented-out init line: neither counts.
        fs::write(h.join(".zshrc"), "export MATCHAPACK_PATHS=x\n# eval \"$(matchapack init)\"\nexport A=1 # matchapack init later\n").unwrap();
        let s = hint(&dirs(&h), "zsh", false, &Alias::Default, false);
        assert!(s.contains(r#"echo 'command -v matchapack >/dev/null && eval "$(matchapack init)"' >> ~/.zshrc"#), "{s}");
        assert!(s.contains("at the end of ~/.zshrc"), "{s}");
        let b = hint(&dirs(&h), "bash", false, &Alias::Named("mpk"), true);
        assert!(b.contains(r#"eval "$(matchapack init --alias mpk)"' >> ~/.bashrc"#) && b.contains("~/.bash_profile"), "{b}");
        let f = hint(&dirs(&h), "fish", false, &Alias::Default, false);
        assert!(f.contains("mkdir -p $__fish_config_dir; and echo 'type -q matchapack; and matchapack init | source' >> $__fish_config_dir/config.fish"), "{f}");
    }

    #[test]
    fn an_explicit_alias_stays_in_the_line() {
        assert_eq!(Alias::from_args(false, Some("mp")), Alias::Named("mp"));
        assert_eq!(Alias::from_args(false, None), Alias::Default);
        assert_eq!(Alias::from_args(true, Some("x")), Alias::None);
    }

    #[test]
    fn a_path_with_spaces_is_quoted_in_the_command() {
        let h = scratch("rcline-quote");
        let d = Dirs { home: h.clone(), zdotdir: Some(h.join("My Config")), xdg_config_home: None };
        assert_eq!(d.shell_path(&h.join(".zshrc")), "~/.zshrc");
        assert_eq!(d.shell_path(&h.join("My Config/.zshrc")), "\"$HOME/My Config/.zshrc\"");
        assert_eq!(d.shell_path(Path::new("/o/a $b")), "\"/o/a \\$b\"");
        let s = hint(&d, "zsh", false, &Alias::Default, false);
        assert!(s.contains(">> \"$HOME/My Config/.zshrc\""), "{s}");
    }

    #[test]
    fn a_login_only_file_counts_on_macos_only() {
        let h = scratch("rcline-login");
        fs::write(h.join(".bash_profile"), "eval \"$(matchapack init)\"\n").unwrap();
        // macOS: Terminal opens login shells, which read ~/.bash_profile.
        assert!(hint(&dirs(&h), "bash", false, &Alias::Default, true).contains("~/.bash_profile:1 already runs"));
        // Linux: a new terminal reads only ~/.bashrc, so the line is still to add.
        let s = hint(&dirs(&h), "bash", false, &Alias::Default, false);
        assert!(s.contains(">> ~/.bashrc") && s.contains("~/.bash_profile:1 runs matchapack init already, but only login shells"), "{s}");
    }

    #[test]
    fn a_login_bash_reads_only_the_first_profile_that_exists() {
        let h = scratch("rcline-bash-profile");
        fs::write(h.join(".bash_profile"), "export A=1\n").unwrap();
        fs::write(h.join(".profile"), "eval \"$(matchapack init)\"\n").unwrap();
        // ~/.bash_profile exists, so a login bash never reads ~/.profile.
        assert_eq!(dirs(&h).login_files("bash"), vec![h.join(".bash_profile")]);
        assert!(hint(&dirs(&h), "bash", false, &Alias::Default, true).contains(">> ~/.bashrc"));
        fs::remove_file(h.join(".bash_profile")).unwrap();
        assert!(hint(&dirs(&h), "bash", false, &Alias::Default, true).contains("~/.profile:1 already runs"));
    }

    #[test]
    fn hint_names_the_line_that_is_already_there() {
        let h = scratch("rcline-present");
        fs::write(h.join(".zshrc"), "export PATH=x\ncommand -v matchapack >/dev/null && eval \"$(matchapack init)\"\n").unwrap();
        let s = hint(&dirs(&h), "zsh", false, &Alias::Default, true);
        assert!(s.contains("~/.zshrc:2 already runs matchapack init") && s.contains("npm and yarn functions"), "{s}");
        // bash reads another set of files: zsh's line does not count for it.
        assert!(hint(&dirs(&h), "bash", false, &Alias::Default, true).contains("echo '"));
        fs::create_dir_all(h.join(".config/fish/conf.d")).unwrap();
        fs::write(h.join(".config/fish/conf.d/mp.fish"), "type -q matchapack; and matchapack init | source\n").unwrap();
        assert!(hint(&dirs(&h), "fish", false, &Alias::Default, false).contains("~/.config/fish/conf.d/mp.fish:1 already"));
    }

    #[test]
    fn a_named_shell_stays_in_the_line() {
        assert_eq!(line("zsh", true, &Alias::Default), r#"command -v matchapack >/dev/null && eval "$(matchapack init zsh)""#);
        assert_eq!(line("fish", true, &Alias::Named("mpk")), "type -q matchapack; and matchapack init fish --alias mpk | source");
    }

    const ZSH_LINE: &str = r#"command -v matchapack >/dev/null && eval "$(matchapack init)""#;

    /// `setup` in `mode` (not root, the shell not named, short name `mp`)
    /// with an answer of `answer` (None: asking is a failure), and what it
    /// said: (outcome, messages, the questions asked).
    fn run(d: &Dirs, shell: &str, alias: &Alias, macos: bool, mode: Mode, answer: Option<bool>) -> (anyhow::Result<Outcome>, String, Vec<String>) {
        run_as(d, shell, alias, macos, &Setup { mode, named: false, short: Some("mp"), root: false }, answer)
    }

    fn run_as(d: &Dirs, shell: &str, alias: &Alias, macos: bool, s: &Setup, answer: Option<bool>) -> (anyhow::Result<Outcome>, String, Vec<String>) {
        let mut asked = vec![];
        let mut ask = |q: &str| {
            asked.push(q.to_string());
            answer.expect("asked, but this run must not ask")
        };
        let mut out = vec![];
        let r = setup(d, shell, alias, macos, s, &mut ask, &mut out);
        (r, String::from_utf8(out).unwrap(), asked)
    }

    #[test]
    fn a_yes_appends_the_line_after_a_blank_line() {
        let h = scratch("setup-yes");
        fs::write(h.join(".zshrc"), "export A=1\n").unwrap();
        let (r, out, asked) = run(&dirs(&h), "zsh", &Alias::Default, true, Mode::Ask, Some(true));
        assert_eq!(r.unwrap(), Outcome::Written(h.join(".zshrc"), 3));
        assert_eq!(asked, vec!["Add it at the end of ~/.zshrc? Nothing else in the file changes. [y/N] "]);
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), format!("export A=1\n\n{ZSH_LINE}\n"));
        assert!(out.contains(&format!("at the end of ~/.zshrc, after a blank line:\n\n  {ZSH_LINE}")), "{out}");
        assert!(out.contains("added line 3 at the end of ~/.zshrc") && out.contains("open a new terminal; `type npm` should say it is a shell function, and `type mp` likewise."), "{out}");
        assert!(out.contains("To undo, delete that line (~/.zshrc:3)."), "{out}");
    }

    /// The file after `append` is the file before it plus the appended
    /// bytes: a newline first when the last line has none, a blank line,
    /// the line. Nothing before them changes.
    #[test]
    fn append_only_adds_bytes_at_the_end() {
        let h = scratch("setup-bytes");
        let l = "the line";
        for (name, before, added, n) in [
            ("missing", None, "\nthe line\n", 2),
            ("empty", Some(""), "\nthe line\n", 2),
            ("no-newline", Some("a\nb"), "\n\nthe line\n", 4),
            ("newline", Some("a\nb\n"), "\nthe line\n", 4),
            ("blank-end", Some("a\n\n"), "\nthe line\n", 4),
        ] {
            let f = h.join(name);
            if let Some(b) = before {
                fs::write(&f, b).unwrap();
            }
            assert_eq!(append(&f, l).unwrap(), n, "{name}");
            assert_eq!(fs::read_to_string(&f).unwrap(), format!("{}{added}", before.unwrap_or("")), "{name}");
            assert_eq!(fs::read_to_string(&f).unwrap().lines().nth(n - 1), Some(l), "{name}: the line number");
        }
    }

    #[test]
    fn a_missing_file_and_its_directory_are_created() {
        let h = scratch("setup-missing");
        run(&dirs(&h), "zsh", &Alias::Default, false, Mode::Yes, None).0.unwrap();
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), format!("\n{ZSH_LINE}\n"));
        let (r, _, _) = run(&dirs(&h), "fish", &Alias::Default, false, Mode::Ask, Some(true));
        assert_eq!(r.unwrap(), Outcome::Written(h.join(".config/fish/config.fish"), 2));
        assert_eq!(fs::read_to_string(h.join(".config/fish/config.fish")).unwrap(), "\ntype -q matchapack; and matchapack init | source\n");
        // ZDOTDIR that does not exist yet.
        let d = Dirs { home: h.clone(), zdotdir: Some(h.join("zdot")), xdg_config_home: None };
        run(&d, "zsh", &Alias::Default, false, Mode::Yes, None).0.unwrap_or_else(|e| panic!("{e}"));
        assert!(fs::read_to_string(h.join("zdot/.zshrc")).unwrap().contains("matchapack init"));
    }

    #[test]
    fn a_no_dry_run_or_hint_writes_nothing() {
        let h = scratch("setup-no");
        fs::write(h.join(".zshrc"), "export A=1\n").unwrap();
        let (r, out, asked) = run(&dirs(&h), "zsh", &Alias::Default, false, Mode::Ask, Some(false));
        assert_eq!(r.unwrap(), Outcome::Declined);
        assert_eq!(asked.len(), 1);
        assert!(out.contains("nothing was written") && out.contains(&format!("echo '{ZSH_LINE}' >> ~/.zshrc")), "{out}");
        let (r, out, _) = run(&dirs(&h), "zsh", &Alias::Default, false, Mode::DryRun, None);
        assert_eq!(r.unwrap(), Outcome::WouldWrite(h.join(".zshrc")));
        assert!(out.contains(ZSH_LINE) && out.contains("dry run: ~/.zshrc was not changed"), "{out}");
        let (r, out, _) = run(&dirs(&h), "zsh", &Alias::Default, false, Mode::Hint, None);
        assert_eq!(r.unwrap(), Outcome::Hinted);
        assert!(out.contains(&format!("echo '{ZSH_LINE}' >> ~/.zshrc")), "{out}");
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), "export A=1\n");
        // A missing file stays missing.
        run(&dirs(&h), "fish", &Alias::Default, false, Mode::DryRun, None).0.unwrap();
        assert!(!h.join(".config").exists());
    }

    #[test]
    fn already_set_up_writes_nothing() {
        let h = scratch("setup-already");
        // Lines that put Node or other things on PATH after it are not warned about.
        let rc = format!("export PATH=/a:$PATH\n{ZSH_LINE}\nexport PATH=/b:$PATH\n. \"$NVM_DIR/nvm.sh\"\n# eval \"$(brew shellenv)\"\n");
        fs::write(h.join(".zshrc"), &rc).unwrap();
        for mode in [Mode::Ask, Mode::Yes, Mode::DryRun, Mode::Hint] {
            let (r, out, asked) = run(&dirs(&h), "zsh", &Alias::Default, false, mode, None);
            assert_eq!(r.unwrap(), Outcome::Already(h.join(".zshrc"), 2));
            assert!(asked.is_empty());
            assert!(out.contains("~/.zshrc:2 already runs matchapack init") && !out.contains("warning"), "{out}");
            assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), rc);
        }
        // Run twice with --yes: one line.
        let h = scratch("setup-twice");
        run(&dirs(&h), "bash", &Alias::Default, false, Mode::Yes, None).0.unwrap();
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, false, Mode::Yes, None).0.unwrap(), Outcome::Already(h.join(".bashrc"), 2));
        assert_eq!(fs::read_to_string(h.join(".bashrc")).unwrap(), format!("\n{ZSH_LINE}\n"));
    }

    #[test]
    fn a_line_that_puts_matchapack_on_path_after_it_is_warned_about() {
        let h = scratch("setup-path-after");
        let rc = format!("{ZSH_LINE}\nexport PATH=/x:$PATH\neval \"$(/opt/homebrew/bin/brew shellenv)\"\n. \"$HOME/.config/matchapack/env.sh\"\n. \"$HOME/.cargo/env\"\n");
        fs::write(h.join(".zshrc"), &rc).unwrap();
        let (r, out, _) = run(&dirs(&h), "zsh", &Alias::Default, false, Mode::Ask, None);
        assert_eq!(r.unwrap(), Outcome::Already(h.join(".zshrc"), 1));
        assert!(out.contains("warning: in ~/.zshrc, line 1 runs matchapack init before a line that puts matchapack on PATH"), "{out}");
        let listed: Vec<&str> = out.lines().filter(|l| l.starts_with("  ~/.zshrc:")).collect();
        assert_eq!(
            listed,
            vec![
                format!("  ~/.zshrc:1: {ZSH_LINE}").as_str(),
                "  ~/.zshrc:3: eval \"$(/opt/homebrew/bin/brew shellenv)\"",
                "  ~/.zshrc:4: . \"$HOME/.config/matchapack/env.sh\"",
                "  ~/.zshrc:5: . \"$HOME/.cargo/env\"",
            ],
            "{out}"
        );
        assert!(out.contains("Move line 1 below line 5; matchapack does not move lines."), "{out}");
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), rc, "nothing is moved");
    }

    #[test]
    fn only_lines_that_put_matchapack_on_path_count() {
        for l in ["eval \"$(/opt/homebrew/bin/brew shellenv)\"", ". \"$HOME/.config/matchapack/env.sh\"", "source \"$HOME/.config/matchapack/env.fish\"", ". \"$HOME/.cargo/env\""] {
            assert!(puts_matchapack_on_path(l), "{l}");
        }
        for l in ["# eval \"$(brew shellenv)\"", "export PATH=/x:$PATH", "path+=(/x)", "eval \"$(fnm env)\"", ". \"$NVM_DIR/nvm.sh\"", "fish_add_path /x", ZSH_LINE] {
            assert!(!puts_matchapack_on_path(l), "{l}");
        }
    }

    #[test]
    fn on_linux_a_login_only_line_still_asks_for_bashrc() {
        let h = scratch("setup-linux-login");
        fs::write(h.join(".profile"), "eval \"$(matchapack init)\"\n").unwrap();
        let (r, out, asked) = run(&dirs(&h), "bash", &Alias::Default, false, Mode::Ask, Some(true));
        assert_eq!(r.unwrap(), Outcome::Written(h.join(".bashrc"), 2));
        assert_eq!(asked, vec!["Add it at the end of ~/.bashrc? Nothing else in the file changes. [y/N] "]);
        assert!(out.contains("~/.profile:1 runs matchapack init already, but only login shells read that file"), "{out}");
        // On macOS the same file counts: nothing to ask.
        let (r, _, _) = run(&dirs(&h), "bash", &Alias::Default, true, Mode::Ask, None);
        assert_eq!(r.unwrap(), Outcome::Already(h.join(".bashrc"), 2));
        fs::remove_file(h.join(".bashrc")).unwrap();
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, true, Mode::Ask, None).0.unwrap(), Outcome::Already(h.join(".profile"), 1));
    }

    #[test]
    fn macos_bash_whose_profile_skips_bashrc_is_not_written() {
        let h = scratch("setup-mac-profile");
        fs::write(h.join(".bash_profile"), "export A=1\n# . ~/.bashrc\n").unwrap();
        for mode in [Mode::Ask, Mode::Yes, Mode::DryRun] {
            let (r, out, _) = run(&dirs(&h), "bash", &Alias::Default, true, mode, None);
            assert_eq!(r.unwrap(), Outcome::Refused, "{mode:?}");
            assert!(out.contains("~/.bash_profile does not load ~/.bashrc") && out.contains(">> ~/.bashrc"), "{out}");
        }
        assert!(!h.join(".bashrc").exists());
        // Linux: a terminal starts a non-login bash, which reads ~/.bashrc.
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, false, Mode::Yes, None).0.unwrap(), Outcome::Written(h.join(".bashrc"), 2));
        // A profile that loads ~/.bashrc, or none at all: written.
        let h = scratch("setup-mac-profile-loads");
        fs::write(h.join(".bash_profile"), "[ -f ~/.bashrc ] && . ~/.bashrc\n").unwrap();
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, true, Mode::Yes, None).0.unwrap(), Outcome::Written(h.join(".bashrc"), 2));
    }

    #[test]
    fn root_is_not_written_for() {
        let h = scratch("setup-root");
        let s = Setup { mode: Mode::Yes, named: false, short: None, root: true };
        let (r, out, _) = run_as(&dirs(&h), "zsh", &Alias::Default, false, &s, None);
        assert_eq!(r.unwrap(), Outcome::Refused);
        assert!(out.contains("running as root") && out.contains(&format!("echo '{ZSH_LINE}' >> ~/.zshrc")), "{out}");
        assert!(!h.join(".zshrc").exists());
    }

    #[test]
    fn the_arguments_go_into_the_written_line() {
        let h = scratch("setup-alias");
        run(&dirs(&h), "zsh", &Alias::Named("mpk"), false, Mode::Yes, None).0.unwrap();
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), "\ncommand -v matchapack >/dev/null && eval \"$(matchapack init --alias mpk)\"\n");
        let s = Setup { mode: Mode::Yes, named: true, short: None, root: false };
        let (_, out, _) = run_as(&dirs(&h), "fish", &Alias::None, false, &s, None);
        assert_eq!(fs::read_to_string(h.join(".config/fish/config.fish")).unwrap(), "\ntype -q matchapack; and matchapack init fish --no-alias | source\n");
        // Without a short name, the next step names npm only.
        assert!(out.contains("a shell function.") && !out.contains("`type mp`"), "{out}");
    }

    #[test]
    fn a_symlinked_file_is_appended_through_the_link() {
        let h = scratch("setup-link");
        fs::create_dir_all(h.join("dotfiles")).unwrap();
        fs::write(h.join("dotfiles/zshrc"), "export A=1").unwrap();
        std::os::unix::fs::symlink(h.join("dotfiles/zshrc"), h.join(".zshrc")).unwrap();
        let (r, out, _) = run(&dirs(&h), "zsh", &Alias::Default, false, Mode::Yes, None);
        assert_eq!(r.unwrap(), Outcome::Written(h.join(".zshrc"), 3));
        assert!(fs::symlink_metadata(h.join(".zshrc")).unwrap().file_type().is_symlink(), "the link is kept");
        assert_eq!(fs::read_to_string(h.join("dotfiles/zshrc")).unwrap(), format!("export A=1\n\n{ZSH_LINE}\n"));
        assert!(out.contains("added line 3 at the end of ~/.zshrc (a symbolic link to ~/dotfiles/zshrc)"), "{out}");
    }

    #[test]
    fn a_failed_write_is_an_error_and_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let h = scratch("setup-readonly");
        fs::write(h.join(".zshrc"), "export A=1\n").unwrap();
        fs::set_permissions(h.join(".zshrc"), fs::Permissions::from_mode(0o444)).unwrap();
        let (r, _, _) = run(&dirs(&h), "zsh", &Alias::Default, false, Mode::Yes, None);
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("could not add the line to ~/.zshrc") && e.contains(&format!("Add it yourself:\n\n  echo '{ZSH_LINE}' >> ~/.zshrc")), "{e}");
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), "export A=1\n");
    }
}
