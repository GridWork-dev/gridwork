//! The built binary, run as a caller runs it.
//!
//! These drive `gw` as a subprocess rather than calling `run` in-process, and
//! that is the point: the socket path comes from the environment, and a
//! subprocess can be given its own without mutating a variable every other test
//! in the binary shares. It also means the exit code being asserted is the one
//! the shell would see.
//!
//! Most cases need nothing but the binary. The last one needs a database,
//! because the only way to prove the client half of the wire is to put a real
//! daemon on the other end of it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, ClientControl,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelRequest, KernelResult, ProtocolVersion, ServerControl,
};
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
use tokio::net::UnixListener;

/// A directory the daemon will accept as a socket's parent.
fn private_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("gw-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

/// Run `gw` with a socket path of the caller's choosing.
fn gw(socket: &Path, line: &str) -> Output {
    gw_env(line, &[("GWK_SOCKET_PATH", &socket.to_string_lossy())])
}

/// Run `gw` with exactly the environment given — nothing inherited that matters.
///
/// A subprocess is what makes this safe: the credentials and the socket path a
/// case needs go to the child, rather than into a variable every other case in
/// this binary would see.
fn gw_env(line: &str, env: &[(&str, &str)]) -> Output {
    let args: Vec<&str> = line.split_whitespace().collect();
    gw_args(&args, env)
}

/// The same, with the argv given piece by piece — for the cases whose whole
/// point is an argument that contains a space.
fn gw_args(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gw"));
    command.args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("run gw")
}

#[cfg(target_os = "linux")]
fn gw_tty(socket: &Path, args: &[&str]) -> Output {
    // script's `-- <command> <args>` form needs util-linux 2.41+, and even
    // there the argv is re-joined through `sh -c`; `-c` before the typescript
    // file is the one spelling every version since 2.39 parses identically.
    // The args here are fixed test literals — only the binary path is quoted.
    let command = format!("'{}' {}", env!("CARGO_BIN_EXE_gw"), args.join(" "));
    Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .env("GWK_SOCKET_PATH", socket)
        .output()
        .expect("run gw under a pseudo-terminal")
}

async fn serve_empty_projection_pages(listener: UnixListener, connections: usize) {
    for _ in 0..connections {
        let (mut stream, _) = listener.accept().await.expect("accept CLI client");
        let mut budget = Budget::new(
            CONNECTION_INGRESS_BYTES_PER_WINDOW,
            CONNECTION_EGRESS_BYTES_PER_WINDOW,
        );
        let hello = read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut budget)
            .await
            .expect("read hello");
        let Incoming::Frame(hello) = hello else {
            panic!("CLI closed before hello");
        };
        assert_eq!(hello.kind, FrameKind::Json);
        assert!(matches!(
            serde_json::from_slice::<ClientControl>(&hello.body).expect("decode hello"),
            ClientControl::Hello { .. }
        ));
        let ack = ServerControl::HelloAck {
            protocol_major: ProtocolVersion::V1,
            protocol_minor: 0,
            capabilities: Vec::new(),
            sealed: true,
            watermark: Some(gwk_domain::ids::Seq::new(221)),
        };
        write_frame(
            &mut stream,
            FrameKind::Json,
            &serde_json::to_vec(&ack).expect("encode hello ack"),
            &mut budget,
        )
        .await
        .expect("write hello ack");

        loop {
            let frame = read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut budget)
                .await
                .expect("read projection request");
            let Incoming::Frame(frame) = frame else {
                break;
            };
            let ClientControl::Request {
                request_id,
                request,
            } = serde_json::from_slice(&frame.body).expect("decode projection request")
            else {
                panic!("unexpected control after hello");
            };
            assert!(matches!(request, KernelRequest::ListProjection { .. }));
            let answer = ServerControl::Response {
                request_id,
                result: KernelResult::ProjectionPage {
                    records: Vec::new(),
                    next_cursor: None,
                    watermark: Some(gwk_domain::ids::Seq::new(221)),
                    // Absent on purpose: this fake stands in for a kernel that
                    // predates the field, which is the fallback path.
                    served_at: None,
                },
            };
            write_frame(
                &mut stream,
                FrameKind::Json,
                &serde_json::to_vec(&answer).expect("encode projection answer"),
                &mut budget,
            )
            .await
            .expect("write projection answer");
        }
    }
}

/// Answer one hello with an ack at the major given, then hang up.
///
/// A daemon that acknowledges at a major the client did not ask for. Until a
/// second major was named this shape was unreachable: `ProtocolVersion` refused
/// anything but 1 at `Deserialize`, so such an ack never decoded and the client
/// errored without ever having to look. Naming `V2` made it decode, and the
/// client has to refuse it explicitly or speak v1 over a connection the peer
/// declared v2 — the downgrade threat 9 exists to prevent.
async fn serve_one_ack_at(listener: UnixListener, major: ProtocolVersion) {
    let (mut stream, _) = listener.accept().await.expect("accept CLI client");
    let mut budget = Budget::new(
        CONNECTION_INGRESS_BYTES_PER_WINDOW,
        CONNECTION_EGRESS_BYTES_PER_WINDOW,
    );
    let hello = read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut budget)
        .await
        .expect("read hello");
    let Incoming::Frame(hello) = hello else {
        panic!("CLI closed before hello");
    };
    assert!(matches!(
        serde_json::from_slice::<ClientControl>(&hello.body).expect("decode hello"),
        ClientControl::Hello { .. }
    ));
    let ack = ServerControl::HelloAck {
        protocol_major: major,
        protocol_minor: 0,
        capabilities: Vec::new(),
        sealed: true,
        watermark: Some(gwk_domain::ids::Seq::new(1)),
    };
    write_frame(
        &mut stream,
        FrameKind::Json,
        &serde_json::to_vec(&ack).expect("encode hello ack"),
        &mut budget,
    )
    .await
    .expect("write hello ack");
}

