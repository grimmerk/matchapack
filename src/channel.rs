//! How the running matchapack was installed, read from where its binary
//! really is (links resolved) and from the records next to it, never from
//! a guess: Homebrew's Cellar, an npm package (a global npm install, under
//! a Node version manager or not; mise's and Volta's own packages; any other
//! `node_modules`), cargo's install records, the install script's receipt.
//! `uninstall` uses it to say how to remove the binary, `init` to warn when
//! switching Node versions would lose matchapack.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The install script's receipt, in its directory (`receipt_home`).
pub const RECEIPT: &str = "matchapack-receipt.json";
/// The files the install script writes in its directory besides the
/// receipt: the scripts that put its install directory on PATH, which a
/// line it appends to the shell startup files loads.
pub const ENV_SCRIPTS: &[&str] = &["env.sh", "env.fish"];
/// The file the install script writes for fish, under `~/.config/fish`
/// whatever XDG_CONFIG_HOME says (it holds only the line loading env.fish).
pub const FISH_CONF_D: &str = ".config/fish/conf.d/matchapack.env.fish";

#[derive(Debug, PartialEq)]
pub enum Channel {
    /// In `<prefix>/Cellar/matchapack/<version>/`.
    Homebrew,
    /// In `<prefix>/lib/node_modules/matchapack/` (`npm i -g`); `manager`:
    /// the Node version manager whose Node version `prefix` is, if any.
    NpmGlobal { prefix: PathBuf, manager: Option<(NodeManager, String)> },
    /// mise's npm backend (`mise use -g npm:matchapack`): `…/installs/npm-matchapack/<version>/`.
    MiseNpm,
    /// A Volta package (`volta install matchapack`): `…/image/packages/matchapack/`.
    Volta,
    /// Any other `node_modules/matchapack/` (a project's, another package
    /// manager's global directory): `dir` is that package directory.
    OtherPackage { dir: PathBuf },
    /// Listed in cargo's install records of `root` (`<root>/bin/matchapack`).
    Cargo { root: PathBuf },
    /// The install script's: its receipt names the binary's directory.
    Installer { receipt: PathBuf },
    /// None of those.
    Plain,
}

/// A Node version manager that keeps each Node version, and its global npm
/// packages, in a directory of its own.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum NodeManager {
    Nvm,
    Fnm,
    Mise,
    Asdf,
    Nodenv,
    Volta,
}

impl NodeManager {
    pub fn name(self) -> &'static str {
        match self {
            NodeManager::Nvm => "nvm",
            NodeManager::Fnm => "fnm",
            NodeManager::Mise => "mise",
            NodeManager::Asdf => "asdf",
            NodeManager::Nodenv => "nodenv",
            NodeManager::Volta => "Volta",
        }
    }
}

fn name(p: Option<&Path>) -> Option<&OsStr> {
    p.and_then(Path::file_name)
}

fn is(p: Option<&Path>, n: &str) -> bool {
    name(p) == Some(OsStr::new(n))
}

/// The Node version manager whose Node version is `prefix` (a global npm
/// prefix), with that version: nvm `$NVM_DIR/versions/node/<v>`, fnm
/// `…/node-versions/<v>/installation`, mise `…/installs/node/<v>`, asdf
/// `…/installs/nodejs/<v>`, nodenv `~/.nodenv/versions/<v>`, Volta's own
/// Node `…/tools/image/node/<v>` (reached only when Volta does not take
/// `npm i -g` over).
pub fn node_manager(prefix: &Path) -> Option<(NodeManager, String)> {
    let (p1, p2) = (prefix.parent(), prefix.parent().and_then(Path::parent));
    let version = |p: Option<&Path>| name(p).map(|n| n.to_string_lossy().into_owned());
    if is(Some(prefix), "installation") && is(p2, "node-versions") {
        return Some((NodeManager::Fnm, version(p1)?));
    }
    let m = if (is(p1, "node") || is(p1, "io.js")) && is(p2, "versions") {
        NodeManager::Nvm
    } else if is(p1, "node") && is(p2, "installs") {
        NodeManager::Mise
    } else if is(p1, "nodejs") && is(p2, "installs") {
        NodeManager::Asdf
    } else if is(p1, "versions") && is(p2, ".nodenv") {
        NodeManager::Nodenv
    } else if is(p1, "node") && is(p2, "image") {
        NodeManager::Volta
    } else {
        return None;
    };
    Some((m, version(Some(prefix))?))
}

