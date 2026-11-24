//! matchapack's line in a shell startup file: which file a shell reads it
//! from, the exact line, and whether a file already has it. `matchapack init`
//! run by hand uses this to say what to add and where (it never writes the
//! file); a later `setup` command that appends the line after asking
//! (grimmerk/matchapack#20) would use the same pieces.

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

/// matchapack's line for `shell`'s startup file, the README's form.
pub fn line(shell: &str, alias: &Alias) -> String {
    let args = match alias {
        Alias::Default => String::new(),
        Alias::Named(n) => format!(" --alias {n}"),
        Alias::None => " --no-alias".to_string(),
    };
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
    crate::uninstall::rc_lines(files)
        .into_iter()
        .filter(|(_, _, l)| {
            let t = l.trim_start();
            let code = t.find(" #").or_else(|| t.find("\t#")).map_or(t, |i| &t[..i]);
            !code.starts_with('#') && code.contains("matchapack init")
        })
        .collect()
}

/// What `matchapack init`, run by hand, says after the shell code: the
/// command that adds the line, or where the line already is.
pub fn hint(dirs: &Dirs, shell: &str, alias: &Alias, macos: bool) -> String {
    let every = existing(&dirs.nonlogin_files(shell));
    let login = existing(&dirs.login_files(shell));
    // On macOS a new terminal is a login shell and reads both sets.
    let found = every.first().or(if macos { login.first() } else { None });
    if let Some((file, n, _)) = found {
        return format!(
            "matchapack init: {}:{n} already runs matchapack init, so new terminals have the {}. If `type npm` in a new terminal does not say it is a shell function, move that line below the lines that put matchapack and Node on PATH.",
            dirs.show(file),
            if macos { "npm and yarn functions" } else { "npm function" }
        );
    }
    let file = dirs.startup_file(shell);
    let the_line = line(shell, alias);
    let command = if shell == "fish" {
        format!("mkdir -p $__fish_config_dir; and echo '{the_line}' >> $__fish_config_dir/config.fish")
    } else {
        format!("echo '{the_line}' >> {}", dirs.shell_path(&file))
    };
    let mut s = format!(
        "matchapack init: the code above does nothing until your shell runs it at every start. To turn it on, add this line at the end of {}, then open a new terminal:\n\n  {command}\n\nKeep it below every line that puts matchapack or Node on PATH.",
        dirs.show(&file)
    );
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
        assert_eq!(line("zsh", &Alias::Default), r#"command -v matchapack >/dev/null && eval "$(matchapack init)""#);
        assert_eq!(line("bash", &Alias::Named("mpk")), r#"command -v matchapack >/dev/null && eval "$(matchapack init --alias mpk)""#);
        assert_eq!(line("fish", &Alias::None), "type -q matchapack; and matchapack init --no-alias | source");
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
        let s = hint(&dirs(&h), "zsh", &Alias::Default, false);
        assert!(s.contains(r#"echo 'command -v matchapack >/dev/null && eval "$(matchapack init)"' >> ~/.zshrc"#), "{s}");
        assert!(s.contains("at the end of ~/.zshrc"), "{s}");
        let b = hint(&dirs(&h), "bash", &Alias::Named("mpk"), true);
        assert!(b.contains(r#"eval "$(matchapack init --alias mpk)"' >> ~/.bashrc"#) && b.contains("~/.bash_profile"), "{b}");
        let f = hint(&dirs(&h), "fish", &Alias::Default, false);
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
        let s = hint(&d, "zsh", &Alias::Default, false);
        assert!(s.contains(">> \"$HOME/My Config/.zshrc\""), "{s}");
    }

    #[test]
    fn a_login_only_file_counts_on_macos_only() {
        let h = scratch("rcline-login");
        fs::write(h.join(".bash_profile"), "eval \"$(matchapack init)\"\n").unwrap();
        // macOS: Terminal opens login shells, which read ~/.bash_profile.
        assert!(hint(&dirs(&h), "bash", &Alias::Default, true).contains("~/.bash_profile:1 already runs"));
        // Linux: a new terminal reads only ~/.bashrc, so the line is still to add.
        let s = hint(&dirs(&h), "bash", &Alias::Default, false);
        assert!(s.contains(">> ~/.bashrc") && s.contains("~/.bash_profile:1 runs matchapack init already, but only login shells"), "{s}");
    }

    #[test]
    fn a_login_bash_reads_only_the_first_profile_that_exists() {
        let h = scratch("rcline-bash-profile");
        fs::write(h.join(".bash_profile"), "export A=1\n").unwrap();
        fs::write(h.join(".profile"), "eval \"$(matchapack init)\"\n").unwrap();
        // ~/.bash_profile exists, so a login bash never reads ~/.profile.
        assert_eq!(dirs(&h).login_files("bash"), vec![h.join(".bash_profile")]);
        assert!(hint(&dirs(&h), "bash", &Alias::Default, true).contains(">> ~/.bashrc"));
        fs::remove_file(h.join(".bash_profile")).unwrap();
        assert!(hint(&dirs(&h), "bash", &Alias::Default, true).contains("~/.profile:1 already runs"));
    }

    #[test]
    fn hint_names_the_line_that_is_already_there() {
        let h = scratch("rcline-present");
        fs::write(h.join(".zshrc"), "export PATH=x\ncommand -v matchapack >/dev/null && eval \"$(matchapack init)\"\n").unwrap();
        let s = hint(&dirs(&h), "zsh", &Alias::Default, true);
        assert!(s.contains("~/.zshrc:2 already runs matchapack init") && s.contains("npm and yarn functions"), "{s}");
        // bash reads another set of files: zsh's line does not count for it.
        assert!(hint(&dirs(&h), "bash", &Alias::Default, true).contains("echo '"));
        fs::create_dir_all(h.join(".config/fish/conf.d")).unwrap();
        fs::write(h.join(".config/fish/conf.d/mp.fish"), "type -q matchapack; and matchapack init | source\n").unwrap();
        assert!(hint(&dirs(&h), "fish", &Alias::Default, false).contains("~/.config/fish/conf.d/mp.fish:1 already"));
    }
}