/// Run `gw attempt list` against a fake daemon that acks at `major`.
async fn stderr_of_ack_at(tag: &str, major: ProtocolVersion) -> String {
    let dir = private_dir(tag);
    let socket = dir.join("k.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake kernel");
    let server = tokio::spawn(serve_one_ack_at(listener, major));
    let output = tokio::task::spawn_blocking({
        let socket = socket.clone();
        move || gw(&socket, "attempt list")
    })
    .await
    .expect("join gw");
    // Bounded, like every other fake-kernel case here: if the client never
    // connects, `accept` waits forever and the failure reads as a hang rather
    // than as a test that could not run.
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("fake kernel timed out")
        .expect("join fake kernel");
    assert_ne!(code(&output), 0, "gw succeeded against a one-ack daemon");
    // Both streams: `gw` reports a refusal on whichever the surface chose, and
    // a test that read only one would call an unprinted message a missing check.
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test]
async fn a_daemon_that_acknowledges_at_another_major_is_refused_not_followed() {
    let refused = stderr_of_ack_at("ack-major-v2", ProtocolVersion::V2).await;
    assert!(
        refused.contains("acknowledged at protocol major"),
        "a v2 ack was not refused as a version mismatch: {refused}"
    );
    assert!(
        refused.contains('2'),
        "the refusal must name the major it refused: {refused}"
    );

    // The positive control, on the same harness. Both runs fail — the fake
    // daemon hangs up after one ack — so asserting only that the v2 run failed
    // would pass against a client that refuses every handshake. What separates
    // them is WHY: the v1 run must fail somewhere later than the version check.
    let accepted = stderr_of_ack_at("ack-major-v1", ProtocolVersion::V1).await;
    assert!(
        !accepted.contains("acknowledged at protocol major"),
        "a v1 ack was refused as a version mismatch: {accepted}"
    );
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("gw exited on a signal")
}

fn json(output: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {text}"))
}

#[test]
fn the_command_tree_is_printed_as_prose_and_everything_else_as_json() {
    let dir = private_dir("help");
    let socket = dir.join("absent.sock");

    let help = gw(&socket, "--help");
    assert_eq!(code(&help), 0);
    let text = String::from_utf8_lossy(&help.stdout);
    // The one output a human reads. Every other answer is machine JSON, which is
    // why this case exists: to pin the exception rather than let it spread.
    assert!(text.starts_with("gw —"), "{text}");
    assert!(text.contains("gw kernel health"), "{text}");
    assert!(text.contains("gw estate overview"), "{text}");
    assert!(text.contains("gw activity brief"), "{text}");
    assert!(text.contains("gw cost rollup"), "{text}");
    assert!(text.contains("gw event tail"), "{text}");
    assert!(text.contains("gw agent fleet"), "{text}");
    assert!(text.contains("gw attempt stop"), "{text}");
    assert!(text.contains("gw attempt budget"), "{text}");
    assert!(text.contains("gw session list"), "{text}");
    assert!(text.contains("gw session inspect"), "{text}");
    assert!(text.contains("gw term list"), "{text}");
    assert!(text.contains("gw term tail"), "{text}");
    assert!(text.contains("gw term attach"), "{text}");

    let info = gw(&socket, "build-info");
    assert_eq!(code(&info), 0);
    let info = json(&info);
    assert_eq!(info["type"], "build_info");
    assert!(info["contract_version"].is_number(), "{info}");
    // Present either way: a build stamped from a clean checkout reports its
    // revision and an unstamped one reports null. What must not happen is the
    // key being absent, because a caller comparing this against the revision
    // genesis recorded would read "absent" as "same".
    assert!(info.get("public_revision").is_some(), "{info}");
    if let Some(revision) = info["public_revision"].as_str() {
        assert_eq!(revision.len(), 40, "{revision}");
    }
}