/// What the binary's resolved path alone says: Homebrew's Cellar or an npm
/// package (the package directory nearest to the binary decides).
pub fn by_path(real: &Path) -> Option<Channel> {
    for a in real.ancestors().skip(1) {
        let parent = a.parent();
        if !is(Some(a), "matchapack") {
            continue;
        }
        if is(parent, "Cellar") {
            return Some(Channel::Homebrew);
        }
        if !is(parent, "node_modules") {
            continue;
        }
        // The package directory: who put it there.
        let above: Vec<&Path> = a.ancestors().skip(2).collect();
        if above.iter().any(|d| is(Some(d), "npm-matchapack") && is(d.parent(), "installs")) {
            return Some(Channel::MiseNpm);
        }
        if above.iter().any(|d| is(Some(d), "matchapack") && is(d.parent(), "packages") && is(d.parent().and_then(Path::parent), "image")) {
            return Some(Channel::Volta);
        }
        let nm = parent.unwrap();
        if is(nm.parent(), "lib") {
            let prefix = nm.parent().and_then(Path::parent).unwrap_or(nm).to_path_buf();
            let manager = node_manager(&prefix);
            return Some(Channel::NpmGlobal { prefix, manager });
        }
        return Some(Channel::OtherPackage { dir: a.to_path_buf() });
    }
    None
}

