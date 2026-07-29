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
    Command::new(env!("CARGO_BIN_EXE_gw"))
        .args(line.split_whitespace())
        .env("GWK_SOCKET_PATH", socket)
        .output()
        .expect("run gw")
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
            health, status, events, tasks, missing, put, plaintext, payload,
        )
    })
    .await
    .expect("join");
    let (health, status, events, tasks, missing, put, plaintext, payload) = answers;

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
