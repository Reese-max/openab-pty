//! Injecting the session's tools MCP into a coding CLI's own config (#39).
//!
//! `OPENAB_TOOLS_MCP_URL` puts the loopback endpoint in the child's
//! environment at spawn (CLIENT-CONTRACT §9.3), but nothing used to wire it
//! into the CLI itself: every session needed a manual `kiro-cli mcp add` plus
//! a hand-edited `allowedTools`, and both hard-coded a URL whose key dies with
//! the session's next spawn. This module writes the *session-independent*
//! form instead:
//!
//! - `~/.kiro/settings/mcp.json` gains a `computer` server that runs a small
//!   stdio bridge (`computer-mcp-bridge.js`, shipped inside the binary and
//!   re-written under `~/.local/share/openab-pty/` every spawn) on a JS
//!   runtime. The bridge reads the URL from *its* environment —
//!   `${OPENAB_TOOLS_MCP_URL}` in the server's `env`, the one place Kiro
//!   expands variables — so the one shared file serves every session in the
//!   pod and survives key rotation with no rewrite. A config that named one
//!   session's URL was the live failure behind the field finding on #39: three
//!   sessions, one computer.
//! - every `~/.kiro/agents/*.json` gains `@computer/*` in `allowedTools` — the
//!   platform-neutral alias per #42, covering whatever the sandbox profile
//!   actually serves (never `exec`; the profile does not have it). Agents that
//!   also pin a restrictive `tools` list get `@computer` so the server is
//!   visible, and an agent that opts out of mcp.json (`includeMcpJson: false`)
//!   gets its own copy of the server entry. Agent *files* are never created:
//!   an agent with no file yet keeps the documented `--trust-tools` / prompt
//!   path, which beats risking a file that shadows a built-in agent.
//! - a CLI this module does not know gets nothing at all: §9.3's env-var
//!   manual path is the documented fallback, and "nothing regresses when
//!   `tools_listen` is unset" means this is only ever called when the tools
//!   plane is enabled.
//!
//! Nothing written here carries a secret — the key exists only in each
//! child's environment — so these files are also safe to leave shared across
//! sessions, which is the whole point.

use serde_json::{json, Map, Value};
use std::io;
use std::path::{Path, PathBuf};

/// The MCP alias every agent sees — platform-neutral, not `mac`.
pub const SERVER_NAME: &str = "computer";
/// What `allowedTools` gains: whatever the sandbox profile serves.
pub const TRUST_PATTERN: &str = "@computer/*";
/// What a restrictive `tools` list gains so the server is visible at all.
const VISIBILITY_PATTERN: &str = "@computer";
/// `tools` entries that already cover the whole server without an addition.
const COVERING_TOOLS: &[&str] = &["*", "@mcp", "@computer", "@computer/*"];

const BRIDGE_JS: &str = include_str!("computer-mcp-bridge.js");
const BRIDGE_REL: &str = ".local/share/openab-pty/computer-mcp-bridge.js";

/// What one spawn-time pass did, for logging and tests.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A known CLI variant was configured; `files` lists everything written.
    Injected {
        variant: &'static str,
        files: Vec<PathBuf>,
    },
    /// No CLI whose config shape this runtime knows was found — the env-var
    /// manual path is what the session gets.
    NoKnownVariant,
    /// A kiro CLI was found but neither `bun` nor `node` to run the bridge on.
    /// A `computer` server nothing can execute is worse than none.
    NoJsRuntime { variant: &'static str },
}

