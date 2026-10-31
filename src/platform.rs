//! os / cpu / libc matching, following npm-install-checks' `checkPlatform`:
//! a list matches when it is empty, or the current value is listed and not
//! negated (`!value`), and no negation names the current value.

pub fn current_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        o => o,
    }
}

pub fn current_cpu() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        a => a,
    }
}

/// libc family as npm reports it; `None` off Linux.
pub fn current_libc() -> Option<&'static str> {
    if current_os() != "linux" {
        return None;
    }
    // npm reads the process report; musl systems ship /lib/ld-musl-*.
    let musl = std::fs::read_dir("/lib")
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-")))
        .unwrap_or(false);
    Some(if musl { "musl" } else { "glibc" })
}

pub fn list_ok(list: &[String], value: Option<&str>) -> bool {
    if list.is_empty() {
        return true;
    }
    let Some(value) = value else { return false };
    let mut positive_seen = false;
    let mut matched = false;
    for item in list {
        if let Some(neg) = item.strip_prefix('!') {
            if neg == value {
                return false;
            }
        } else {
            positive_seen = true;
            if item == value || item == "any" {
                matched = true;
            }
        }
    }
    matched || !positive_seen
}

pub fn matches(os: &[String], cpu: &[String], libc: &[String]) -> bool {
    list_ok(os, Some(current_os()))
        && list_ok(cpu, Some(current_cpu()))
        // npm only checks libc on Linux.
        && (current_os() != "linux" || list_ok(libc, current_libc()))
}
