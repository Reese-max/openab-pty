//! The spawn-time tools-MCP injection into the session CLI's config (#39).
//!
//! A lent Mac's tools must appear in the coding CLI with zero manual
//! `mcp add`, and the config a session writes must be *session-independent*:
//! the workspace `mcp.json` is shared by every session in the pod, so it can
//! never name one session's URL or key — the bridge reads
//! `OPENAB_TOOLS_MCP_URL` from the environment of whatever process launches it.
//!
//! Pure filesystem tests: each one builds its own tempdir HOME with a stub
//! `kiro-cli`/`bun`/`node` layout, so no test touches the real home directory
//! or needs the tools plane running.

use openab_pty::cli_config::{inject_tools_mcp, Outcome};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A kiro installer's layout under `home`: `kiro-cli` and its bundled `bun`.
fn kiro_install(home: &Path) -> PathBuf {
    let dir = home.join(".local/share/kiro-cli");
    fs::create_dir_all(&dir).unwrap();
    executable(&dir.join("kiro-cli"), "#!/bin/sh\n");
    executable(&dir.join("bun"), "#!/bin/sh\n");
    dir
}

/// A PATH that resolves nothing but the stubs deliberately placed in `bin`.
fn env_with_path(bin: &Path) -> Vec<(String, String)> {
    vec![("PATH".to_string(), bin.to_string_lossy().into_owned())]
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn mcp_path(home: &Path) -> PathBuf {
    home.join(".kiro/settings/mcp.json")
}

fn bridge_path(home: &Path) -> PathBuf {
    home.join(".local/share/openab-pty/computer-mcp-bridge.js")
}

#[test]
fn kiro_gets_a_session_independent_computer_server_and_trust() {
    let home = tempfile::tempdir().unwrap();
    let kiro = kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();

    let agents = home.path().join(".kiro/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("main.json"),
        r#"{"name":"main","description":"the agent","tools":["read","write"],
            "allowedTools":["read"],"resources":["file://AGENTS.md"]}"#,
    )
    .unwrap();
    fs::write(agents.join("notes.md"), "---\nname: notes\n---\n").unwrap();

    let outcome = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    let Outcome::Injected { variant, files } = outcome else {
        panic!("expected Injected, got {outcome:?}");
    };
    assert_eq!(variant, "kiro");
    assert!(files.contains(&mcp_path(home.path())));
    assert!(files.contains(&bridge_path(home.path())));
    assert!(files.contains(&agents.join("main.json")));

    // The server entry is the verified stdio shape: the bundled runtime on a
    // bridge that reads the URL from *its* environment. No URL, no key, so the
    // one shared file cannot pin a session — or go stale on rotation.
    let mcp = read_json(&mcp_path(home.path()));
    let computer = &mcp["mcpServers"]["computer"];
    assert_eq!(
        computer["command"],
        json!(kiro.join("bun").to_string_lossy())
    );
    assert_eq!(
        computer["args"],
        json!([bridge_path(home.path()).to_string_lossy()])
    );
    assert_eq!(
        computer["env"]["OPENAB_TOOLS_MCP_URL"],
        json!("${OPENAB_TOOLS_MCP_URL}")
    );
    let raw = fs::read_to_string(mcp_path(home.path())).unwrap();
    assert!(!raw.contains("http"), "no endpoint literal: {raw}");
    assert!(!raw.contains("/mcp/"), "no URL path literal: {raw}");

    // The bridge itself is the only place the variable is *read*; the config
    // only forwards it. And it holds no key either.
    let bridge = fs::read_to_string(bridge_path(home.path())).unwrap();
    assert!(bridge.contains("OPENAB_TOOLS_MCP_URL"));

    // Trust: the served set, under the platform-neutral alias.
    let agent = read_json(&agents.join("main.json"));
    assert_eq!(
        agent["allowedTools"],
        json!(["read", "@computer/*"]),
        "trust is additive, after what the file already had"
    );
    assert_eq!(
        agent["tools"],
        json!(["read", "write", "@computer"]),
        "a restrictive tools list must still see the server"
    );
    assert_eq!(agent["name"], json!("main"));
    assert_eq!(agent["description"], json!("the agent"));
    assert_eq!(agent["resources"], json!(["file://AGENTS.md"]));

    // Markdown agents are not JSON-mergeable; they are left alone.
    assert_eq!(
        fs::read_to_string(agents.join("notes.md")).unwrap(),
        "---\nname: notes\n---\n"
    );
}

