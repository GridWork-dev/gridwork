//! `gw` — one binary, and for now one of its three modes.
//!
//! What is here is the CLI: it opens the host-local socket, asks one question,
//! and prints the answer as the protocol produced it. Nothing is reshaped on the
//! way through. A `health` answer is the contract's `health` value and a refusal
//! is the contract's error object, so a caller that already knows the wire needs
//! no second vocabulary for the command line — and this program has no view of
//! its own to drift out of step.
//!
//! Three rules hold everywhere:
//!
//! * **The answer is JSON on standard output.** `--pretty` changes the
//!   formatting and never the value. `--help` is the single exception, because a
//!   caller asking for the command tree is asking for prose.
//! * **The exit says what to do about it** ([`exit`]). One table, derived from
//!   the refusal's own code, so the exit and the JSON can never disagree.
//! * **No database, no key material.** Every verb here goes through the socket.
//!   The database and the KEK belong to `daemon` and `admin`, which is the whole
//!   reason those two are separate verbs.

pub mod args;
pub mod client;
pub mod exit;

use std::path::{Path, PathBuf};

use args::{Invocation, Sink, Source, Verb};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use client::Client;
use exit::Failure;
use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, Origin};
use gwk_domain::ids::{
    AttentionItemId, AuthorityGrantId, ByteCount, CommandId, IdempotencyKey, ProjectId, Seq,
    Timestamp,
};
use gwk_domain::protocol::{
    CONTRACT_VERSION, KernelErrorCode, KernelRequest, KernelResult, ServerControl,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// The revision this build came from, or nothing if it was not stamped.
///
/// `None` is a real answer and not a defect — see `build.rs`. A daemon cannot
/// serve without one, but every verb in this module can, so the CLI reports the
/// absence rather than inventing a value.
pub const PUBLIC_REVISION: Option<&str> = option_env!("GW_PUBLIC_REVISION");

/// Run one invocation and return the exit it earned.
pub async fn run(argv: &[String]) -> u8 {
    let invocation = match args::parse(argv) {
        Ok(invocation) => invocation,
        Err(failure) => return report(&failure, false),
    };
    let Invocation { verb, pretty } = invocation;
    match execute(verb, pretty).await {
        Ok(()) => exit::OK,
        Err(failure) => report(&failure, pretty),
    }
}

/// Print a failure in the protocol's own error shape, and say what it exits as.
fn report(failure: &Failure, pretty: bool) -> u8 {
    emit(&failure.to_json(), pretty);
    failure.exit
}

fn emit(value: &Value, pretty: bool) {
    let rendered = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match rendered {
        Ok(text) => println!("{text}"),
        // Unreachable for a `Value`, and not worth a panic if it ever is.
        Err(e) => eprintln!("gw: could not render an answer: {e}"),
    }
}

async fn execute(verb: Verb, pretty: bool) -> Result<(), Failure> {
    match verb {
        Verb::Help => {
            print!("{}", args::HELP);
            Ok(())
        }
        Verb::BuildInfo => {
            emit(&build_info(), pretty);
            Ok(())
        }

        // Everything below needs the daemon.
        Verb::Health => ask(KernelRequest::Health {}, pretty).await,
        Verb::Status => ask(KernelRequest::Status {}, pretty).await,
        Verb::Watermark => ask(KernelRequest::Watermark {}, pretty).await,
        Verb::VerifySealed => ask(KernelRequest::VerifySealed {}, pretty).await,

        Verb::Activate {
            cutover_id,
            manifest_sha256,
        } => {
            let command = KernelCommand::ActivateKernel {
                cutover_id: cutover_id.clone(),
                archive_manifest_sha256: manifest_sha256,
            };
            // The key is the cutover's, so a retried activation is the SAME
            // activation. A second cutover id is refused by the kernel, which is
            // the check that matters and is not this program's to make.
            submit(&command, &format!("kernel_activated:{cutover_id}"), pretty).await
        }

        Verb::CommandSubmit { source } => {
            let envelope: CommandEnvelope = document(&source)?;
            ask(KernelRequest::SubmitCommand { envelope }, pretty).await
        }

        Verb::ProjectionGet { kind, id } => {
            ask(
                KernelRequest::GetProjection {
                    projection: kind,
                    id,
                },
                pretty,
            )
            .await
        }
        Verb::ProjectionList {
            kind,
            cursor,
            limit,
        } => {
            ask(
                KernelRequest::ListProjection {
                    projection: kind,
                    cursor,
                    limit,
                },
                pretty,
            )
            .await
        }

        Verb::EventRead { cursor, limit } => {
            ask(KernelRequest::ReadEvents { cursor, limit }, pretty).await
        }
        Verb::EventFollow { cursor } => follow(cursor, pretty).await,

        Verb::AttentionResolve { id, resolution } => {
            let command = KernelCommand::ResolveAttention {
                attention_item_id: AttentionItemId::new(id.clone()),
                resolution,
            };
            submit(&command, &format!("resolve_attention:{id}"), pretty).await
        }
        Verb::AuthorityGrant { source } => {
            let envelope: CommandEnvelope = document(&source)?;
            // Checked here rather than left to the kernel, because the kernel
            // would accept it: `gw authority grant` pointed at the wrong file
            // would submit whatever the file held under a verb that promised
            // otherwise.
            expect_command(&envelope, "grant_authority")?;
            ask(KernelRequest::SubmitCommand { envelope }, pretty).await
        }
        Verb::AuthorityRevoke { id, reason } => {
            let command = KernelCommand::RevokeAuthority {
                authority_grant_id: AuthorityGrantId::new(id.clone()),
                reason,
            };
            submit(&command, &format!("revoke_authority:{id}"), pretty).await
        }

        Verb::BlobPut { source, media_type } => blob_put(&source, media_type, pretty).await,
        Verb::BlobGet { address, output } => blob_get(&address, &output, pretty).await,
        Verb::BlobStat { address } => ask(KernelRequest::BlobStat { address }, pretty).await,

        Verb::IngestSubmit {
            kind,
            source,
            project,
            key,
        } => {
            let payload: Value = document(&source)?;
            let key =
                key.unwrap_or_else(|| format!("{}:{}", kind.as_str(), digest_of_json(&payload)));
            let command = KernelCommand::IngestRecord {
                kind,
                payload,
                payload_ref: None,
            };
            let envelope = envelope(
                &command,
                &key,
                project.as_deref().unwrap_or(gwk_kernel::SYSTEM_PROJECT),
            );
            ask(KernelRequest::SubmitCommand { envelope }, pretty).await
        }
    }
}

/// What this build is, without asking anything.
fn build_info() -> Value {
    serde_json::json!({
        "type": "build_info",
        "crate_version": env!("CARGO_PKG_VERSION"),
        "contract_version": CONTRACT_VERSION,
        // Null when the build was not stamped from a clean checkout. A caller
        // comparing this against the revision genesis recorded needs the
        // absence to be visible, not papered over.
        "public_revision": PUBLIC_REVISION,
        "socket_path": socket_path().to_string_lossy(),
    })
}

/// The socket every client verb uses.
fn socket_path() -> PathBuf {
    std::env::var_os(gwk_kernel::config::SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(gwk_kernel::DEFAULT_SOCKET_PATH))
}

async fn connect() -> Result<Client, Failure> {
    let (client, _ack) = Client::connect(&socket_path()).await?;
    Ok(client)
}

/// One request, one answer, printed as the kernel produced it.
async fn ask(request: KernelRequest, pretty: bool) -> Result<(), Failure> {
    let result = connect().await?.ask(request).await?;
    answer(result, pretty)
}

/// A result printed, or turned into the failure it is.
fn answer(result: KernelResult, pretty: bool) -> Result<(), Failure> {
    if let KernelResult::Error {
        code,
        message,
        detail,
    } = result
    {
        let mut failure = Failure::new(code, message);
        if let Some(detail) = detail {
            // Kept, because the contract puts machine-readable specifics there —
            // the version behind a stale_version, the field behind a validation.
            failure.message = format!("{}: {detail}", failure.message);
        }
        return Err(failure);
    }
    let value = serde_json::to_value(&result)
        .map_err(|e| Failure::internal(format!("render an answer: {e}")))?;
    emit(&value, pretty);
    Ok(())
}

/// Submit a command this program minted, in the kernel's own project.
///
/// The project scopes the idempotency key while the aggregate namespace is
/// global, so a convenience verb naming one id does not need to know which
/// project the aggregate belongs to — and a retry of the same verb is the same
/// command. A caller who needs a different project writes the envelope itself
/// and submits it with `gw command submit`.
async fn submit(command: &KernelCommand, key: &str, pretty: bool) -> Result<(), Failure> {
    let envelope = envelope(command, key, gwk_kernel::SYSTEM_PROJECT);
    ask(KernelRequest::SubmitCommand { envelope }, pretty).await
}

fn envelope(command: &KernelCommand, key: &str, project: &str) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(format!("cmd-{key}")),
        project_id: ProjectId::new(project),
        command_type: command.command_type().to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        // The CALLER's clock, which is what `issued_at` means. The kernel stamps
        // its own `appended_at` from the database, and that one is authoritative
        // for order — so these two disagreeing is information, not a conflict.
        issued_at: now(),
        actor: Actor {
            kind: "operator".to_owned(),
            id: None,
        },
        origin: Origin {
            system: "gw".to_owned(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new(key),
        causation_id: None,
        correlation_id: None,
        // Infallible for a command built here — every variant serializes — and
        // a null payload would be refused by the kernel anyway.
        payload: serde_json::to_value(command).unwrap_or(Value::Null),
    }
}

fn expect_command(envelope: &CommandEnvelope, wanted: &str) -> Result<(), Failure> {
    if envelope.command_type == wanted {
        return Ok(());
    }
    Err(Failure::usage(format!(
        "this verb submits {wanted}, and the envelope is a {}",
        envelope.command_type
    )))
}

/// Follow the log until the stream ends or the daemon hangs up.
///
/// One JSON object per line, one line per event — not per batch. Batching is the
/// transport's business, and a consumer that wants to resume reads
/// `global_sequence` off the last line it managed to handle.
async fn follow(cursor: Option<Seq>, pretty: bool) -> Result<(), Failure> {
    let mut client = connect().await?;
    let stream = client.subscribe(cursor).await?;
    loop {
        let Some(control) = client.receive().await? else {
            // The daemon hung up. Not an error: a drain on shutdown looks
            // exactly like this, and the caller has every event it printed.
            return Ok(());
        };
        match control {
            ServerControl::EventBatch {
                request_id, events, ..
            } if request_id == stream => {
                for event in &events {
                    let value = serde_json::to_value(event)
                        .map_err(|e| Failure::internal(format!("render an event: {e}")))?;
                    emit(&value, pretty);
                }
            }
            ServerControl::StreamClosed {
                request_id,
                code,
                last_cursor,
            } if request_id == stream => {
                // The cursor is what was DELIVERED, so it belongs in the message:
                // it is the one piece of information that makes the next attempt
                // gap-free.
                let resume = last_cursor
                    .map(|seq| seq.value().to_string())
                    .unwrap_or_else(|| "the beginning".to_owned());
                return Err(Failure::new(
                    code,
                    format!("the stream closed; resume from {resume}"),
                ));
            }
            // Another subscription's traffic, or a response to a request this
            // process never made. Not ours to interpret.
            _ => {}
        }
    }
}

/// Upload one blob and print the descriptor the commit reported.
async fn blob_put(source: &Source, media_type: String, pretty: bool) -> Result<(), Failure> {
    let plaintext = bytes(source)?;
    let address = address_of(&plaintext);
    let mut client = connect().await?;

    let upload = match client
        .ask(KernelRequest::BlobBegin {
            media_type,
            byte_size: ByteCount::new(plaintext.len() as u64),
        })
        .await?
    {
        KernelResult::BlobBegun { upload_id } => upload_id,
        other => return answer(other, pretty),
    };

    // An empty blob still writes one chunk. `chunks` yields nothing for an empty
    // slice, and an upload that never wrote anything is not the same thing to the
    // store as one that wrote nothing.
    let pieces: Vec<&[u8]> = if plaintext.is_empty() {
        vec![&[]]
    } else {
        plaintext.chunks(BLOB_CHUNK_BYTES).collect()
    };
    for (sequence, chunk) in pieces.into_iter().enumerate() {
        let sequence = u32::try_from(sequence)
            .map_err(|_| Failure::usage("this blob has more chunks than the protocol counts"))?;
        match client
            .ask(KernelRequest::BlobChunk {
                upload_id: upload.clone(),
                sequence,
                data_base64: BASE64_STANDARD.encode(chunk),
            })
            .await?
        {
            KernelResult::BlobChunkAccepted { .. } => {}
            other => return answer(other, pretty),
        }
    }

    let result = client
        .ask(KernelRequest::BlobCommit {
            upload_id: upload,
            address,
        })
        .await?;
    answer(result, pretty)
}

/// Read one blob whole, a frame at a time.
async fn blob_get(address: &BlobAddress, output: &Sink, pretty: bool) -> Result<(), Failure> {
    let mut client = connect().await?;
    // Its size first, because a read is clamped to one chunk and the loop has to
    // know when it is done. `stat` is also what distinguishes an absent blob from
    // a shredded one before any bytes move.
    let size = match client
        .ask(KernelRequest::BlobStat {
            address: address.clone(),
        })
        .await?
    {
        KernelResult::BlobStat { descriptor } => descriptor.byte_size.value(),
        other => return answer(other, pretty),
    };

    let mut bytes: Vec<u8> = Vec::with_capacity(size as usize);
    while (bytes.len() as u64) < size {
        let result = client
            .ask(KernelRequest::BlobRead {
                address: address.clone(),
                offset: ByteCount::new(bytes.len() as u64),
                length: ByteCount::new(size - bytes.len() as u64),
            })
            .await?;
        let KernelResult::BlobBytes { data_base64, .. } = result else {
            return answer(result, pretty);
        };
        let part = BASE64_STANDARD
            .decode(&data_base64)
            .map_err(|e| Failure::new(KernelErrorCode::BlobIntegrity, format!("base64: {e}")))?;
        if part.is_empty() {
            return Err(Failure::new(
                KernelErrorCode::BlobIntegrity,
                format!("the read stalled at {} of {size} bytes", bytes.len()),
            ));
        }
        bytes.extend_from_slice(&part);
    }

    match output {
        // Raw bytes, and nothing else — a caller piping a blob somewhere does
        // not want a JSON object in the middle of it.
        Sink::Stdout => tokio::io::stdout()
            .write_all(&bytes)
            .await
            .map_err(|e| Failure::internal(format!("write to standard output: {e}")))?,
        Sink::File(path) => {
            write_file(path, &bytes)?;
            emit(
                &serde_json::json!({
                    "type": "blob_written",
                    "address": address.as_str(),
                    "byte_size": size.to_string(),
                    "path": path.to_string_lossy(),
                }),
                pretty,
            );
        }
    }
    Ok(())
}

/// A JSON document from a file or standard input, decoded into `T`.
///
/// The contract types refuse unknown fields, so a document with a stray key is a
/// refusal here rather than a field the kernel silently ignored.
fn document<T: serde::de::DeserializeOwned>(source: &Source) -> Result<T, Failure> {
    let raw = bytes(source)?;
    serde_json::from_slice(&raw).map_err(|e| Failure::usage(format!("{source:?}: {e}")))
}

fn bytes(source: &Source) -> Result<Vec<u8>, Failure> {
    match source {
        Source::Stdin => {
            use std::io::Read as _;
            let mut buffer = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buffer)
                .map_err(|e| Failure::usage(format!("read standard input: {e}")))?;
            Ok(buffer)
        }
        Source::File(path) => {
            std::fs::read(path).map_err(|e| Failure::usage(format!("read {}: {e}", path.display())))
        }
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    std::fs::write(path, bytes)
        .map_err(|e| Failure::usage(format!("write {}: {e}", path.display())))
}

/// The address a blob will have: a digest over its PLAINTEXT, which is what
/// makes deduplication work across encryptions of the same content.
fn address_of(plaintext: &[u8]) -> BlobAddress {
    BlobAddress::from_digest(&hex_lower(plaintext))
        // 64 lowercase hex characters by construction.
        .unwrap_or_else(|_| BlobAddress::parse("sha256:").expect("unreachable"))
}

fn digest_of_json(value: &Value) -> String {
    hex_lower(serde_json::to_string(value).unwrap_or_default().as_bytes())
}

fn hex_lower(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut out = String::with_capacity(64);
    for byte in digest {
        // Two lowercase hex digits, without a formatting dependency.
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Now, as RFC 3339 in UTC.
///
/// Written out rather than taken from a date library, because this is the only
/// place the CLI needs a calendar and the conversion is a dozen lines. UTC only:
/// a local offset in an `issued_at` would be a second thing to reconcile against
/// the kernel's own UTC-pinned clock.
fn now() -> Timestamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    Timestamp::new(rfc3339(secs))
}

fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Days since the epoch to a civil date, by Howard Hinnant's algorithm — the
/// standard one, shifted to an era starting in March so a leap day lands last.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_renders_dates_a_database_will_accept() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(rfc3339(86_400), "1970-01-02T00:00:00Z");
        // The case a naive conversion gets wrong: a leap day in a century year
        // that IS a leap year.
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(951_868_800), "2000-03-01T00:00:00Z");
        // And a century year that is NOT a leap year: February ends at the 28th
        // and the next day is March, with no 29th in between.
        assert_eq!(rfc3339(4_107_456_000), "2100-02-28T00:00:00Z");
        assert_eq!(rfc3339(4_107_456_000 + 86_400), "2100-03-01T00:00:00Z");
        // Monotonic as text, which is what makes these sortable at all.
        assert!(rfc3339(0) < rfc3339(86_400));
    }

    #[test]
    fn a_blob_address_is_the_digest_of_its_own_plaintext() {
        // The empty-string SHA-256, which is the one digest worth hard-coding:
        // it catches a hex table with its nibbles the wrong way round, which a
        // round-trip test would not.
        assert_eq!(
            address_of(b"").as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(address_of(b"a").as_str(), address_of(b"b").as_str());
    }

    #[test]
    fn a_minted_envelope_is_the_same_envelope_on_a_retry() {
        let command = KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("a-1"),
            resolution: None,
        };
        let one = envelope(&command, "resolve_attention:a-1", "system");
        let two = envelope(&command, "resolve_attention:a-1", "system");
        // Everything but the clock: the id and the key are derived from the
        // request, which is what makes a repeated `gw attention resolve` land
        // once rather than twice.
        assert_eq!(one.command_id, two.command_id);
        assert_eq!(one.idempotency_key, two.idempotency_key);
        assert_eq!(one.command_type, "resolve_attention");
        assert_eq!(one.payload, two.payload);
    }

    #[test]
    fn a_verb_that_promises_one_command_refuses_another() {
        let command = KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("a-1"),
            resolution: None,
        };
        let envelope = envelope(&command, "k", "system");
        assert!(expect_command(&envelope, "resolve_attention").is_ok());
        let wrong = expect_command(&envelope, "grant_authority").expect_err("refused");
        assert_eq!(wrong.exit, exit::USAGE);
    }
}