/// Wire the tools MCP into every known agent-CLI config under `home`. Called
/// once per spawn: the files are session-independent, so the merge is
/// idempotent and a re-lend or restart needs no separate refresh pass — the
/// next spawn's bridge reads the new generation's URL from its own env.
pub fn inject_tools_mcp(home: &Path, env: &[(String, String)]) -> io::Result<Outcome> {
    let dirs = path_dirs(home, env);
    let Some(kiro) = detect_executable("kiro-cli", home, &dirs) else {
        return Ok(Outcome::NoKnownVariant);
    };
    let Some(js_runtime) = find_js_runtime(&kiro, home, &dirs) else {
        return Ok(Outcome::NoJsRuntime { variant: "kiro" });
    };

    let mut files = Vec::new();
    // Runtime-owned: rewritten every spawn so an outdated bridge self-heals.
    let bridge = home.join(BRIDGE_REL);
    write_atomic(&bridge, BRIDGE_JS)?;
    files.push(bridge.clone());

    let entry = computer_server_entry(&js_runtime, &bridge);
    let mcp = home.join(".kiro/settings/mcp.json");
    if merge_mcp_json(&mcp, &entry)? {
        files.push(mcp);
    }

    // Merge into every agent file that already exists; never create one (a
    // hand-made `kiro_default.json` can shadow the built-in agent).
    let agents = home.join(".kiro/agents");
    if agents.is_dir() {
        for entry_result in std::fs::read_dir(&agents)? {
            // One unreadable entry must not abort the files after it — the
            // merge is per-file, not atomic across the directory.
            let Ok(dir_entry) = entry_result else {
                tracing::warn!(dir = %agents.display(), "could not read an agents dir entry");
                continue;
            };
            let path = dir_entry.path();
            if path.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("json")
                && merge_agent_json(&path, &entry)?
            {
                files.push(path);
            }
        }
    }
    Ok(Outcome::Injected {
        variant: "kiro",
        files,
    })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Directories a spawned `command` can come from: the child's own PATH, plus
/// `~/.local/bin` for the installs that never made it onto PATH. Absolute
/// system dirs are deliberately *not* appended — detection stays a function
/// of the child's environment, not of wherever the runtime happens to run.
fn path_dirs(home: &Path, env: &[(String, String)]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = env
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| std::env::split_paths(value).collect())
        .unwrap_or_default();
    let local_bin = home.join(".local/bin");
    if !dirs.contains(&local_bin) {
        dirs.push(local_bin);
    }
    dirs
}

/// An executable file, following symlinks (an installer's `~/.local/bin`
/// entry usually is one).
fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolved(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

/// `name` on the child's PATH or in the spots its installer uses without one.
fn detect_executable(name: &str, home: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        let candidate = dir.join(name);
        if executable(&candidate) {
            return Some(resolved(candidate));
        }
    }
    // The kiro-cli installer drops binaries under the share dir even when
    // ~/.local/bin never made it onto PATH.
    for candidate in [
        home.join(".local/share/kiro-cli/kiro-cli"),
        home.join(".local/share/kiro-cli/bin/kiro-cli"),
    ] {
        if executable(&candidate) {
            return Some(resolved(candidate));
        }
    }
    None
}

/// What can execute the bridge: the runtime bundled beside `kiro-cli` first
/// (the shape verified against the real deployment), then a PATH-visible
/// `bun`, then `node`. The bridge is portable JS for exactly this fallback.
fn find_js_runtime(kiro_cli: &Path, home: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = kiro_cli.parent() {
        candidates.push(dir.join("bun"));
    }
    candidates.push(home.join(".local/share/kiro-cli/bun"));
    candidates.push(home.join(".local/share/kiro-cli/bin/bun"));
    for name in ["bun", "node"] {
        candidates.extend(dirs.iter().map(|dir| dir.join(name)));
    }
    candidates
        .into_iter()
        .find(|c| usable_js_runtime(c))
        .map(resolved)
}

/// `executable`, plus a version probe for `node` — a node older than v18 has
/// no `fetch`, so a `computer` server pointing at it would fail on its first
/// request, which is worse than falling back to the env-var manual path.
/// `bun` has always had `fetch`.
fn usable_js_runtime(path: &Path) -> bool {
    if !executable(path) {
        return false;
    }
    if path.file_name().and_then(|n| n.to_str()) != Some("node") {
        return true;
    }
    let Ok(output) = std::process::Command::new(path).arg("--version").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .strip_prefix('v')
        .and_then(|v| v.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 18)
}

// ---------------------------------------------------------------------------
// Config merge
// ---------------------------------------------------------------------------

/// The verified stdio shape: nothing in it names a session, a URL, or a key.
fn computer_server_entry(runtime: &Path, bridge: &Path) -> Value {
    json!({
        "command": runtime.to_string_lossy(),
        "args": [bridge.to_string_lossy()],
        "env": { "OPENAB_TOOLS_MCP_URL": "${OPENAB_TOOLS_MCP_URL}" }
    })
}