#[test]
fn injection_merges_without_touching_unrelated_config() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();
    fs::create_dir_all(mcp_path(home.path()).parent().unwrap()).unwrap();
    fs::write(
        mcp_path(home.path()),
        r#"{
          "mcpServers": {"github": {"command": "gh-mcp"}},
          "otherSetting": true
        }"#,
    )
    .unwrap();

    let outcome = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    assert!(matches!(outcome, Outcome::Injected { .. }));

    let mcp = read_json(&mcp_path(home.path()));
    assert_eq!(
        mcp["mcpServers"]["github"],
        json!({"command": "gh-mcp"}),
        "an existing server survives the merge"
    );
    assert_eq!(mcp["otherSetting"], json!(true));
    assert!(mcp["mcpServers"]["computer"].is_object());
}

#[test]
fn a_cli_the_runtime_does_not_know_gets_the_manual_path() {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap(); // nothing on PATH either

    let outcome = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    assert_eq!(outcome, Outcome::NoKnownVariant);
    assert!(
        !home.path().join(".kiro").exists(),
        "nothing is written for a CLI whose config shape we do not know"
    );
}

#[test]
fn no_js_runtime_means_no_broken_server_entry() {
    let home = tempfile::tempdir().unwrap();
    let kiro = kiro_install(home.path());
    fs::remove_file(kiro.join("bun")).unwrap();
    let bin = tempfile::tempdir().unwrap(); // no bun, no node

    let outcome = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    let Outcome::NoJsRuntime { variant } = outcome else {
        panic!("expected NoJsRuntime, got {outcome:?}");
    };
    assert_eq!(variant, "kiro");
    assert!(
        !mcp_path(home.path()).exists(),
        "a computer server nothing can execute is worse than none"
    );
}

#[test]
fn node_is_the_fallback_runtime_when_no_bun_exists() {
    let home = tempfile::tempdir().unwrap();
    let kiro = kiro_install(home.path());
    fs::remove_file(kiro.join("bun")).unwrap();
    let bin = tempfile::tempdir().unwrap();
    executable(&bin.path().join("node"), "#!/bin/sh\necho v18.19.0\n");

    let outcome = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    assert!(matches!(outcome, Outcome::Injected { .. }));
    let mcp = read_json(&mcp_path(home.path()));
    assert_eq!(
        mcp["mcpServers"]["computer"]["command"],
        json!(bin.path().join("node").to_string_lossy())
    );
}

#[test]
fn a_node_too_old_for_fetch_falls_back_to_the_manual_path() {
    // node < 18 has no fetch; pointing `computer` at it injects a server that
    // only ever reports errors — better to write nothing at all.
    let home = tempfile::tempdir().unwrap();
    let kiro = kiro_install(home.path());
    fs::remove_file(kiro.join("bun")).unwrap();
    let bin = tempfile::tempdir().unwrap();
    executable(&bin.path().join("node"), "#!/bin/sh\necho v16.20.0\n");

    let outcome = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    assert_eq!(outcome, Outcome::NoJsRuntime { variant: "kiro" });
    assert!(!mcp_path(home.path()).exists());
}