#[test]
fn the_chrome_theme_resolves_without_a_daemon_and_refuses_an_invisible_remap() {
    let dir = private_dir("theme");
    let socket = dir.join("absent.sock");

    // No socket, no daemon, no database: the slot is a local file against a
    // ratified palette, so its twin answers with the daemon down.
    let bare = gw(&socket, "theme");
    assert_eq!(
        code(&bare),
        0,
        "{:?}",
        String::from_utf8_lossy(&bare.stdout)
    );
    let answer = json(&bare);
    assert_eq!(answer["type"], "chrome_theme");
    assert_eq!(answer["signal"], true, "an unset variable is the default");
    let roles = answer["roles"].as_array().expect("roles");
    assert_eq!(roles.len(), 7, "{answer}");
    assert_eq!(roles[0]["role"], "pane_border");
    assert_eq!(roles[0]["token"], roles[0]["default"]);

    // The twin reports exactly what the workspace would paint, remap and all.
    let file = dir.join("chrome.toml");
    std::fs::write(&file, "tab_active = \"gws_ok\"\n").expect("write theme");
    let themed = gw_env("theme", &[("GWK_CHROME_THEME", &file.to_string_lossy())]);
    assert_eq!(code(&themed), 0);
    let answer = json(&themed);
    assert_eq!(answer["signal"], false, "a remap is not the default");
    let active = answer["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .find(|role| role["role"] == "tab_active")
        .expect("tab_active");
    assert_eq!(active["token"], "gws_ok");
    assert_eq!(active["default"], "gws_hue_bright");

    // An elevation step paints nothing at any tier, so a role pointed at one
    // would be invisible rather than differently coloured. Refused, with the
    // reason, rather than accepted into a workspace that looks broken.
    let bad = dir.join("bad.toml");
    std::fs::write(&bad, "pane_border = \"gws_bg\"\n").expect("write theme");
    let refused = gw_env("theme", &[("GWK_CHROME_THEME", &bad.to_string_lossy())]);
    assert_eq!(
        code(&refused),
        6,
        "{:?}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let answer = json(&refused);
    assert_eq!(answer["type"], "error");
    let message = answer["message"].as_str().expect("message");
    assert!(message.contains("elevation step"), "{message}");

    // A named path that is not there is an error, never a silent revert to
    // Signal — those two look identical to an operator otherwise.
    let absent = gw_env(
        "theme",
        &[("GWK_CHROME_THEME", &dir.join("gone.toml").to_string_lossy())],
    );
    assert_ne!(code(&absent), 0);
    assert!(
        json(&absent)["message"]
            .as_str()
            .expect("message")
            .contains("could not be read"),
        "{:?}",
        String::from_utf8_lossy(&absent.stdout)
    );
}

#[test]
fn a_mistake_exits_two_and_says_what_it_was() {
    let dir = private_dir("usage");
    let socket = dir.join("absent.sock");

    for line in ["nonsense", "projection list tsak", "kernel activate"] {
        let out = gw(&socket, line);
        assert_eq!(
            code(&out),
            2,
            "{line}: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        let answer = json(&out);
        // The protocol's own error shape, so a caller parses one thing whether
        // the refusal came from the kernel or from the command line.
        assert_eq!(answer["type"], "error");
        assert_eq!(answer["code"], "validation");
    }

    // The projection refusal lists what would have worked. A closed set that
    // refuses without naming its members makes the caller go and read source.
    let answer = json(&gw(&socket, "projection list tsak"));
    let message = answer["message"].as_str().expect("message");
    assert!(message.contains("attention_item"), "{message}");
}

#[test]
fn a_daemon_that_is_not_there_is_unavailable_rather_than_a_crash() {
    let dir = private_dir("absent");
    let socket = dir.join("absent.sock");

    let out = gw(&socket, "kernel health");
    // 5, not 10: nothing is wrong with `gw`, and nothing is wrong with the
    // request. The kernel is not running, and trying again later is the fix.
    assert_eq!(code(&out), 5);
    let answer = json(&out);
    assert_eq!(answer["code"], "storage");
    assert!(
        answer["message"]
            .as_str()
            .expect("message")
            .contains("absent.sock"),
        "{answer}"
    );

    for line in [
        "cost rollup",
        "attempt list",
        "attempt stop at-1",
        "attempt budget at-1",
        "attempt budget clear at-1 --expected-version 1",
        "session list",
        "term list",
        "term tail pty-1",
        "term attach pty-1",
    ] {
        let out = gw(&socket, line);
        assert_eq!(
            code(&out),
            5,
            "{line}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert_eq!(json(&out)["code"], "storage", "{line}");
    }
}

#[test]
fn pr_dry_run_prints_the_gh_argv_it_would_run() {
    let out = gw_args(
        &[
            "pr",
            "open",
            "--dry-run",
            "--title",
            "two words",
            "--body-file",
            "-",
            "--repo",
            "o/r",
            "--head",
            "b",
        ],
        &[],
    );
    assert_eq!(code(&out), 0);
    let answer = json(&out);
    assert_eq!(answer["type"], "gh_argv");
    // The two-word title is ONE element. This is the arg-array claim made
    // inspectable: no shell line exists anywhere for it to have been split by.
    assert_eq!(
        answer["argv"],
        serde_json::json!([
            "pr",
            "create",
            "--title",
            "two words",
            "--body-file",
            "-",
            "--head",
            "b",
            "--repo",
            "o/r"
        ])
    );
}

#[test]
fn pr_reaches_gh_as_an_argument_array_and_relays_its_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir("gh");
    let record = dir.join("argv");

    // A `gh` that records the argv it received, one element per line — so an
    // argument that was split or joined on the way through shows up as the
    // wrong number of lines.
    let fake = dir.join("gh");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$GW_TEST_GH_ARGV\"\nexit 0\n",
    )
    .expect("write fake gh");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let path = dir.to_string_lossy().into_owned();
    let record_path = record.to_string_lossy().into_owned();
    let env = [
        ("PATH", path.as_str()),
        ("GW_TEST_GH_ARGV", record_path.as_str()),
    ];
    // Arguments WITH spaces, through the live path — a regression that joined
    // the argv into a shell line and re-split it would change the line count
    // the recorder writes, so this case can actually catch the bug class the
    // arg-array rule forbids.
    let out = gw_args(
        &["pr", "open", "--title", "two words", "--body", "b b"],
        &env,
    );
    assert_eq!(code(&out), 0, "{:?}", String::from_utf8_lossy(&out.stdout));
    // The live answer is gh's own conversation; gw adds nothing on top.
    assert!(
        out.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let received = std::fs::read_to_string(&record).expect("the fake gh ran");
    let received: Vec<&str> = received.lines().collect();
    assert_eq!(
        received,
        ["pr", "create", "--title", "two words", "--body", "b b"]
    );

    // And when gh says no, gw relays the fact in its own error shape and the
    // one exit table: the reason was gh's to print, the machine-readable
    // fact of the refusal is ours.
    std::fs::write(&fake, "#!/bin/sh\nexit 7\n").expect("rewrite fake gh");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let out = gw_args(&["pr", "merge", "61"], &env);
    assert_eq!(code(&out), 5);
    let answer = json(&out);
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["code"], "storage");
    assert!(
        answer["message"]
            .as_str()
            .expect("message")
            .contains("gh exited 7"),
        "{answer}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn list_output_is_a_table_on_a_tty_and_wire_json_when_piped_or_forced() {
    let dir = private_dir("tty-table");

    let tty_socket = dir.join("tty.sock");
    let listener = UnixListener::bind(&tty_socket).expect("bind tty fake kernel");
    let server = tokio::spawn(serve_empty_projection_pages(listener, 1));
    let tty = tokio::task::spawn_blocking({
        let socket = tty_socket.clone();
        move || gw_tty(&socket, &["attempt", "list"])
    })
    .await
    .expect("join tty gw");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("tty fake kernel timed out")
        .expect("join tty fake kernel");
    assert_eq!(code(&tty), 0, "{}", String::from_utf8_lossy(&tty.stdout));
    let tty_text = String::from_utf8_lossy(&tty.stdout).replace('\r', "");
    assert!(tty_text.contains("ATTEMPT"), "{tty_text}");
    assert!(tty_text.contains("0 rows · watermark 221"), "{tty_text}");
    assert!(!tty_text.contains("\"records\""), "{tty_text}");

    let forced_socket = dir.join("forced.sock");
    let listener = UnixListener::bind(&forced_socket).expect("bind forced fake kernel");
    let server = tokio::spawn(serve_empty_projection_pages(listener, 1));
    let forced = tokio::task::spawn_blocking({
        let socket = forced_socket.clone();
        move || gw_tty(&socket, &["attempt", "list", "--json"])
    })
    .await
    .expect("join forced gw");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("forced fake kernel timed out")
        .expect("join forced fake kernel");
    assert_eq!(code(&forced), 0);
    let forced_text = String::from_utf8_lossy(&forced.stdout).replace('\r', "");
    let forced_json: serde_json::Value =
        serde_json::from_str(forced_text.trim()).expect("forced tty output is JSON");
    assert_eq!(forced_json["type"], "projection_page");
    assert_eq!(forced_json["records"], serde_json::json!([]));

    let pipe_socket = dir.join("pipe.sock");
    let listener = UnixListener::bind(&pipe_socket).expect("bind pipe fake kernel");
    let server = tokio::spawn(serve_empty_projection_pages(listener, 1));
    let piped = tokio::task::spawn_blocking({
        let socket = pipe_socket.clone();
        move || gw(&socket, "attempt list")
    })
    .await
    .expect("join piped gw");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("pipe fake kernel timed out")
        .expect("join pipe fake kernel");
    assert_eq!(code(&piped), 0);
    assert_eq!(json(&piped)["type"], "projection_page");
}

/// The runtime role `admin::init` grants to. Created by the maintenance pool.
const RUNTIME_ROLE: &str = "gwk_cli_runtime";
const TEST_KEK: [u8; 32] = [0x5a; 32];

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_binary_talks_to_a_real_daemon() {
    use gwk_kernel::blob::store::PgBlobStore;
    use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, BlobConfig, RUNTIME_ROLE_ENV};
    use gwk_kernel::wire::listen::Listener;
    use gwk_kernel::wire::serve::Daemon;
    use gwk_kernel::{PgEventStore, admin, connect_pool};
    use secrecy::SecretString;
    use sqlx::PgPool;

    let admin_url = std::env::var("GWK_TEST_ADMIN_DATABASE_URL")
        .expect("GWK_TEST_ADMIN_DATABASE_URL must point at a PostgreSQL superuser DSN");
    let maintenance = PgPool::connect(&admin_url).await.expect("maintenance");
    // No CREATE ROLE IF NOT EXISTS; the failure case is "already exists", which
    // is the state this wanted. A role genuinely absent fails at the GRANT.
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {RUNTIME_ROLE} NOLOGIN;"
    )))
    .execute(&maintenance)
    .await;

    let name = format!("gwk_cli_{}", std::process::id());
    let url = {
        let (prefix, _) = admin_url.rsplit_once('/').expect("a /database suffix");
        format!("{prefix}/{name}")
    };
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE);"
    )))
    .execute(&maintenance)
    .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name};")))
        .execute(&maintenance)
        .await
        .expect("create database");

    let pool = connect_pool(&SecretString::from(url.clone()), 8)
        .await
        .expect("connect");
    let config = AdminConfig::from_lookup(move |key| match key {
        ADMIN_DATABASE_URL_ENV => Some(url.clone()),
        RUNTIME_ROLE_ENV => Some(RUNTIME_ROLE.to_owned()),
        _ => None,
    })
    .expect("admin config");
    admin::init(&pool, &config).await.expect("init");

    let dir = private_dir("live");
    let blob_root = dir.join("blobs");
    let blobs = PgBlobStore::open(
        pool.clone(),
        BlobConfig::new(blob_root.clone(), TEST_KEK, "kek-test".to_owned()).expect("blob config"),
    )
    .await
    .expect("blob store");
    let store = PgEventStore::open(pool).await.expect("store");
    store
        .ensure_genesis(&"a1b2c3d4e5".repeat(4))
        .await
        .expect("genesis");

    let socket = dir.join("gwk.sock");
    let listener = Listener::bind(&socket).await.expect("bind");
    let daemon = std::sync::Arc::new(
        Daemon::new(store.with_blobs(blobs), "a1b2c3d4e5".repeat(4)).expect("daemon"),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        let _ = gwk_kernel::wire::serve::run(listener, daemon, async move {
            let _ = stopped.await;
        })
        .await;
    });

    // Run the binary on a blocking thread: it is a subprocess doing synchronous
    // I/O against a daemon living in this runtime, so waiting for it here would
    // park the very task that has to answer it.
    let socket_for_blocking = socket.clone();
    let answers = tokio::task::spawn_blocking(move || {
        let socket = socket_for_blocking.as_path();
        let health = gw(socket, "kernel health");
        let status = gw(socket, "kernel status");
        let events = gw(socket, "event read --limit 1");
        let tasks = gw(socket, "projection list task");
        let estate = gw(socket, "estate overview");
        let activity = gw(socket, "activity brief");
        let cost_rollup = gw(socket, "cost rollup");
        let agent_fleet = gw(socket, "agent fleet");
        let attempt_budget_missing = gw(socket, "attempt budget at-nope");
        let sessions = gw(socket, "session list");
        let session_missing = gw(socket, "session inspect es-nope");
        let pty_missing = gw(socket, "term tail pty-nope");
        let pty_attach_missing = gw(socket, "term attach pty-nope");
        let missing = gw(socket, "projection get task t-nope");

        // A blob, all the way there and back through the built binary.
        let payload = socket.with_file_name("payload.bin");
        let mut file = std::fs::File::create(&payload).expect("create");
        let plaintext: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();
        file.write_all(&plaintext).expect("write");
        drop(file);
        let put = gw(
            socket,
            &format!(
                "blob put --file {} --media-type text/plain",
                payload.display()
            ),
        );
        (
            health,
            status,
            events,
            tasks,
            estate,
            activity,
            missing,
            put,
            plaintext,
            payload,
            (
                agent_fleet,
                sessions,
                session_missing,
                pty_missing,
                pty_attach_missing,
                cost_rollup,
                attempt_budget_missing,
            ),
        )
    })
    .await
    .expect("join");
    let (
        health,
        status,
        events,
        tasks,
        estate,
        activity,
        missing,
        put,
        plaintext,
        payload,
        (
            agent_fleet,
            sessions,
            session_missing,
            pty_missing,
            pty_attach_missing,
            cost_rollup,
            attempt_budget_missing,
        ),
    ) = answers;

    assert_eq!(
        code(&health),
        0,
        "{:?}",
        String::from_utf8_lossy(&health.stderr)
    );
    // The contract's own value, not a shape this program invented: a sealed
    // kernel is READY and says so.
    assert_eq!(
        json(&health),
        serde_json::json!({"type": "health", "ready": true, "sealed": true})
    );

    assert_eq!(code(&status), 0);
    let status = json(&status);
    assert_eq!(status["type"], "status");
    assert_eq!(status["public_revision"], "a1b2c3d4e5".repeat(4));

    assert_eq!(code(&events), 0);
    let events = json(&events);
    // Genesis, and nothing else — the log of a kernel that has only been
    // initialized.
    assert_eq!(
        events["events"].as_array().expect("events").len(),
        1,
        "{events}"
    );

    assert_eq!(code(&tasks), 0);
    // An empty page is an answer, and a cursor that says the walk is over.
    assert_eq!(json(&tasks)["records"], serde_json::json!([]));

    assert_eq!(code(&estate), 0);
    let estate = json(&estate);
    assert_eq!(estate["type"], "estate_overview");
    assert_eq!(estate["counts"]["tasks"], 0);
    assert_eq!(estate["counts"]["unresolved_attention"], 0);

    assert_eq!(code(&activity), 0);
    let activity = json(&activity);
    assert_eq!(activity["type"], "activity_brief");
    assert_eq!(activity["owed_total"], 0);
    assert_eq!(activity["cost"]["entries"], 0);

    assert_eq!(code(&cost_rollup), 0);
    let cost_rollup = json(&cost_rollup);
    assert_eq!(cost_rollup["type"], "cost_rollup");
    assert_eq!(cost_rollup["headline"]["entries"], 0);
    assert!(
        cost_rollup["unknowns"]
            .as_array()
            .expect("cost unknowns")
            .iter()
            .any(|note| note["why"]
                .as_str()
                .is_some_and(|note| note.contains("no entries"))),
        "{cost_rollup}"
    );

    assert_eq!(code(&agent_fleet), 0);
    let agent_fleet = json(&agent_fleet);
    assert_eq!(agent_fleet["type"], "agent_fleet");
    assert_eq!(agent_fleet["counts"]["sessions"], 0);
    assert_eq!(agent_fleet["dispatch_nodes"], serde_json::json!([]));

    assert_eq!(code(&sessions), 0);
    assert_eq!(json(&sessions)["records"], serde_json::json!([]));
    assert_eq!(code(&session_missing), 4);
    assert_eq!(json(&session_missing)["code"], "not_found");
    assert_eq!(code(&pty_missing), 4);
    assert_eq!(json(&pty_missing)["code"], "not_found");
    assert_eq!(code(&pty_attach_missing), 4);
    assert_eq!(json(&pty_attach_missing)["code"], "not_found");
    assert_eq!(code(&attempt_budget_missing), 4);
    assert_eq!(json(&attempt_budget_missing)["code"], "not_found");

    // Absent exits 4, distinctly from a refusal and from unavailability.
    assert_eq!(code(&missing), 4);
    assert_eq!(json(&missing)["code"], "not_found");

    assert_eq!(code(&put), 0, "{:?}", String::from_utf8_lossy(&put.stderr));
    let put = json(&put);
    assert_eq!(put["type"], "blob_committed");
    assert_eq!(put["deduplicated"], false);
    let address = put["descriptor"]["address"]
        .as_str()
        .expect("an address")
        .to_owned();

    let back = payload.with_file_name("back.bin");
    let (stat, got) = tokio::task::spawn_blocking({
        let socket = socket.clone();
        let back = back.clone();
        let address = address.clone();
        move || {
            let stat = gw(&socket, &format!("blob stat {address}"));
            let got = gw(
                &socket,
                &format!("blob get {address} --output {}", back.display()),
            );
            (stat, got)
        }
    })
    .await
    .expect("join");

    assert_eq!(code(&stat), 0);
    assert_eq!(json(&stat)["descriptor"]["media_type"], "text/plain");
    assert_eq!(code(&got), 0, "{:?}", String::from_utf8_lossy(&got.stderr));
    assert_eq!(
        std::fs::read(&back).expect("read back"),
        plaintext,
        "the blob did not survive the round trip"
    );

    let _ = stop.send(());
    serving.await.expect("join");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE);"
    )))
    .execute(&maintenance)
    .await;
}

