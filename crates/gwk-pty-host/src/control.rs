//! Request-driven session starts from the kernel's declared catalog.
//!
//! Derivation: PTY-PROCESS `Command` API - declared program, arguments,
//! working directory, and environment are applied through the public builder.

use std::sync::Arc;

use gwk_domain::{PtySessionId, PtySessionTemplate, ServerControl};
use tokio::sync::{Mutex, watch};

use crate::kernel_client::KernelClient;
use crate::publish;
use crate::registry::SessionRegistry;
use crate::session::SpawnFn;

const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const APPLIED_STARTS: usize = 1_024;
/// Paid before every reconnect, matching the publisher's own backoff.
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
enum StartError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Indeterminate(String),
}

#[derive(Default)]
struct AppliedStarts {
    order: std::collections::VecDeque<gwk_domain::EventId>,
    ids: std::collections::HashSet<gwk_domain::EventId>,
}

impl AppliedStarts {
    fn contains(&self, id: &gwk_domain::EventId) -> bool {
        self.ids.contains(id)
    }

    fn insert(&mut self, id: gwk_domain::EventId) {
        if !self.ids.insert(id.clone()) {
            return;
        }
        self.order.push_back(id);
    }

    fn ids(&self) -> Vec<gwk_domain::EventId> {
        self.order.iter().cloned().collect()
    }

    fn settle(&mut self, id: &gwk_domain::EventId) {
        if self.ids.remove(id) {
            self.order.retain(|candidate| candidate != id);
        }
    }
}

/// Collect ended session threads so a request-minted id can name a later
/// lifetime without waiting for daemon shutdown.
pub async fn serve_reaper(
    registry: Arc<Mutex<SessionRegistry>>,
    publishers: Arc<Mutex<tokio::task::JoinSet<()>>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(REAP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                for (id, exit) in registry.lock().await.reap() {
                    tracing::info!(session = %id, ?exit, "session reaped");
                }
                let mut publishers = publishers.lock().await;
                while let Some(result) = publishers.try_join_next() {
                    if let Err(error) = result {
                        tracing::warn!(%error, "publisher task failed");
                    }
                }
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
            }
        }
    }
}

pub async fn serve_starts(
    socket: std::path::PathBuf,
    registry: Arc<Mutex<SessionRegistry>>,
    publishers: Arc<Mutex<tokio::task::JoinSet<()>>>,
    stop: watch::Receiver<bool>,
) {
    let mut stop = stop;
    let mut applied_starts = AppliedStarts::default();
    let mut reconnecting = false;
    loop {
        if *stop.borrow() {
            return;
        }
        // Every re-entry pays the delay, not just the connect-failure one. A
        // kernel that accepts and then immediately closes — a reconcile it
        // keeps rejecting, say — would otherwise turn this into a connect
        // storm bounded only by socket latency.
        if reconnecting {
            tokio::select! {
                () = tokio::time::sleep(RECONNECT_DELAY) => {}
                _ = stop.changed() => return,
            }
            if *stop.borrow() {
                return;
            }
        }
        reconnecting = true;
        let mut client = match KernelClient::connect_start_manager(&socket).await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "PTY start manager could not connect; retrying");
                continue;
            }
        };
        if let Err(error) = client
            .reconcile_applied_deliveries(applied_starts.ids())
            .await
        {
            tracing::warn!(%error, "PTY start manager could not reconcile applied starts");
            continue;
        }
        loop {
            let control = tokio::select! {
                control = client.wait_for_control() => control,
                _ = stop.changed() => return,
            };
            let control = match control {
                Ok(control) => control,
                Err(error) => {
                    tracing::warn!(%error, "PTY start manager disconnected; retrying");
                    break;
                }
            };
            if let ServerControl::PtyDeliverySettled { delivery_id } = control {
                applied_starts.settle(&delivery_id);
                continue;
            }
            let ServerControl::PtyStart {
                delivery_id,
                template_name,
                session_id,
                ..
            } = control
            else {
                tracing::warn!(?control, "PTY start manager received a non-start control");
                break;
            };
            if applied_starts.contains(&delivery_id) {
                if let Err(error) = client
                    .acknowledge_delivery(
                        delivery_id,
                        gwk_domain::protocol::PtyDeliveryResult::Applied,
                    )
                    .await
                {
                    tracing::warn!(%error, "could not acknowledge replayed declared PTY start");
                    break;
                }
                continue;
            }
            if applied_starts.order.len() >= APPLIED_STARTS {
                let result = gwk_domain::protocol::PtyDeliveryResult::Refused {
                    code: gwk_domain::protocol::KernelErrorCode::Overloaded,
                    message: "the PTY start deduplication window is full of unsettled starts"
                        .to_owned(),
                };
                if let Err(error) = client.acknowledge_delivery(delivery_id, result).await {
                    tracing::warn!(%error, "could not acknowledge saturated PTY start deduplication");
                    break;
                }
                continue;
            }
            let template = match KernelClient::connect(&socket).await {
                Ok(mut reader) => reader.pty_template(&template_name).await,
                Err(error) => Err(error),
            };
            let applied = match template {
                Ok(Some(template)) if template.state == "active" => {
                    start_declared(
                        &registry,
                        &socket,
                        session_id.clone(),
                        template,
                        stop.clone(),
                        Some(&publishers),
                    )
                    .await
                }
                Ok(Some(_)) | Ok(None) => Err(StartError::Refused(format!(
                    "declared PTY start names no active template {template_name}"
                ))),
                Err(error) => Err(StartError::Refused(format!(
                    "could not resolve PTY template {template_name}: {error}"
                ))),
            };
            let result = match applied {
                Ok(()) => {
                    applied_starts.insert(delivery_id.clone());
                    gwk_domain::protocol::PtyDeliveryResult::Applied
                }
                Err(StartError::Refused(message)) => {
                    tracing::warn!(session = %session_id, %message, "declared PTY start refused locally");
                    gwk_domain::protocol::PtyDeliveryResult::Refused {
                        code: gwk_domain::protocol::KernelErrorCode::Validation,
                        message,
                    }
                }
                Err(StartError::Indeterminate(message)) => {
                    tracing::warn!(session = %session_id, %message, "declared PTY start outcome is indeterminate");
                    break;
                }
            };
            if let Err(error) = client.acknowledge_delivery(delivery_id, result).await {
                tracing::warn!(%error, "could not acknowledge declared PTY start");
                break;
            }
        }
    }
}