/// Whether cargo's install records in `root` (`.crates2.json`, or the older
/// `.crates.toml`) list the package `matchapack` with the binary `bin`.
fn cargo_lists(root: &Path, bin: &str) -> bool {
    let ours = |key: &str| key.starts_with("matchapack ");
    if let Some(v) = std::fs::read_to_string(root.join(".crates2.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
        let listed = v["installs"].as_object().is_some_and(|m| {
            m.iter().any(|(k, i)| ours(k) && i["bins"].as_array().is_some_and(|b| b.iter().any(|x| x.as_str() == Some(bin))))
        });
        if listed {
            return true;
        }
    }
    // `[v1]` then `"matchapack 0.1.0 (…)" = ["matchapack"]`, one per line.
    let toml = std::fs::read_to_string(root.join(".crates.toml")).unwrap_or_default();
    toml.lines().any(|l| {
        l.strip_prefix('"').is_some_and(|rest| ours(rest)) && l.rsplit_once('=').is_some_and(|(_, bins)| bins.contains(&format!("\"{bin}\"")))
    })
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The directory the install script's receipt at `receipt` says it put the
/// binary in, when the receipt is there and lists `bin`.
pub fn receipt_dir(receipt: &Path, bin: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(receipt).ok()?).ok()?;
    let prefix = PathBuf::from(v["install_prefix"].as_str()?);
    if !v["binaries"].as_array().is_some_and(|b| b.iter().any(|x| x.as_str() == Some(bin))) {
        return None;
    }
    // dist's layouts: "flat" puts the binary in the prefix, "cargo-home"
    // and "hierarchical" in its bin/.
    Some(match v["install_layout"].as_str() {
        Some("cargo-home" | "hierarchical") => prefix.join("bin"),
        _ => prefix,
    })
}

/// The binary `exe` resolved, and how it was installed. `cargo_home`: where
/// a plain `cargo install` puts it (its records decide, not the place);
/// `receipt_home`: the install script's directory
/// (`${XDG_CONFIG_HOME:-~/.config}/matchapack`).
pub fn detect(exe: &Path, cargo_home: &Path, receipt_home: &Path) -> (PathBuf, Channel) {
    let real = canon(exe);
    if let Some(c) = by_path(&real) {
        return (real, c);
    }
    let bin = real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(root) = real.parent().filter(|d| is(Some(d), "bin")).and_then(Path::parent) {
        if cargo_lists(root, &bin) {
            let root = if canon(root) == canon(cargo_home) { cargo_home.to_path_buf() } else { root.to_path_buf() };
            return (real, Channel::Cargo { root });
        }
    }
    let receipt = receipt_home.join(RECEIPT);
    if receipt_dir(&receipt, &bin).is_some_and(|d| Some(canon(&d)) == real.parent().map(canon)) {
        return (real, Channel::Installer { receipt });
    }
    (real, Channel::Plain)
}

/// What `matchapack init` says when it is run by hand (`tty`: stdout is a
/// terminal; never from the startup file's line, which would print it at
/// every shell start) and the binary `exe` is a global npm package of one
/// Node version of a version manager: switching versions loses it.
pub fn init_note(exe: Option<&Path>, tty: bool) -> Option<String> {
    if !tty {
        return None;
    }
    let Some(Channel::NpmGlobal { prefix, manager: Some((m, version)) }) = by_path(&canon(exe?)) else { return None };
    let keep = match m {
        NodeManager::Nvm => "; or add matchapack to $NVM_DIR/default-packages, which nvm installs in each new Node version",
        NodeManager::Mise => "; or `mise use -g npm:matchapack`, kept apart from any Node version",
        _ => "; or install it in each Node version you use",
    };
    Some(format!(
        "matchapack init: this matchapack is a global npm package of Node {version} ({}, in {}): after you switch to another Node version, `matchapack` is not on PATH and this line skips it without a word (npm and Yarn then run plain). To keep it, install it with Homebrew or the install script, which do not depend on the Node version{keep}.",
        m.name(),
        prefix.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::scratch;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// A file at `d/<rel>` (directories made), and its path.
    fn file(d: &Path, rel: &str) -> PathBuf {
        let p = d.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "bin").unwrap();
        p
    }

    /// Each channel from a binary at its real place (synthetic files under a
    /// scratch directory: nothing outside it is read).
    #[test]
    fn channels_from_real_paths() {
        let d = std::fs::canonicalize(scratch("channel")).unwrap();
        let cargo_home = d.join("home/.cargo");
        let receipts = d.join("home/.config/matchapack");
        let det = |exe: &Path| detect(exe, &cargo_home, &receipts).1;
        // Homebrew, run through its link in <prefix>/bin.
        let cellar = file(&d, "opt/homebrew/Cellar/matchapack/0.1.0/bin/matchapack");
        fs::create_dir_all(d.join("opt/homebrew/bin")).unwrap();
        symlink("../Cellar/matchapack/0.1.0/bin/matchapack", d.join("opt/homebrew/bin/matchapack")).unwrap();
        let (real, c) = detect(&d.join("opt/homebrew/bin/matchapack"), &cargo_home, &receipts);
        assert_eq!((real, c), (cellar.clone(), Channel::Homebrew), "the link is resolved");
        // npm global, plain and under each version manager.
        let npm = |prefix: &str| file(&d, &format!("{prefix}/lib/node_modules/matchapack/native/darwin-arm64/matchapack"));
        assert_eq!(det(&npm("usr/local")), Channel::NpmGlobal { prefix: d.join("usr/local"), manager: None });
        for (prefix, m, v) in [
            ("home/.nvm/versions/node/v22.11.0", NodeManager::Nvm, "v22.11.0"),
            ("home/.local/share/fnm/node-versions/v20.18.0/installation", NodeManager::Fnm, "v20.18.0"),
            ("home/.local/share/mise/installs/node/22.11.0", NodeManager::Mise, "22.11.0"),
            ("home/.asdf/installs/nodejs/22.11.0", NodeManager::Asdf, "22.11.0"),
            ("home/.nodenv/versions/22.11.0", NodeManager::Nodenv, "22.11.0"),
            ("home/.volta/tools/image/node/22.11.0", NodeManager::Volta, "22.11.0"),
        ] {
            assert_eq!(det(&npm(prefix)), Channel::NpmGlobal { prefix: d.join(prefix), manager: Some((m, v.to_string())) }, "{prefix}");
        }
        // fnm runs it through a per-shell link to the version's directory.
        fs::create_dir_all(d.join("home/.local/state/fnm_multishells")).unwrap();
        symlink(d.join("home/.local/share/fnm/node-versions/v20.18.0/installation"), d.join("home/.local/state/fnm_multishells/123_456")).unwrap();
        let via = d.join("home/.local/state/fnm_multishells/123_456/lib/node_modules/matchapack/native/darwin-arm64/matchapack");
        assert!(matches!(det(&via), Channel::NpmGlobal { manager: Some((NodeManager::Fnm, _)), .. }), "{:?}", det(&via));
        // mise's and Volta's own packages: not a Node version's.
        assert_eq!(det(&npm("home/.local/share/mise/installs/npm-matchapack/0.1.0")), Channel::MiseNpm);
        assert_eq!(det(&npm("home/.volta/tools/image/packages/matchapack")), Channel::Volta);
        // Other node_modules: a project's, pnpm's and Yarn's global ones.
        for rel in [
            "work/app/node_modules/matchapack/native/linux-x64/matchapack",
            "home/Library/pnpm/global/5/node_modules/.pnpm/matchapack@0.1.0/node_modules/matchapack/native/darwin-arm64/matchapack",
            "home/.config/yarn/global/node_modules/matchapack/native/darwin-arm64/matchapack",
        ] {
            let c = det(&file(&d, rel));
            assert!(matches!(&c, Channel::OtherPackage { dir } if dir.ends_with("node_modules/matchapack")), "{rel}: {c:?}");
        }
        // cargo: by its records, at CARGO_HOME or another root; a binary in
        // ~/.cargo/bin that cargo did not record is a plain file.
        let in_cargo = file(&cargo_home, "bin/matchapack");
        assert_eq!(det(&in_cargo), Channel::Plain, "not recorded");
        fs::write(cargo_home.join(".crates2.json"), r#"{"installs":{"matchapack 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)":{"version_req":null,"bins":["matchapack"]}}}"#).unwrap();
        assert_eq!(det(&in_cargo), Channel::Cargo { root: cargo_home.clone() });
        let other = d.join("tools");
        let in_root = file(&other, "bin/matchapack");
        fs::write(other.join(".crates.toml"), "[v1]\n\"matchapack 0.1.0 (path+file:///src/matchapack)\" = [\"matchapack\"]\n").unwrap();
        assert_eq!(det(&in_root), Channel::Cargo { root: other.clone() });
        fs::write(other.join(".crates.toml"), "[v1]\n\"matchapack-other 0.1.0 (path+file:///x)\" = [\"matchapack\"]\n").unwrap();
        assert_eq!(det(&in_root), Channel::Plain, "another package's binary of that name");
        // The install script: the receipt names the binary's directory.
        let local = file(&d, "home/.local/bin/matchapack");
        assert_eq!(det(&local), Channel::Plain, "no receipt");
        fs::create_dir_all(&receipts).unwrap();
        let receipt = |prefix: &Path, layout: &str| {
            let r = format!(
                r#"{{"install_prefix":"{}","install_layout":"{layout}","binaries":["matchapack"],"source":{{"app_name":"matchapack"}},"provider":{{"source":"cargo-dist","version":"0.34.0"}},"modify_path":true}}"#,
                prefix.display()
            );
            fs::write(receipts.join(RECEIPT), r).unwrap();
        };
        receipt(&d.join("home/.local/bin"), "flat");
        assert_eq!(det(&local), Channel::Installer { receipt: receipts.join(RECEIPT) });
        assert_eq!(det(&file(&d, "elsewhere/matchapack")), Channel::Plain, "the receipt names another directory");
        receipt(&d.join("home/.local"), "hierarchical");
        assert_eq!(det(&file(&d, "home/.local/bin/matchapack")), Channel::Installer { receipt: receipts.join(RECEIPT) }, "<prefix>/bin");
        // cargo's records come before a receipt naming the same directory.
        receipt(&cargo_home, "cargo-home");
        assert_eq!(det(&in_cargo), Channel::Cargo { root: cargo_home.clone() });
        // Anything else: the file itself.
        assert_eq!(det(&file(&d, "bin/matchapack")), Channel::Plain);
        let _ = fs::remove_dir_all(&d);
    }

    /// The init note: on a terminal only, for a global npm package of a
    /// version manager's Node version only.
    #[test]
    fn init_note_only_for_a_node_versions_package_on_a_terminal() {
        let d = std::fs::canonicalize(scratch("channel-init")).unwrap();
        let nvm = file(&d, ".nvm/versions/node/v22.11.0/lib/node_modules/matchapack/native/darwin-arm64/matchapack");
        let note = init_note(Some(&nvm), true).unwrap();
        assert!(note.contains("global npm package of Node v22.11.0 (nvm") && note.contains("switch to another Node version") && note.contains("Homebrew or the install script") && note.contains("default-packages"), "{note}");
        assert_eq!(init_note(Some(&nvm), false), None, "never from the startup file's line");
        let mise = file(&d, "mise/installs/node/22.11.0/lib/node_modules/matchapack/native/darwin-arm64/matchapack");
        assert!(init_note(Some(&mise), true).unwrap().contains("mise use -g npm:matchapack"));
        for rel in [
            "usr/local/lib/node_modules/matchapack/native/darwin-arm64/matchapack",
            "mise/installs/npm-matchapack/0.1.0/lib/node_modules/matchapack/native/darwin-arm64/matchapack",
            "opt/homebrew/Cellar/matchapack/0.1.0/bin/matchapack",
            "home/.local/bin/matchapack",
        ] {
            assert_eq!(init_note(Some(&file(&d, rel)), true), None, "{rel}");
        }
        assert_eq!(init_note(None, true), None);
        let _ = fs::remove_dir_all(&d);
    }
}