/// A login-capable, NON-superuser role — the only kind a daemon may run as.
///
/// Separate from the role the other live case grants to, which is `NOLOGIN`:
/// this one has to be able to connect, because the whole point is to prove the
/// privilege check passes for a credential `admin init` actually granted.
const DAEMON_ROLE: &str = "gwk_cli_daemon";
/// The throwaway password for that role on an ephemeral test database. Not a
/// secret: the superuser DSN this case is handed already carries CI's own.
const DAEMON_PASSWORD: &str = "ci";

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_admin_door_initializes_a_database_and_the_daemon_serves_it() {
    use base64::prelude::{BASE64_STANDARD, Engine as _};
    use sqlx::PgPool;

    let admin_url = std::env::var("GWK_TEST_ADMIN_DATABASE_URL")
        .expect("GWK_TEST_ADMIN_DATABASE_URL must point at a PostgreSQL superuser DSN");
    let maintenance = PgPool::connect(&admin_url).await.expect("maintenance");
    // Created if absent, then forced into the shape this case needs. Both are
    // allowed to fail: the second is what makes a leftover role from an earlier
    // run usable rather than a reason to stop.
    for statement in [
        format!("CREATE ROLE {DAEMON_ROLE} LOGIN PASSWORD '{DAEMON_PASSWORD}';"),
        format!(
            "ALTER ROLE {DAEMON_ROLE} LOGIN PASSWORD '{DAEMON_PASSWORD}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;"
        ),
    ] {
        let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
            .execute(&maintenance)
            .await;
    }

    let live = format!("gwk_daemon_{}", std::process::id());
    let scratch = format!("gwk_scratch_{}", std::process::id());
    for name in [&live, &scratch] {
        let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name} WITH (FORCE);"
        )))
        .execute(&maintenance)
        .await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name};")))
            .execute(&maintenance)
            .await
            .expect("create database");
    }

    let (prefix, _) = admin_url.rsplit_once('/').expect("a /database suffix");
    let admin_dsn = format!("{prefix}/{live}");
    // The runtime credential: the same server and database, as the granted role
    // rather than as a superuser. A daemon handed a superuser refuses to start,
    // which is a rule this case would otherwise never reach.
    let runtime_dsn = {
        let (scheme, rest) = prefix.split_once("://").expect("a scheme");
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        format!("{scheme}://{DAEMON_ROLE}:{DAEMON_PASSWORD}@{host}/{live}")
    };

    let dir = private_dir("daemon");
    let socket = dir.join("gwk.sock");
    let blob_root = dir.join("blobs");
    let revision = "a1b2c3d4e5".repeat(4);
    let kek = BASE64_STANDARD.encode([0x5a_u8; 32]);
    let blob_root_text = blob_root.to_string_lossy().into_owned();
    let admin_env: Vec<(&str, &str)> = vec![
        ("GWK_ADMIN_DATABASE_URL", &admin_dsn),
        ("GWK_RUNTIME_ROLE", DAEMON_ROLE),
        ("GWK_PUBLIC_REVISION", &revision),
        ("GWK_BLOB_ROOT", &blob_root_text),
        ("GWK_BLOB_KEK", &kek),
        ("GWK_BLOB_KEK_ID", "kek-test"),
    ];

    // The override rule, pinned in both directions rather than assumed to be in
    // one of them: a STAMPED build states a fact about the bytes compiled and
    // refuses the environment, an unstamped one has no fact to state and takes
    // it. Which case this is depends on whether the tree was clean when
    // `build.rs` last ran — so a case that hardcoded `revision` here passed on a
    // developer's tree and failed on CI, where the checkout is clean and the
    // stamp is the merge commit. `build-info` reports the STAMP ONLY, by design;
    // it is the oracle for which case we are in, not for the resolved value.
    let stamp = json(&gw_env("build-info", &admin_env))["public_revision"]
        .as_str()
        .map(str::to_owned);
    let reported = stamp.clone().unwrap_or_else(|| revision.clone());
    assert!(
        reported.len() == 40 && reported.bytes().all(|b| b.is_ascii_hexdigit()),
        "a reported revision is a full hex revision, got {reported:?}"
    );
    if let Some(stamped) = &stamp {
        assert_ne!(
            stamped, &revision,
            "this case can only prove the override rule if the two differ"
        );
    }

    let init = gw_env("admin init", &admin_env);
    assert_eq!(code(&init), 0, "{}", String::from_utf8_lossy(&init.stdout));
    let answer = json(&init);
    assert_eq!(answer["type"], "admin_initialized");
    assert_eq!(answer["outcome"], "initialized");
    assert_eq!(answer["public_revision"], reported);

    // Again, unchanged: the contract is already installed and genesis is
    // idempotent under its own key, so a second run is a no-op and not a second
    // epoch.
    let again = gw_env("admin init", &admin_env);
    assert_eq!(
        code(&again),
        0,
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    assert_eq!(json(&again)["outcome"], "already_initialized");

    let verified = gw_env("admin verify", &admin_env);
    assert_eq!(
        code(&verified),
        0,
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
    let answer = json(&verified);
    assert_eq!(answer["target"], "initialized");
    assert_eq!(answer["runtime_role_exists"], true);
    // The load-bearing assertion of the whole case: the role `admin init`
    // granted to is one the daemon will accept. If the grants and the refusal
    // list ever disagree, `gw daemon` can never start, and nothing short of
    // running both halves finds that.
    assert_eq!(answer["violations"], serde_json::json!([]));
    assert_eq!(
        answer["detail"]["contract_sha256"],
        answer["expected_contract_sha256"]
    );

    let rebuilt = gw_env(
        &format!("admin rebuild-projections --scratch-database {scratch}"),
        &admin_env,
    );
    assert_eq!(
        code(&rebuilt),
        0,
        "{}",
        String::from_utf8_lossy(&rebuilt.stdout)
    );
    let answer = json(&rebuilt);
    // A replay of the log into an empty scratch agrees with the live
    // projections. It also proves the reader store did not fence anybody: the
    // daemon below starts afterwards and appends nothing, but `admin init`
    // already claimed an epoch, and a rebuild that had claimed another would
    // have made this database unwritable.
    assert_eq!(answer["agrees"], true, "{answer}");
    assert_eq!(answer["live_hash"], answer["rebuilt_hash"]);

    // The service itself, as a service: its own process, its own credential.
    let mut serving = Command::new(env!("CARGO_BIN_EXE_gw"))
        .arg("daemon")
        .env("GWK_DATABASE_URL", &runtime_dsn)
        .env("GWK_SOCKET_PATH", &socket)
        .env("GWK_BLOB_ROOT", &blob_root)
        .env("GWK_BLOB_KEK", &kek)
        .env("GWK_BLOB_KEK_ID", "kek-test")
        .env("GWK_PUBLIC_REVISION", &revision)
        .spawn()
        .expect("spawn the daemon");

    // Poll rather than sleep once: a fixed wait is either flaky or slow, and
    // "answers health" is the only definition of started that matters.
    let mut health = None;
    for _ in 0..100 {
        let out = gw(&socket, "kernel health");
        if code(&out) == 0 {
            health = Some(out);
            break;
        }
        assert!(
            serving.try_wait().expect("wait").is_none(),
            "the daemon exited before it served: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let health = health.expect("the daemon never answered health");
    assert_eq!(
        json(&health),
        serde_json::json!({"type": "health", "ready": true, "sealed": true})
    );

    let status = gw(&socket, "kernel status");
    assert_eq!(code(&status), 0);
    let answer = json(&status);
    // The revision the daemon resolved, reported back — the comparison
    // `build-info` and genesis exist for. `reported`, not `revision`: a stamped
    // build ignores the environment on purpose (see above).
    assert_eq!(answer["public_revision"], reported);
    assert_eq!(answer["sealed"], true);

    // Retention, through the admin door rather than the socket — which is the
    // whole reason these four verbs live there. Uploading goes over the wire; pin,
    // unpin, sweep, and shred need a credential, and none of them has a wire
    // request to reach through.
    let payload = dir.join("hold.bin");
    std::fs::write(&payload, b"a blob nothing in the log points at").expect("write a payload");
    let put = gw(
        &socket,
        &format!(
            "blob put --file {} --media-type text/plain",
            payload.display()
        ),
    );
    assert_eq!(code(&put), 0, "{}", String::from_utf8_lossy(&put.stdout));
    let held = json(&put)["descriptor"]["address"]
        .as_str()
        .expect("an address")
        .to_owned();

    let pinned = gw_env(&format!("admin blob pin {held} evidence-1"), &admin_env);
    assert_eq!(
        code(&pinned),
        0,
        "{}",
        String::from_utf8_lossy(&pinned.stdout)
    );
    assert_eq!(json(&pinned)["type"], "blob_pinned");

    // Nothing in this log references the blob, so the only thing standing between
    // it and the sweep is the pin.
    let swept = gw_env("admin blob sweep", &admin_env);
    assert_eq!(
        code(&swept),
        0,
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    let removed = json(&swept);
    assert!(
        !removed["removed"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|address| address == &serde_json::Value::String(held.clone())),
        "a pinned blob was swept: {removed}"
    );
    assert_eq!(code(&gw(&socket, &format!("blob stat {held}"))), 0);

    // Released, and the same sweep reclaims it. Absent afterwards, not
    // tombstoned: a sweep is reclamation of something nothing pointed at, and it
    // leaves no claim behind.
    let unpinned = gw_env(&format!("admin blob unpin {held} evidence-1"), &admin_env);
    assert_eq!(
        code(&unpinned),
        0,
        "{}",
        String::from_utf8_lossy(&unpinned.stdout)
    );
    let swept = gw_env("admin blob sweep", &admin_env);
    assert_eq!(code(&swept), 0);
    assert!(
        json(&swept)["removed"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|address| address == &serde_json::Value::String(held.clone())),
        "the unpinned blob survived a sweep: {}",
        String::from_utf8_lossy(&swept.stdout)
    );
    let gone = gw(&socket, &format!("blob stat {held}"));
    assert_eq!(code(&gone), 4, "{}", String::from_utf8_lossy(&gone.stdout));
    assert_eq!(json(&gone)["code"], "not_found");

    // Shred is the other half and it is the opposite answer: permanent, and
    // REFUSED rather than absent, because a shredded address is a claim the
    // kernel keeps making. The container may still be on disk; the key is gone.
    let doomed = dir.join("shred.bin");
    std::fs::write(&doomed, b"a blob that will be cryptographically erased").expect("write");
    let put = gw(
        &socket,
        &format!(
            "blob put --file {} --media-type text/plain",
            doomed.display()
        ),
    );
    assert_eq!(code(&put), 0);
    let erased = json(&put)["descriptor"]["address"]
        .as_str()
        .expect("an address")
        .to_owned();
    let shredded = gw_env(&format!("admin blob shred {erased}"), &admin_env);
    assert_eq!(
        code(&shredded),
        0,
        "{}",
        String::from_utf8_lossy(&shredded.stdout)
    );
    assert_eq!(json(&shredded)["type"], "blob_shredded");
    let refused = gw(
        &socket,
        &format!(
            "blob get {erased} --output {}",
            dir.join("never.bin").display()
        ),
    );
    assert_eq!(
        code(&refused),
        6,
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_eq!(json(&refused)["code"], "blob_tombstoned");
    assert!(
        !dir.join("never.bin").exists(),
        "a refused read still wrote a file"
    );

    // Rotation, the one admin verb that holds two keys at once. A blob put
    // before it has to survive it, so it is uploaded here and read back after
    // the restart below.
    let carried_file = dir.join("carried.bin");
    std::fs::write(&carried_file, b"a blob that outlives its key").expect("write");
    let put = gw(
        &socket,
        &format!(
            "blob put --file {} --media-type text/plain",
            carried_file.display()
        ),
    );
    assert_eq!(code(&put), 0);
    let carried = json(&put)["descriptor"]["address"]
        .as_str()
        .expect("an address")
        .to_owned();

    let next_kek = BASE64_STANDARD.encode([0x77u8; 32]);
    let rotate_env: Vec<(&str, &str)> = admin_env
        .iter()
        .copied()
        .chain([("GWK_BLOB_KEK_NEXT", next_kek.as_str())])
        .collect();
    let rotated = gw_env("admin blob rotate", &rotate_env);
    assert_eq!(
        code(&rotated),
        0,
        "{}",
        String::from_utf8_lossy(&rotated.stdout)
    );
    let report = json(&rotated);
    assert_eq!(report["type"], "blobs_rotated");
    // The LABEL is unchanged. It lives inside each container's authenticated
    // header, so relabeling would invalidate the AAD the new wrap is bound to —
    // rotation replaces the key behind the name.
    assert_eq!(report["kek_id"], "kek-test");
    assert_eq!(report["rewrapped"], 1);
    assert_eq!(report["already_rotated"], 0);

    // Re-running is safe and finishes rather than faults. This is the state an
    // operator is in when a rotation was interrupted and they cannot tell how
    // far it got: the counts answer that, and `rewrapped: 0` here means done,
    // not "nothing to do".
    let again = gw_env("admin blob rotate", &rotate_env);
    assert_eq!(
        code(&again),
        0,
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    assert_eq!(json(&again)["rewrapped"], 0);
    assert_eq!(json(&again)["already_rotated"], 1);

    // The running daemon is now BLIND to its own blobs: it holds the old key,
    // and nothing told it otherwise. This is the operational hazard the
    // procedure in docs/operations.md exists for — a rotation is not complete
    // until the daemon is restarted with the new key.
    let blinded = gw(
        &socket,
        &format!(
            "blob get {carried} --output {}",
            dir.join("blind.bin").display()
        ),
    );
    assert_ne!(
        code(&blinded),
        0,
        "the daemon read a blob its key no longer opens"
    );

    // A second daemon on the same database must refuse rather than race: one
    // writer per store, enforced by an advisory lock that is never waited on.
    let second = gw_env(
        "daemon",
        &[
            ("GWK_DATABASE_URL", &runtime_dsn),
            (
                "GWK_SOCKET_PATH",
                &dir.join("second.sock").to_string_lossy(),
            ),
            ("GWK_BLOB_ROOT", &blob_root.to_string_lossy()),
            ("GWK_BLOB_KEK", &kek),
            ("GWK_BLOB_KEK_ID", "kek-test"),
            ("GWK_PUBLIC_REVISION", &revision),
        ],
    );
    assert_eq!(
        code(&second),
        3,
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert_eq!(json(&second)["code"], "fenced");

    // SIGKILL: the crash the daemon gets no say in. Not SIGTERM — the point is
    // that no shutdown code runs at all, so nothing releases the writer lock and
    // nothing removes the socket. Everything after this is what the NEXT daemon
    // has to cope with, and it is the only path in this suite that produces a
    // genuinely stale socket beside a genuinely abandoned lock.
    Command::new("kill")
        .args(["-KILL", &serving.id().to_string()])
        .status()
        .expect("signal the daemon");
    let died = serving.wait().expect("wait");
    assert!(!died.success(), "SIGKILL is not a clean exit: {died:?}");
    assert!(
        socket.exists(),
        "a killed daemon cannot have cleaned up after itself"
    );

    // Its connections die with it, and the advisory lock goes when the backend
    // does — which the server learns from a closed socket rather than from the
    // signal, so it is prompt but not synchronous. Waiting for the backends to
    // disappear is the precondition, not a retry loop dressed up as one.
    let mut released = false;
    for _ in 0..100 {
        let live_backends: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND usename = $2",
        )
        .bind(&live)
        .bind(DAEMON_ROLE)
        .fetch_one(&maintenance)
        .await
        .expect("count the daemon's backends");
        if live_backends == 0 {
            released = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(released, "the killed daemon's backends outlived it");

    // A replacement starts on the same socket path and the same database. Three
    // things have to be true at once for this to work: the abandoned advisory
    // lock is takeable (`acquire` never waits, so a lock a corpse still held
    // would exit 3), the stale socket is probed and replaced rather than refused,
    // and recovery reaches a verdict on a log nobody checkpointed on the way out.
    //
    // It also carries the ROTATED key, which is the second half of the rotation
    // above: same variable, same label, different bytes. That is the entire
    // operator-facing swap.
    let mut restarted = Command::new(env!("CARGO_BIN_EXE_gw"))
        .arg("daemon")
        .env("GWK_DATABASE_URL", &runtime_dsn)
        .env("GWK_SOCKET_PATH", &socket)
        .env("GWK_BLOB_ROOT", &blob_root)
        .env("GWK_BLOB_KEK", &next_kek)
        .env("GWK_BLOB_KEK_ID", "kek-test")
        .env("GWK_PUBLIC_REVISION", &revision)
        .spawn()
        .expect("spawn the replacement");

    let mut recovered = None;
    for _ in 0..100 {
        let out = gw(&socket, "kernel health");
        if code(&out) == 0 {
            recovered = Some(out);
            break;
        }
        if let Some(exited) = restarted.try_wait().expect("wait") {
            panic!("the replacement refused to start after a crash: {exited:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let recovered = recovered.expect("the replacement never answered health");
    assert_eq!(json(&recovered)["ready"], true);
    // And it is serving the same log, not a fresh one: the epoch it reports is
    // past the crashed daemon's, because claiming write authority bumps it.
    let status = gw(&socket, "kernel status");
    assert_eq!(code(&status), 0);
    assert_eq!(json(&status)["public_revision"], reported);

    // ...and the blob that was uploaded under the old key reads back byte for
    // byte under the new one. No ciphertext moved to make that true — only the
    // 32 wrapped bytes in its row.
    let recovered_file = dir.join("carried-back.bin");
    let readback = gw(
        &socket,
        &format!("blob get {carried} --output {}", recovered_file.display()),
    );
    assert_eq!(
        code(&readback),
        0,
        "{}",
        String::from_utf8_lossy(&readback.stdout)
    );
    assert_eq!(
        std::fs::read(&recovered_file).expect("read back"),
        b"a blob that outlives its key"
    );

    // SIGTERM is how a service manager asks. The socket goes with it, or the
    // next start would take the stale-takeover path for no reason.
    Command::new("kill")
        .args(["-TERM", &restarted.id().to_string()])
        .status()
        .expect("signal the daemon");
    let stopped = restarted.wait().expect("wait");
    assert!(stopped.success(), "the daemon exited {stopped:?}");
    assert!(!socket.exists(), "shutdown left the socket behind");

    let _ = std::fs::remove_dir_all(&dir);
    for name in [&live, &scratch] {
        let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name} WITH (FORCE);"
        )))
        .execute(&maintenance)
        .await;
    }
}

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gridwork has two ancestors")
        .to_path_buf()
}

/// Every `.service` and `.timer` file in the tree.
fn unit_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `target` is build output and `.git` is history; neither is the
            // tree this pin is about, and both are enormous.
            if path.is_dir() {
                if name != "target" && name != ".git" && name != "node_modules" {
                    walk(&path, found);
                }
            } else if name.ends_with(".service") || name.ends_with(".timer") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    found
}

/// Criterion 9, arm 1: `migrate` reaches the parse tree once.
///
/// The verb exists and is reachable — an unreachable verb is a different
/// failure, and this arm would pass for it too if it only counted absences.
#[test]
fn admin_migrate_is_reachable_from_the_parse_tree_and_named_once() {
    let args = std::fs::read_to_string(repo_root().join("crates/gridwork/src/args.rs"))
        .expect("read args.rs");

    // One dispatch arm. A second would mean two spellings reaching the same
    // verb, and one of them would be the one nobody reviewed.
    let arms = args.matches("\"migrate\" =>").count();
    assert_eq!(
        arms, 1,
        "`\"migrate\" =>` appears {arms} times in the parse tree"
    );
    assert!(
        args.contains("Verb::AdminMigrate"),
        "the parse tree does not construct Verb::AdminMigrate at all"
    );
}

/// Criterion 9, arm 2: no unit file invokes it.
///
/// A migration is an operator act taken with the daemon down and the writer
/// lock held. A systemd unit that could reach it is a migration that runs
/// because a machine rebooted.
#[test]
fn no_unit_file_invokes_admin_migrate() {
    let root = repo_root();
    let units = unit_files(&root);

    // COUNT first. A glob that matched nothing sweeps nothing and passes
    // forever, and this arm would then be a guarantee about a set that does not
    // exist. The number is a floor rather than an equality: adding a unit is
    // routine, and the per-file assertion below is what covers a new one.
    assert!(
        !units.is_empty(),
        "no .service or .timer files were found under {}: this pin is about what they contain, \
         and over an empty sweep every one of them is innocent",
        root.display()
    );

    for unit in &units {
        let text = std::fs::read_to_string(unit)
            .unwrap_or_else(|err| panic!("read {}: {err}", unit.display()));
        assert!(
            !text.contains("admin migrate"),
            "{} invokes `admin migrate`: a migration is an operator act, not something a \
             machine does on its own",
            unit.display()
        );
    }
}

/// Criterion 9, arm 3: the wire cannot ask for it.
///
/// `contracts/bindings.ts` already records the neighbouring refusal in prose —
/// "There is deliberately NO `import`, `migrate`, `backfill`, or `legacy` kind"
/// on `IngestionKind`. A sentence in a generated file is a wish; this makes it
/// a check. The command union is read from the domain crate rather than from
/// the generated bindings, because the bindings are downstream of it and a
/// variant would reach the union first.
#[test]
fn the_command_union_has_no_migrate_variant() {
    let command = std::fs::read_to_string(repo_root().join("crates/gwk-domain/src/command.rs"))
        .expect("read command.rs");

    let union = command
        .split_once("pub enum KernelCommand {")
        .expect("KernelCommand is declared")
        .1;

    // Counted before it is searched, for the same reason as the sweep above: a
    // split that found an empty tail would contain no forbidden variant either.
    let variants = union
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            line.len() - trimmed.len() == 4
                && trimmed
                    .split(['{', ',', ' '])
                    .next()
                    .is_some_and(|word| word.chars().next().is_some_and(char::is_uppercase))
        })
        .count();
    assert!(
        variants > 40,
        "found {variants} variants in KernelCommand, which is too few to be the real union — \
         this pin would then be searching an empty string"
    );

    for forbidden in ["Migrate", "ApplyStep", "Backfill", "AlterSchema"] {
        assert!(
            !union.contains(&format!("    {forbidden} {{")),
            "KernelCommand has a `{forbidden}` variant: a DDL act performed with the daemon down \
             and the writer lock held is not something a client asks for over the wire"
        );
    }

    // And the prose the generated contract already carries stays true.
    let bindings = std::fs::read_to_string(repo_root().join("contracts/bindings.ts"))
        .expect("read bindings.ts");
    assert!(
        bindings
            .contains("There is deliberately NO `import`, `migrate`, `backfill`, or `legacy` kind"),
        "the IngestionKind refusal this pin makes enforceable is no longer in the contract"
    );
}