async fn start_declared(
    registry: &Arc<Mutex<SessionRegistry>>,
    socket: &std::path::Path,
    id: PtySessionId,
    template: PtySessionTemplate,
    stop: watch::Receiver<bool>,
    publishers: Option<&Arc<Mutex<tokio::task::JoinSet<()>>>>,
) -> Result<(), StartError> {
    let config = publish::session_config(template.cols, template.rows);
    registry
        .lock()
        .await
        .spawn(id.clone(), spawn_fn(&template), config)
        .await
        .map_err(|error| StartError::Refused(error.to_string()))?;
    let attacher = registry
        .lock()
        .await
        .attacher(&id)
        .map_err(|error| StartError::Indeterminate(error.to_string()))?;
    if let Some(publishers) = publishers {
        let cleanup = attacher.clone();
        let (ready, published) = tokio::sync::oneshot::channel();
        let publisher = publish::publish_session_with_readiness(
            attacher,
            socket.to_path_buf(),
            id,
            stop,
            ready,
        );
        let task = publishers.lock().await.spawn(publisher);
        match tokio::time::timeout(
            std::time::Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            published,
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(message))) => {
                task.abort();
                let _ = cleanup.stop().await;
                return Err(StartError::Indeterminate(message));
            }
            Ok(Err(_)) => {
                task.abort();
                let _ = cleanup.stop().await;
                return Err(StartError::Indeterminate(
                    "the publisher ended before confirming the initial seed".to_owned(),
                ));
            }
            Err(_) => {
                task.abort();
                let _ = cleanup.stop().await;
                return Err(StartError::Indeterminate(
                    "the initial kernel seed exceeded the delivery deadline".to_owned(),
                ));
            }
        }
    } else {
        tokio::spawn(publish::publish_session(
            attacher,
            socket.to_path_buf(),
            id,
            stop,
        ));
    }
    Ok(())
}

