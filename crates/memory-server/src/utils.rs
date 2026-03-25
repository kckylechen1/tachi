use super::*;

pub(super) fn is_active_global_rule(entry: &MemoryEntry) -> bool {
    entry
        .metadata
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("DRAFT")
        == "ACTIVE"
}

pub(super) fn find_git_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Check if a command is in the trusted allowlist for MCP server spawning.
/// Trusted: common package runners, interpreters, and brew-installed binaries.
pub(super) fn is_trusted_command(cmd: &str) -> bool {
    let basename = std::path::Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);

    const TRUSTED_BASENAMES: &[&str] = &[
        "npx", "node", "bun", "deno", "python3", "python", "uv", "cargo", "rustup", "docker",
        "podman", "tachi",
    ];

    if TRUSTED_BASENAMES.contains(&basename) {
        return true;
    }

    // Allow absolute paths under Homebrew, nvm, cargo, common bin dirs
    const TRUSTED_PREFIXES: &[&str] = &["/opt/homebrew/", "/usr/local/bin/", "/usr/bin/", "/bin/"];

    for prefix in TRUSTED_PREFIXES {
        if cmd.starts_with(prefix) {
            return true;
        }
    }

    // Allow paths under user's home .cargo/bin, .local/bin, .nvm
    if let Ok(home) = std::env::var("HOME") {
        let home_prefixes = [
            format!("{}/.cargo/bin/", home),
            format!("{}/.local/bin/", home),
            format!("{}/.nvm/", home),
            format!("{}/.bun/bin/", home),
        ];
        for prefix in &home_prefixes {
            if cmd.starts_with(prefix.as_str()) {
                return true;
            }
        }
    }

    false
}

pub(super) fn value_to_template_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        v.to_string()
    }
}

/// Stable hash function (FNV-1a). Deterministic across Rust toolchain versions,
/// unlike DefaultHasher which uses SipHash with randomized keys.
pub(super) fn stable_hash(input: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash)
}

pub(super) fn parse_env_bool(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => {
            eprintln!("Ignoring invalid {name} value '{raw}' (expected true/false)");
            None
        }
    }
}

pub(super) fn parse_env_u64(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(value) => Some(value),
        Err(_) => {
            eprintln!("Ignoring invalid {name} value '{raw}' (expected non-negative integer)");
            None
        }
    }
}

pub(super) fn lock_or_recover<'a, T>(
    mutex: &'a StdMutex<T>,
    label: &str,
) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("WARNING: mutex poisoned: {label}; recovering with inner state");
            poisoned.into_inner()
        }
    }
}