enum MergeResult {
    /// The file changed and was (re)written.
    Applied,
    /// Already had what it needed; not rewritten, so the mtime does not churn
    /// on every spawn.
    Unchanged,
    /// Present but not ours to repair — wrong shape or unparseable content is
    /// left byte-identical, never silently replaced.
    Skipped(&'static str),
}

/// Read–mutate–write a JSON config file atomically. A symlinked config is a
/// shared dotfile: the merge writes its target, never replaces the link — and
/// a *dangling* link is left alone entirely rather than overwritten in place,
/// so a dotfile layout whose target appears later is not destroyed.
fn merge_json_file(
    path: &Path,
    mutate: impl FnOnce(&mut Map<String, Value>) -> MergeResult,
) -> io::Result<bool> {
    let dangling_link = path
        .symlink_metadata()
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
        && path.canonicalize().is_err();
    if dangling_link {
        tracing::warn!(file = %path.display(), "skipping config: symlink target does not exist");
        return Ok(false);
    }
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut root: Map<String, Value> = match std::fs::read_to_string(&real) {
        Ok(text) if text.trim().is_empty() => Map::new(),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => {
                tracing::warn!(file = %real.display(), "skipping config that is not a JSON object");
                return Ok(false);
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(error) => return Err(error),
    };
    match mutate(&mut root) {
        MergeResult::Unchanged => Ok(false),
        MergeResult::Skipped(reason) => {
            tracing::warn!(file = %real.display(), "skipping config: {reason}");
            Ok(false)
        }
        MergeResult::Applied => {
            let text =
                serde_json::to_string_pretty(&Value::Object(root)).map_err(io::Error::other)?;
            write_atomic(&real, &format!("{text}\n")).map(|_| true)
        }
    }
}

/// `mcpServers.computer` = the bridge server. Everything else in the file —
/// other servers, other settings — is preserved.
fn merge_mcp_json(path: &Path, entry: &Value) -> io::Result<bool> {
    merge_json_file(path, |root| {
        let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            return MergeResult::Skipped("`mcpServers` is not an object");
        };
        if servers.get(SERVER_NAME) == Some(entry) {
            return MergeResult::Unchanged;
        }
        servers.insert(SERVER_NAME.into(), entry.clone());
        MergeResult::Applied
    })
}

/// One agent file: `@computer/*` into `allowedTools`, `@computer` into a
/// restrictive `tools` list, and the server itself into `mcpServers` when the
/// agent opted out of the shared mcp.json. Other fields are untouched.
fn merge_agent_json(path: &Path, entry: &Value) -> io::Result<bool> {
    merge_json_file(path, |root| {
        let mut changed = false;
        match root.get_mut("allowedTools") {
            None => {
                root.insert("allowedTools".into(), json!([TRUST_PATTERN]));
                changed = true;
            }
            Some(Value::Array(list)) => {
                let trusted = list
                    .iter()
                    .any(|v| matches!(v.as_str(), Some(TRUST_PATTERN) | Some("@computer")));
                if !trusted {
                    list.push(json!(TRUST_PATTERN));
                    changed = true;
                }
            }
            Some(_) => return MergeResult::Skipped("`allowedTools` is not an array"),
        }
        if let Some(tools) = root.get_mut("tools") {
            match tools {
                Value::Array(list) => {
                    let covered = list
                        .iter()
                        .any(|v| v.as_str().is_some_and(|s| COVERING_TOOLS.contains(&s)));
                    if !covered {
                        list.push(json!(VISIBILITY_PATTERN));
                        changed = true;
                    }
                }
                _ => return MergeResult::Skipped("`tools` is not an array"),
            }
        }
        if root.get("includeMcpJson") == Some(&json!(false)) {
            let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
            let Some(servers) = servers.as_object_mut() else {
                return MergeResult::Skipped("`mcpServers` is not an object");
            };
            if servers.get(SERVER_NAME) != Some(entry) {
                servers.insert(SERVER_NAME.into(), entry.clone());
                changed = true;
            }
        }
        if changed {
            MergeResult::Applied
        } else {
            MergeResult::Unchanged
        }
    })
}

/// Write `text` to `path` via a sibling tempfile + rename, so a reader of the
/// shared config never sees a torn file. An existing file keeps its mode —
/// the tempfile's restrictive default must not tighten a shared dotfile.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    let Some(dir) = path.parent() else {
        return Err(io::Error::other(format!(
            "no parent dir: {}",
            path.display()
        )));
    };
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(tmp.path(), meta.permissions())?;
    }
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}
