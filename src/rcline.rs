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
/// On macOS a ~/.bashrc line that a login bash never reads (`unread`)
/// comes after a login-only file's line. Also the login-only files' lines,
/// which a Linux terminal skips.
type Found = (Option<(PathBuf, usize, String)>, Vec<(PathBuf, usize, String)>);
fn found(dirs: &Dirs, shell: &str, macos: bool) -> Found {
    let every = existing(&dirs.nonlogin_files(shell));
    let login = existing(&dirs.login_files(shell));
    let login_first = if macos { login.first().cloned() } else { None };
    let first = match every.into_iter().next() {
        Some(e) if unread(dirs, &e.0, macos).is_some() => login_first.or(Some(e)),
        Some(e) => Some(e),
        None => login_first,
    };
    (first, login)
}

/// What is said when `file:n` already runs matchapack init; on macOS, when
/// that is ~/.bashrc and a login bash does not read it (`unread`), that it
/// does not run in a new terminal, and what makes it run.
fn already(dirs: &Dirs, file: &Path, n: usize, macos: bool) -> String {
    if let Some(s) = unread(dirs, file, macos) {
        return format!("matchapack init: {}:{n} runs matchapack init, but {}, so a new terminal does not run that line. {}, then open a new terminal.", dirs.show(file), s.why, s.fix);
    }
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

/// Whether the process runs as root (effective user id `euid` 0) in a home
/// directory that belongs to another user: `sudo matchapack init` on macOS
/// keeps the user's HOME. Root in its own home (`/root` in a container)
/// is not this case, nor a home whose owner cannot be read.
pub fn root_in_other_home(euid: u32, home: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    euid == 0 && std::fs::metadata(home).is_ok_and(|m| m.uid() != 0)
}

/// Why `setup` must not write the file, even after a yes: root in another
/// user's home (`root_in_other_home`: the file would be written by root),
/// or bash on macOS whose login bash never reads ~/.bashrc (no profile, or
/// one that does not load it), so a line there would not run in Terminal.
fn no_write_reason(dirs: &Dirs, shell: &str, macos: bool, root_in_other_home: bool) -> Option<String> {
    if root_in_other_home {
        return Some(format!("matchapack init: running as root in {}, a home directory that belongs to another user (as under sudo), so it does not write the file: a startup file written by root does not belong there. Run `matchapack init` as that user, or add the line by hand.", dirs.home.display()));
    }
    if shell == "bash" && macos {
        if let Some(s) = login_bash_skips_bashrc(dirs) {
            return Some(format!(
                "matchapack init: {}: a line in ~/.bashrc would not run there, so it does not write it. {} and run `matchapack init` again, or add matchapack's line to {} yourself.",
                s.why,
                s.fix,
                dirs.show(&s.profile)
            ));
        }
    }
    None
}

/// Why a login bash does not read ~/.bashrc, and what makes it.
struct BashSkip {
    why: String,
    fix: String,
    /// The profile a login bash reads (~/.bash_profile when none exists).
    profile: PathBuf,
}

/// Terminal on macOS opens login shells, and a login bash reads ~/.bashrc
/// only when the first of ~/.bash_profile, ~/.bash_login and ~/.profile
/// that exists loads it (`loads_bashrc`). None when it does. A profile that
/// cannot be read counts as one that does not.
fn login_bash_skips_bashrc(dirs: &Dirs) -> Option<BashSkip> {
    let load = "`[ -f ~/.bashrc ] && . ~/.bashrc`";
    let Some(profile) = dirs.login_files("bash").into_iter().next() else {
        return Some(BashSkip {
            why: "Terminal on macOS opens login shells, and a login bash reads only ~/.bash_profile, ~/.bash_login or ~/.profile, none of which exists here".to_string(),
            fix: format!("Create ~/.bash_profile containing {load}"),
            profile: dirs.home.join(".bash_profile"),
        });
    };
    let p = dirs.show(&profile);
    let why = match std::fs::read(&profile) {
        Ok(text) if String::from_utf8_lossy(&text).lines().any(|l| loads_bashrc(dirs, l)) => return None,
        Ok(_) => format!("Terminal on macOS opens login shells, which read {p}, and {p} does not load ~/.bashrc"),
        Err(e) => format!("Terminal on macOS opens login shells, which read {p}, and matchapack could not read {p} to see whether it loads ~/.bashrc ({e})"),
    };
    Some(BashSkip { why, fix: format!("Load ~/.bashrc from {p} (for example, add {load} to it)"), profile })
}

/// Whether a profile's line loads ~/.bashrc: a `.` or `source` command
/// whose file, as bash expands it, is ~/.bashrc itself. The words come
/// from `shell_words`, so `~/.bashrc`, `"$HOME/.bashrc"`, `${HOME}/.bashrc`
/// and the home directory spelled out count, and `"~/.bashrc"` or
/// `'$HOME/.bashrc'`, which bash does not expand, do not. A mention
/// (`echo ~/.bashrc`) or another file (`~/.bashrc.local`) does not count.
/// A form it does not recognize (a variable holding the path, a loop)
/// counts as not loading it, the safe side: `setup` then writes nothing
/// and says what to do. As `existing` does, it reads one line's text
/// only: a line in a function or a branch that never runs, or inside a
/// string or here-document that spans lines, still counts.
fn loads_bashrc(dirs: &Dirs, line: &str) -> bool {
    let bashrc = dirs.home.join(".bashrc");
    let words = shell_words(line, &dirs.home.to_string_lossy());
    // Each command of the line: the words between separators.
    words.split(Option::is_none).any(|cmd| {
        let mut w = cmd.iter().flatten().map(String::as_str).skip_while(|w| ["then", "do", "else", "{"].contains(w));
        matches!(w.next(), Some("." | "source")) && w.next().is_some_and(|f| Path::new(f) == bashrc)
    })
}

/// A line of a bash startup file split as bash splits it, for
/// `loads_bashrc`: each word (`Some`) with its quotes and backslashes
/// removed, and `;`, `&`, `|`, `(`, `)`, `<` and `>` outside quotes as
/// separators (`None`). Of bash's expansions it does only the ones a path
/// to ~/.bashrc uses: `~` that starts an unquoted word (before `/` or at
/// its end), and `$HOME` or `${HOME}` outside single quotes (unquoted, a
/// value with a space or `*?[`, which bash would split or expand, gives a
/// word that matches no path); any other stays as written. An unquoted
/// `#` that starts a word ends the line.
fn shell_words(line: &str, home: &str) -> Vec<Option<String>> {
    let mut out = vec![];
    let mut word: Option<String> = None;
    let mut cs = line.chars().peekable();
    // `$HOME` or `${HOME}` at the `$` just read: consumes it and returns true.
    fn home_var(cs: &mut std::iter::Peekable<std::str::Chars>) -> bool {
        let rest: String = cs.clone().take(7).collect();
        let n = if rest.starts_with("{HOME}") {
            6
        } else if rest.starts_with("HOME") && !rest[4..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            4
        } else {
            return false;
        };
        for _ in 0..n {
            cs.next();
        }
        true
    }
    while let Some(c) = cs.next() {
        match c {
            ' ' | '\t' | '\n' => out.extend(word.take().map(Some)),
            ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                out.extend(word.take().map(Some));
                out.push(None);
            }
            '#' if word.is_none() => break,
            '\\' => word.get_or_insert_default().extend(cs.next()),
            '\'' => {
                let w = word.get_or_insert_default();
                w.extend(cs.by_ref().take_while(|c| *c != '\''));
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(c) = cs.next() {
                    match c {
                        '"' => break,
                        '\\' if cs.peek().is_some_and(|n| "\"\\$`".contains(*n)) => w.extend(cs.next()),
                        '$' if home_var(&mut cs) => w.push_str(home),
                        _ => w.push(c),
                    }
                }
            }
            '~' if word.is_none() && cs.peek().is_none_or(|n| *n == '/' || n.is_whitespace() || ";&|()<>".contains(*n)) => word = Some(home.to_string()),
            '$' if home_var(&mut cs) => {
                let w = word.get_or_insert_default();
                // Unquoted, bash splits the value at spaces and expands
                // `*?[` in it: then the word is not the path. A NUL, which
                // no path holds, keeps it from matching.
                if home.contains(|c: char| c.is_whitespace() || "*?[".contains(c)) {
                    w.push('\0');
                }
                w.push_str(home);
            }
            _ => word.get_or_insert_default().push(c),
        }
    }
    out.extend(word.take().map(Some));
    out
}