#[test]
fn malformed_or_foreign_shaped_config_is_never_clobbered() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();

    let mangled = "{ not json";
    fs::create_dir_all(mcp_path(home.path()).parent().unwrap()).unwrap();
    fs::write(mcp_path(home.path()), mangled).unwrap();

    let agents = home.path().join(".kiro/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("odd.json"),
        r#"{"name":"odd","allowedTools":"read"}"#,
    )
    .unwrap();

    inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();

    assert_eq!(
        fs::read_to_string(mcp_path(home.path())).unwrap(),
        mangled,
        "a file that is not ours to fix is left byte-identical"
    );
    assert_eq!(
        fs::read_to_string(agents.join("odd.json")).unwrap(),
        r#"{"name":"odd","allowedTools":"read"}"#,
        "a non-array allowedTools is skipped, not rewritten"
    );
}

#[test]
#[cfg(unix)]
fn a_dangling_symlinked_config_is_left_alone() {
    // Dotfile managers often link a config whose target exists only later;
    // replacing the link with a real file would break that layout.
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let settings = home.path().join(".kiro/settings");
    std::fs::create_dir_all(&settings).unwrap();
    let mcp = settings.join("mcp.json");
    std::os::unix::fs::symlink(settings.join("dotfiles-mcp.json"), &mcp).unwrap();
    let outcome = inject_tools_mcp(home.path(), &env_with_path(home.path())).unwrap();
    assert!(matches!(outcome, Outcome::Injected { .. }));
    assert!(mcp.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(mcp.canonicalize().is_err(), "the link stays dangling");
    assert!(!settings.join("dotfiles-mcp.json").exists());
}

#[test]
#[cfg(unix)]
fn a_merged_config_keeps_its_existing_permissions() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let settings = home.path().join(".kiro/settings");
    std::fs::create_dir_all(&settings).unwrap();
    let mcp = settings.join("mcp.json");
    std::fs::write(&mcp, "{}\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&mcp, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    inject_tools_mcp(home.path(), &env_with_path(home.path())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&mcp).unwrap().permissions().mode() & 0o777,
            0o644,
            "the merge must not tighten a shared dotfile's mode"
        );
    }
}

#[test]
fn a_broken_symlink_target_or_missing_agents_dir_is_not_invented() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();

    inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    assert!(
        !home.path().join(".kiro/agents").exists(),
        "no agent files exist yet: creating one could shadow a built-in agent, \
         so trust stays the documented manual step"
    );
    assert!(mcp_path(home.path()).exists());
}

#[test]
fn mcp_json_symlink_is_written_through_not_replaced() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();

    let real = home.path().join("shared-mcp.json");
    fs::write(&real, r#"{"mcpServers":{}}"#).unwrap();
    fs::create_dir_all(mcp_path(home.path()).parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, mcp_path(home.path())).unwrap();

    inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();

    assert!(
        mcp_path(home.path())
            .symlink_metadata()
            .unwrap()
            .is_symlink(),
        "the link itself must survive; the merge writes its target"
    );
    assert!(read_json(&real)["mcpServers"]["computer"].is_object());
}

#[test]
fn an_agent_opted_out_of_mcp_json_still_gets_the_server() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();
    let agents = home.path().join(".kiro/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("solo.json"),
        r#"{"name":"solo","includeMcpJson":false,"mcpServers":{"own":{"command":"x"}}}"#,
    )
    .unwrap();

    inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();

    let agent = read_json(&agents.join("solo.json"));
    assert_eq!(agent["includeMcpJson"], json!(false), "the opt-out stands");
    assert_eq!(agent["mcpServers"]["own"], json!({"command":"x"}));
    assert!(
        agent["mcpServers"]["computer"].is_object(),
        "an agent that excludes mcp.json needs its own copy of the server"
    );
    assert_eq!(agent["allowedTools"], json!(["@computer/*"]));
}

// --------------------------------------------------------------------------
// The bridge itself. `#[ignore]`d like the other e2e tests: it spawns a real
// JS runtime and binds a socket — it is the only coverage of the wire shape
// the kiro side actually sees.
// --------------------------------------------------------------------------

fn js_runtime() -> Option<PathBuf> {
    for name in ["node", "bun"] {
        if let Ok(output) = std::process::Command::new("which").arg(name).output() {
            if output.status.success() {
                return Some(String::from_utf8(output.stdout).unwrap().trim().into());
            }
        }
    }
    None
}