fn spawn_fn(template: &PtySessionTemplate) -> SpawnFn {
    let template = template.clone();
    Box::new(move |cols, rows| {
        let env = template
            .env
            .iter()
            .map(|(key, reference)| {
                let name = reference.strip_prefix("env:").ok_or_else(|| {
                    pty_process::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("template environment {key} is not an env:NAME reference"),
                    ))
                })?;
                let value = std::env::var_os(name).ok_or_else(|| {
                    pty_process::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("template environment source {name} is unavailable"),
                    ))
                })?;
                Ok((key.clone(), value))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, pty_process::Error>>()?;
        let mut command = pty_process::Command::new(&template.command)
            .args(&template.args)
            .env_clear();
        if let Some(cwd) = &template.cwd {
            command = command.current_dir(cwd);
        }
        command = command.envs(&env);
        gwk_pty::Session::spawn(command, cols, rows)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn an_active_catalog_template_starts_a_real_local_session() {
        let registry = Arc::new(Mutex::new(SessionRegistry::new()));
        let (_stop, stopped) = watch::channel(true);
        let id = PtySessionId::new("declared");
        let template = PtySessionTemplate {
            name: gwk_domain::PtySessionTemplateName::new("cat"),
            version: 1,
            state: "active".to_owned(),
            command: "/bin/cat".to_owned(),
            args: vec![],
            cwd: None,
            env: BTreeMap::new(),
            cols: 80,
            rows: 24,
            declared_at: gwk_domain::Timestamp::new("2026-08-12T00:00:00Z"),
            updated_at: gwk_domain::Timestamp::new("2026-08-12T00:00:00Z"),
            retired_at: None,
        };

        start_declared(
            &registry,
            std::path::Path::new("/unused/kernel.sock"),
            id.clone(),
            template,
            stopped,
            None,
        )
        .await
        .expect("declared command starts");
        assert_eq!(registry.lock().await.ids(), vec![id.clone()]);
        registry.lock().await.stop(&id).await.expect("stop child");
    }

    #[tokio::test]
    async fn a_catalog_child_receives_only_the_declared_environment() {
        if std::env::var_os("GWK_ENV_HELPER").is_none() {
            let output =
                tokio::process::Command::new(std::env::current_exe().expect("test binary"))
                    .arg("--exact")
                    .arg("control::tests::a_catalog_child_receives_only_the_declared_environment")
                    .arg("--nocapture")
                    .env("GWK_ENV_HELPER", "1")
                    .env("GWK_UNDECLARED_SECRET", "must-not-leak")
                    .output()
                    .await
                    .expect("run environment helper");
            assert!(
                output.status.success(),
                "environment helper failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        assert_eq!(
            std::env::var("GWK_UNDECLARED_SECRET").as_deref(),
            Ok("must-not-leak"),
            "the helper must begin with an inherited sentinel"
        );
        let registry = Arc::new(Mutex::new(SessionRegistry::new()));
        let (_stop, stopped) = watch::channel(true);
        let id = PtySessionId::new("declared-env");
        let dir = std::env::temp_dir().join(format!(
            "gwk-pty-host-declared-env-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let script = dir.join("print-env");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s|%s\\n' \"${DECLARED_VALUE-unset}\" \"${GWK_UNDECLARED_SECRET-unset}\"\n/bin/sleep 2\n",
        )
        .expect("write script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).expect("make script executable");
        let template = PtySessionTemplate {
            name: gwk_domain::PtySessionTemplateName::new("env"),
            version: 1,
            state: "active".to_owned(),
            command: script.display().to_string(),
            args: vec![],
            cwd: None,
            env: BTreeMap::from([("DECLARED_VALUE".to_owned(), "env:PATH".to_owned())]),
            cols: 80,
            rows: 24,
            declared_at: gwk_domain::Timestamp::new("2026-08-12T00:00:00Z"),
            updated_at: gwk_domain::Timestamp::new("2026-08-12T00:00:00Z"),
            retired_at: None,
        };

        start_declared(
            &registry,
            std::path::Path::new("/unused/kernel.sock"),
            id.clone(),
            template,
            stopped,
            None,
        )
        .await
        .expect("declared command starts");
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(snapshot) = registry.lock().await.snapshot(&id).await {
                    let text = snapshot
                        .frame
                        .cells()
                        .expect("snapshot expands")
                        .into_iter()
                        .flatten()
                        .map(|cell| cell.glyph)
                        .collect::<String>();
                    if text.contains("|unset") || text.contains("must-not-leak") {
                        break text;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("child output arrives");
        let path = std::env::var("PATH").expect("the test process has PATH");
        assert!(output.contains(&format!("{path}|unset")), "{output:?}");
        assert!(!output.contains("must-not-leak"), "{output:?}");
        let _ = registry.lock().await.stop(&id).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_resident_reaper_collects_finished_publisher_tasks() {
        let registry = Arc::new(Mutex::new(SessionRegistry::new()));
        let publishers = Arc::new(Mutex::new(tokio::task::JoinSet::new()));
        publishers.lock().await.spawn(async {});
        let (stop, stopped) = watch::channel(false);
        let reaper = tokio::spawn(serve_reaper(registry, Arc::clone(&publishers), stopped));

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if publishers.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("finished publisher is reaped");
        let _ = stop.send(true);
        reaper.await.expect("join reaper");
    }

    #[test]
    fn start_delivery_deduplication_is_bounded_and_event_derived() {
        let mut applied = AppliedStarts::default();
        let first = gwk_domain::EventId::new("pty_session_start:review:1");
        applied.insert(first.clone());
        applied.insert(first.clone());
        assert!(applied.contains(&first));
        assert_eq!(applied.order.len(), 1);

        assert_eq!(applied.ids(), vec![first.clone()]);
        applied.settle(&first);
        assert!(!applied.contains(&first));
        for index in 0..APPLIED_STARTS {
            applied.insert(gwk_domain::EventId::new(format!("start:{index}")));
        }
        assert_eq!(applied.order.len(), APPLIED_STARTS);
    }
}