/// On macOS, why a new terminal's bash does not read `file`, when it is
/// ~/.bashrc and the login profile does not load it (`login_bash_skips_bashrc`).
fn unread(dirs: &Dirs, file: &Path, macos: bool) -> Option<BashSkip> {
    (macos && file == dirs.home.join(".bashrc")).then(|| login_bash_skips_bashrc(dirs)).flatten()
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

/// Whether an answer to `setup`'s question is yes: `y` or `yes`, in any
/// case. Anything else, an empty line or EOF included, is no.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
    /// Running as root in another user's home (`root_in_other_home`).
    pub root_in_other_home: bool,
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
    /// to paste were printed. Or, on macOS, ~/.bashrc has the line but a
    /// login bash does not read it (`unread`): that, and how to fix it.
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
        // The line is there, but a new terminal does not read the file.
        if unread(dirs, &file, macos).is_some() {
            return Ok(Outcome::Refused);
        }
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
    if let Some(why) = no_write_reason(dirs, shell, macos, s.root_in_other_home) {
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
    fn only_y_or_yes_is_yes() {
        for a in ["y\n", "Y\n", "yes\n", " YES \n", "Yes"] {
            assert!(is_yes(a), "{a:?}");
        }
        for a in ["", "\n", "n\n", "no\n", "yy\n", "ye\n", "sure\n", "y es\n"] {
            assert!(!is_yes(a), "{a:?}");
        }
    }

    #[test]
    fn a_named_shell_stays_in_the_line() {
        assert_eq!(line("zsh", true, &Alias::Default), r#"command -v matchapack >/dev/null && eval "$(matchapack init zsh)""#);
        assert_eq!(line("fish", true, &Alias::Named("mpk")), "type -q matchapack; and matchapack init fish --alias mpk | source");
    }

    const ZSH_LINE: &str = r#"command -v matchapack >/dev/null && eval "$(matchapack init)""#;

    /// `setup` in `mode` (not root elsewhere, the shell not named, short name `mp`)
    /// with an answer of `answer` (None: asking is a failure), and what it
    /// said: (outcome, messages, the questions asked).
    fn run(d: &Dirs, shell: &str, alias: &Alias, macos: bool, mode: Mode, answer: Option<bool>) -> (anyhow::Result<Outcome>, String, Vec<String>) {
        run_as(d, shell, alias, macos, &Setup { mode, named: false, short: Some("mp"), root_in_other_home: false }, answer)
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
        // On macOS a new terminal reads ~/.profile, which runs the line
        // (and does not load ~/.bashrc): nothing to ask.
        let (r, _, _) = run(&dirs(&h), "bash", &Alias::Default, true, Mode::Ask, None);
        assert_eq!(r.unwrap(), Outcome::Already(h.join(".profile"), 1));
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
        // No profile at all: a login bash reads none, so not ~/.bashrc either.
        let h = scratch("setup-mac-no-profile");
        for mode in [Mode::Ask, Mode::Yes, Mode::DryRun] {
            let (r, out, _) = run(&dirs(&h), "bash", &Alias::Default, true, mode, None);
            assert_eq!(r.unwrap(), Outcome::Refused, "{mode:?}");
            assert!(out.contains("none of which exists here") && out.contains("Create ~/.bash_profile containing `[ -f ~/.bashrc ] && . ~/.bashrc`") && out.contains(">> ~/.bashrc"), "{out}");
        }
        assert_eq!(fs::read_dir(&h).unwrap().count(), 0, "nothing written");
        // A profile that loads ~/.bashrc: written.
        let h = scratch("setup-mac-profile-loads");
        fs::write(h.join(".bash_profile"), "[ -f ~/.bashrc ] && . ~/.bashrc\n").unwrap();
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, true, Mode::Yes, None).0.unwrap(), Outcome::Written(h.join(".bashrc"), 2));
    }

    #[test]
    fn a_profile_loads_bashrc_only_by_sourcing_that_file() {
        let d = dirs(Path::new("/Users/u"));
        for l in [
            "[ -f ~/.bashrc ] && . ~/.bashrc",
            "[[ -r ~/.bashrc ]] && source ~/.bashrc",
            "if [ -f ~/.bashrc ]; then . ~/.bashrc; fi",
            "\t. \"$HOME/.bashrc\"",
            "test -f ~/.bashrc && source \"${HOME}/.bashrc\" # aliases",
            "source \"$HOME\"/.bashrc",
            "[ -s ~/.bashrc ] && \\. ~/.bashrc",
            ". /Users/u/.bashrc",
            ". ~/.bashrc>/dev/null 2>&1",
            "{ . ~/.bashrc; }",
        ] {
            assert!(loads_bashrc(&d, l), "{l}");
        }
        for l in [
            "echo ~/.bashrc",
            "source ~/.bashrc.local",
            ". ~/.bashrc_extra",
            "# . ~/.bashrc",
            "export A=1 # . ~/.bashrc",
            ": # ; . ~/.bashrc",
            "[ -f ~/.bashrc ]",
            "cp ~/.bashrc /tmp",
            ". /Users/other/.bashrc",
            // Forms bash does not expand to the home directory.
            "source \"~/.bashrc\"",
            "source '$HOME/.bashrc'",
            "source \"\\$HOME/.bashrc\"",
            ". \\~/.bashrc",
            ". ~u/.bashrc",
            "source $HOMEDIR/.bashrc",
            // Quoted, the separator is part of the word.
            "echo \"; . ~/.bashrc\"",
        ] {
            assert!(!loads_bashrc(&d, l), "{l}");
        }
        // A home directory with a space: one word when quoted.
        let d = dirs(Path::new("/Users/Jane Doe"));
        for l in ["source \"/Users/Jane Doe/.bashrc\"", ". \"$HOME/.bashrc\"", ". ~/.bashrc", "source /Users/Jane\\ Doe/.bashrc"] {
            assert!(loads_bashrc(&d, l), "{l}");
        }
        // Unquoted, bash splits the path at the space: not loaded.
        for l in ["source /Users/Jane Doe/.bashrc", "source $HOME/.bashrc", ". ${HOME}/.bashrc"] {
            assert!(!loads_bashrc(&d, l), "{l}");
        }
    }

    #[test]
    fn a_profile_that_only_mentions_bashrc_or_cannot_be_read_is_not_written_for() {
        use std::os::unix::fs::PermissionsExt;
        let h = scratch("setup-mac-mention");
        fs::write(h.join(".bash_profile"), "echo ~/.bashrc\nsource ~/.bashrc.local\n").unwrap();
        let (r, out, _) = run(&dirs(&h), "bash", &Alias::Default, true, Mode::Yes, None);
        assert_eq!(r.unwrap(), Outcome::Refused);
        assert!(out.contains("~/.bash_profile does not load ~/.bashrc"), "{out}");
        fs::write(h.join(".bash_profile"), ". ~/.bashrc\n").unwrap();
        fs::set_permissions(h.join(".bash_profile"), fs::Permissions::from_mode(0o000)).unwrap();
        let (r, out, _) = run(&dirs(&h), "bash", &Alias::Default, true, Mode::Yes, None);
        assert_eq!(r.unwrap(), Outcome::Refused);
        assert!(out.contains("could not read ~/.bash_profile to see whether it loads ~/.bashrc"), "{out}");
        assert!(!h.join(".bashrc").exists());
    }

    #[test]
    fn a_bashrc_line_that_a_macos_login_bash_skips_is_not_called_set_up() {
        let h = scratch("setup-mac-skipped-line");
        fs::write(h.join(".bash_profile"), "export A=1\n").unwrap();
        fs::write(h.join(".bashrc"), format!("{ZSH_LINE}\n")).unwrap();
        for mode in [Mode::Ask, Mode::Yes, Mode::DryRun, Mode::Hint] {
            let (r, out, asked) = run(&dirs(&h), "bash", &Alias::Default, true, mode, None);
            assert_eq!(r.unwrap(), Outcome::Refused, "{mode:?}");
            assert!(asked.is_empty());
            assert!(
                out.contains("~/.bashrc:1 runs matchapack init, but Terminal on macOS opens login shells, which read ~/.bash_profile, and ~/.bash_profile does not load ~/.bashrc, so a new terminal does not run that line.")
                    && out.contains("Load ~/.bashrc from ~/.bash_profile")
                    && !out.contains("already runs")
                    && !out.contains(">> ~/.bashrc"),
                "{out}"
            );
        }
        assert!(hint(&dirs(&h), "bash", false, &Alias::Default, true).contains("so a new terminal does not run that line"));
        assert_eq!(fs::read_to_string(h.join(".bashrc")).unwrap(), format!("{ZSH_LINE}\n"), "nothing written");
        // Linux: a terminal's bash reads ~/.bashrc.
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, false, Mode::Yes, None).0.unwrap(), Outcome::Already(h.join(".bashrc"), 1));
        // The profile runs the line itself: set up.
        fs::write(h.join(".bash_profile"), format!("export A=1\n{ZSH_LINE}\n")).unwrap();
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, true, Mode::Yes, None).0.unwrap(), Outcome::Already(h.join(".bash_profile"), 2));
        // The profile loads ~/.bashrc: ~/.bashrc's line counts.
        fs::write(h.join(".bash_profile"), "[ -f ~/.bashrc ] && . ~/.bashrc\n").unwrap();
        assert_eq!(run(&dirs(&h), "bash", &Alias::Default, true, Mode::Yes, None).0.unwrap(), Outcome::Already(h.join(".bashrc"), 1));
    }

    #[test]
    fn root_in_another_users_home_is_not_written_for() {
        use std::os::unix::fs::MetadataExt;
        let h = scratch("setup-root");
        let me = fs::metadata(&h).unwrap().uid();
        assert_ne!(me, 0, "run the tests as a normal user");
        // Root in a home that is not root's (sudo keeps the user's HOME): refused.
        assert!(root_in_other_home(0, &h));
        // Root in its own home (`/` is root's on macOS and Linux, as /root is): written.
        assert!(!root_in_other_home(0, Path::new("/")));
        // Not root; or a home whose owner cannot be read.
        assert!(!root_in_other_home(me, &h));
        assert!(!root_in_other_home(0, &h.join("missing")));
        let s = Setup { mode: Mode::Yes, named: false, short: None, root_in_other_home: root_in_other_home(0, &h) };
        let (r, out, _) = run_as(&dirs(&h), "zsh", &Alias::Default, false, &s, None);
        assert_eq!(r.unwrap(), Outcome::Refused);
        assert!(out.contains("running as root in") && out.contains("belongs to another user") && out.contains(&format!("echo '{ZSH_LINE}' >> ~/.zshrc")), "{out}");
        assert!(!h.join(".zshrc").exists());
    }

    #[test]
    fn the_arguments_go_into_the_written_line() {
        let h = scratch("setup-alias");
        run(&dirs(&h), "zsh", &Alias::Named("mpk"), false, Mode::Yes, None).0.unwrap();
        assert_eq!(fs::read_to_string(h.join(".zshrc")).unwrap(), "\ncommand -v matchapack >/dev/null && eval \"$(matchapack init --alias mpk)\"\n");
        let s = Setup { mode: Mode::Yes, named: true, short: None, root_in_other_home: false };
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