/// A stub Streamable-HTTP endpoint: one JSON answer for requests carrying an
/// `id`, an SSE answer with a *multi-line* `data:` field for the SSE probe,
/// and a bare 202 for notifications — the three shapes the bridge must get
/// right.
async fn stub_mcp(body: axum::body::Bytes) -> axum::response::Response {
    use axum::response::IntoResponse;
    let request: Value = serde_json::from_slice(&body).unwrap();
    if request.get("sse_probe").is_some() {
        return (
            [("content-type", "text/event-stream")],
            "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":7,\ndata: \"result\":{\"sse\":true}}\n\n",
        )
            .into_response();
    }
    match request.get("id") {
        Some(id) if !id.is_null() => axum::Json(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "result": {"echo": request.get("method")}
        }))
        .into_response(),
        _ => axum::http::StatusCode::ACCEPTED.into_response(),
    }
}

#[tokio::test]
#[ignore = "spawns a JS runtime and binds a socket"]
async fn the_bridge_relays_one_jsonrpc_message_per_line() {
    let Some(runtime) = js_runtime() else {
        eprintln!("no node/bun on PATH; skipping");
        return;
    };
    let bridge = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/computer-mcp-bridge.js");

    let app = axum::Router::new().route("/", axum::routing::post(stub_mcp));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut child = tokio::process::Command::new(runtime)
        .arg(&bridge)
        .env("OPENAB_TOOLS_MCP_URL", format!("http://127.0.0.1:{port}/"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
        .await
        .unwrap();
    let line = tokio::time::timeout(std::time::Duration::from_secs(10), stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(response["result"]["echo"], serde_json::json!("initialize"));

    // An SSE reply whose `data:` spans multiple lines must still come back as
    // exactly one stdout line — the bug the compact re-encode guards.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"x\",\"sse_probe\":true}\n")
        .await
        .unwrap();
    let line = tokio::time::timeout(std::time::Duration::from_secs(10), stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["result"]["sse"], serde_json::json!(true));
    assert!(
        !line.contains('\n'),
        "a multi-line data payload would emit more than one line"
    );

    // A notification gets the 202's whole acknowledgement: nothing written.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let quiet =
        tokio::time::timeout(std::time::Duration::from_millis(500), stdout.next_line()).await;
    assert!(quiet.is_err(), "a notification must not produce output");

    child.kill().await.unwrap();
}

#[test]
fn repeated_spawns_rewrite_the_server_but_change_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    kiro_install(home.path());
    let bin = tempfile::tempdir().unwrap();
    let agents = home.path().join(".kiro/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("main.json"),
        r#"{"name":"main","allowedTools":["@computer/*"],"tools":["*"]}"#,
    )
    .unwrap();

    let first = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    let Outcome::Injected {
        files: first_files, ..
    } = first
    else {
        panic!("expected Injected");
    };
    let second = inject_tools_mcp(home.path(), &env_with_path(bin.path())).unwrap();
    let Outcome::Injected {
        files: second_files,
        ..
    } = second
    else {
        panic!("expected Injected");
    };

    let agent = read_json(&agents.join("main.json"));
    assert_eq!(
        agent["allowedTools"],
        json!(["@computer/*"]),
        "no duplicate trust entry on the second spawn"
    );
    // tools already covers everything via "*"; nothing was appended.
    assert_eq!(agent["tools"], json!(["*"]));
    // The bridge is always (re)written — it is runtime-owned — but the agent
    // file is only rewritten when a merge was actually needed.
    assert!(second_files.contains(&bridge_path(home.path())));
    assert!(
        !second_files.contains(&agents.join("main.json")),
        "an unchanged agent file is not rewritten on every spawn"
    );
    assert!(
        first_files.contains(&agents.join("main.json")) || {
            // unless the first spawn already found it complete
            agent["allowedTools"] == json!(["@computer/*"])
        }
    );
}
