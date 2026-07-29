//! The handshake: the first frame, on a deadline, and what it settles.
//!
//! Three things are negotiated and one is not. The MAJOR version must match
//! exactly — an unknown major is a refusal, never a best-effort session, because
//! two peers that disagree about the frame grammar cannot discover it by trying.
//! The MINOR negotiates DOWNWARD to `min(client, server)`, so a newer client
//! degrades rather than failing. Capabilities INTERSECT: the client asks, the
//! kernel answers with what it also has, and a client must treat anything
//! absent as unavailable.
//!
//! What is not negotiated is authority. A capability is a feature flag, not a
//! grant — ADR 0002 makes the UDS peer credential the only identity here — and
//! `client` is a free-form label for logs. Nothing in a hello can widen what the
//! connection may do, which is why the handshake can be this permissive about
//! what it accepts and this strict about what it promises.

use gwk_domain::ids::Seq;
use gwk_domain::protocol::{
    CapabilityName, ClientControl, FrameKind, HELLO_DEADLINE_SECS, HELLO_MAX_BYTES,
    KernelErrorCode, MAX_CAPABILITIES, PROTOCOL_MINOR, ProtocolVersion, ServerControl,
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::frame::{Budget, Incoming, read_frame, write_frame};
use super::{WireError, strict};

/// What this kernel offers to negotiate.
///
/// Empty, and that is the honest answer for 4′: every request in the contract
/// is served by every 4′ daemon, so there is no optional surface to advertise.
/// A name here would be something a client branches on, and once branched on it
/// is owed forever — including by the 5′ daemon that would have to keep meaning
/// the same thing by it.
pub const OFFERED_CAPABILITIES: &[&str] = &[];

/// What a completed handshake settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub minor: u32,
    pub capabilities: Vec<CapabilityName>,
    pub client: Option<String>,
}

/// The kernel state a `HelloAck` reports.
#[derive(Debug, Clone, Copy)]
pub struct Readiness {
    pub sealed: bool,
    pub watermark: Option<Seq>,
}

/// Read the first frame and settle the session, or refuse and say why.
///
/// On refusal the client is sent a `HelloRefusal` before the connection closes:
/// a peer that guessed a major version wrong has to be able to tell that from a
/// socket that was never there. The refusal is still returned to the caller, so
/// the connection is closed by the one place that owns closing it.
pub async fn negotiate<R, W>(
    reader: &mut R,
    writer: &mut W,
    budget: &mut Budget,
    readiness: Readiness,
) -> Result<Session, WireError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let outcome = read_hello(reader, budget).await;
    match outcome {
        Ok(session) => {
            let ack = ServerControl::HelloAck {
                protocol_major: ProtocolVersion::V1,
                protocol_minor: session.minor,
                capabilities: session.capabilities.clone(),
                sealed: readiness.sealed,
                watermark: readiness.watermark,
            };
            send(writer, budget, &ack).await?;
            Ok(session)
        }
        Err(refusal) => {
            // Best effort: the budget may be exhausted or the socket already
            // gone, and the refusal the caller gets must be the original one
            // rather than whatever failed while reporting it.
            let told = ServerControl::HelloRefusal {
                code: refusal.code,
                message: refusal.message.clone(),
            };
            let _ = send(writer, budget, &told).await;
            Err(refusal)
        }
    }
}

async fn read_hello<R>(reader: &mut R, budget: &mut Budget) -> Result<Session, WireError>
where
    R: AsyncRead + Unpin,
{
    // The deadline covers reading the frame, not parsing it: a peer that opens
    // a connection and says nothing is the case this exists for, and it costs a
    // file descriptor for as long as it is allowed to.
    let deadline = std::time::Duration::from_secs(HELLO_DEADLINE_SECS);
    let incoming = tokio::time::timeout(deadline, read_frame(reader, HELLO_MAX_BYTES, budget))
        .await
        .map_err(|_| {
            WireError::new(
                KernelErrorCode::Handshake,
                format!("no hello within {HELLO_DEADLINE_SECS}s"),
            )
        })??;

    let frame = match incoming {
        Incoming::Frame(frame) => frame,
        Incoming::Closed => {
            return Err(WireError::new(
                KernelErrorCode::Handshake,
                "the connection closed before its hello",
            ));
        }
    };
    if frame.kind != FrameKind::Json {
        return Err(WireError::new(
            KernelErrorCode::Handshake,
            format!("the first frame must be JSON, not kind {:?}", frame.kind),
        ));
    }

    let control: ClientControl = strict::decode(&frame.body)?;
    let (protocol_major, protocol_minor, capabilities, client) = match control {
        ClientControl::Hello {
            protocol_major,
            protocol_minor,
            capabilities,
            client,
        } => (protocol_major, protocol_minor, capabilities, client),
        // A request before a hello is refused rather than served: the session
        // has not agreed on a grammar yet, so serving it would mean guessing
        // which one it was written against.
        ClientControl::Request { .. } => {
            return Err(WireError::new(
                KernelErrorCode::Handshake,
                "the first frame must be a hello, not a request",
            ));
        }
    };

    // Redundant today — `ProtocolVersion` refuses an unknown major at
    // `Deserialize` time, so an unsupported one never reaches here as a value.
    // Kept because the day a second major exists this becomes the only check
    // that says "known, but not this connection's", and the `Version` code is
    // what a client distinguishes that by.
    if protocol_major != ProtocolVersion::V1 {
        return Err(WireError::new(
            KernelErrorCode::UnsupportedVersion,
            format!(
                "protocol major {protocol_major} is not {}",
                ProtocolVersion::V1
            ),
        ));
    }

    if capabilities.len() > MAX_CAPABILITIES {
        return Err(WireError::new(
            KernelErrorCode::Capability,
            format!(
                "{} capabilities exceeds the {MAX_CAPABILITIES} maximum",
                capabilities.len()
            ),
        ));
    }

    Ok(Session {
        // Downward, so a client speaking a minor this kernel has not heard of
        // gets a session at the minor they share instead of a refusal.
        //
        // Clippy is right that this is a no-op while `PROTOCOL_MINOR` is 0, and
        // wrong that it should therefore be written as the constant: the min IS
        // the negotiation rule, and the version of this line that says
        // `PROTOCOL_MINOR` would keep compiling — and start lying — on the day
        // the minor first moves.
        #[allow(clippy::unnecessary_min_or_max)]
        minor: protocol_minor.min(PROTOCOL_MINOR),
        capabilities: intersect(&capabilities),
        client,
    })
}

/// What both sides have. Order follows the CLIENT's list so the answer reads as
/// a reply to what was asked, and duplicates in the request collapse.
fn intersect(asked: &[CapabilityName]) -> Vec<CapabilityName> {
    let mut out: Vec<CapabilityName> = Vec::new();
    for name in asked {
        if OFFERED_CAPABILITIES.contains(&name.as_str()) && !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

async fn send<W>(
    writer: &mut W,
    budget: &mut Budget,
    control: &ServerControl,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(control).map_err(|e| {
        WireError::new(
            KernelErrorCode::Storage,
            format!("serialize server control: {e}"),
        )
    })?;
    write_frame(writer, FrameKind::Json, &body, budget).await
}

#[cfg(test)]
mod tests {
    use gwk_domain::protocol::{
        CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW,
        FRAME_BODY_MAX_BYTES,
    };

    use super::*;

    fn budget() -> Budget {
        Budget::new(
            CONNECTION_INGRESS_BYTES_PER_WINDOW,
            CONNECTION_EGRESS_BYTES_PER_WINDOW,
        )
    }

    fn ready() -> Readiness {
        Readiness {
            sealed: true,
            watermark: Some(Seq::new(1)),
        }
    }

    /// Frame `raw` as a client would, run the handshake, and hand back both the
    /// outcome and whatever the kernel said on the wire.
    async fn handshake(raw: &str) -> (Result<Session, WireError>, ServerControl) {
        let mut client_bytes = Vec::new();
        write_frame(
            &mut client_bytes,
            FrameKind::Json,
            raw.as_bytes(),
            &mut budget(),
        )
        .await
        .expect("frame the hello");

        let mut reader = std::io::Cursor::new(client_bytes);
        let mut written = Vec::new();
        let mut b = budget();
        let outcome = negotiate(&mut reader, &mut written, &mut b, ready()).await;

        let mut back = std::io::Cursor::new(written);
        let answer = match read_frame(&mut back, FRAME_BODY_MAX_BYTES, &mut budget())
            .await
            .expect("the kernel answered")
        {
            Incoming::Frame(frame) => {
                strict::decode::<ServerControl>(&frame.body).expect("decode the answer")
            }
            Incoming::Closed => panic!("the kernel said nothing"),
        };
        (outcome, answer)
    }

    #[tokio::test]
    async fn a_matching_hello_is_acked_with_the_kernels_state() {
        let (session, answer) = handshake(
            r#"{"type":"hello","protocol_major":1,"protocol_minor":0,"capabilities":[],"client":"gw/0.0.1"}"#,
        )
        .await;
        let session = session.expect("negotiate");
        assert_eq!(session.minor, 0);
        assert_eq!(session.client.as_deref(), Some("gw/0.0.1"));
        match answer {
            ServerControl::HelloAck {
                protocol_major,
                protocol_minor,
                capabilities,
                sealed,
                watermark,
            } => {
                assert_eq!(protocol_major, ProtocolVersion::V1);
                assert_eq!(protocol_minor, 0);
                assert!(capabilities.is_empty());
                // A sealed kernel says so in the ack, so a client knows before
                // its first request that only the epoch commands are admitted.
                assert!(sealed);
                assert_eq!(watermark, Some(Seq::new(1)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_major_is_refused_in_words_the_client_can_read() {
        // Not a dropped socket: a peer that guessed the major wrong must be
        // able to tell that from a daemon that was never listening.
        let (outcome, answer) = handshake(
            r#"{"type":"hello","protocol_major":2,"protocol_minor":0,"capabilities":[]}"#,
        )
        .await;
        let error = outcome.expect_err("major 2 accepted");
        assert!(matches!(
            error.code,
            KernelErrorCode::UnsupportedVersion | KernelErrorCode::Validation
        ));
        match answer {
            ServerControl::HelloRefusal { message, .. } => {
                assert!(message.contains('2'), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_newer_minor_negotiates_down_instead_of_failing() {
        let (session, answer) = handshake(
            r#"{"type":"hello","protocol_major":1,"protocol_minor":99,"capabilities":[]}"#,
        )
        .await;
        assert_eq!(session.expect("negotiate").minor, PROTOCOL_MINOR);
        assert!(matches!(
            answer,
            ServerControl::HelloAck { protocol_minor, .. } if protocol_minor == PROTOCOL_MINOR
        ));
    }

    #[tokio::test]
    async fn capabilities_intersect_and_this_kernel_offers_none() {
        // The machinery is live — a client may ask for anything and gets back
        // what both sides have, which in 4′ is nothing. Answering with what was
        // asked would be a promise no code keeps.
        let (session, _) = handshake(
            r#"{"type":"hello","protocol_major":1,"protocol_minor":0,"capabilities":["event_subscribe","blob_transfer"]}"#,
        )
        .await;
        assert!(session.expect("negotiate").capabilities.is_empty());
    }

    #[tokio::test]
    async fn too_many_capabilities_is_a_typed_refusal() {
        let names: Vec<String> = (0..=MAX_CAPABILITIES)
            .map(|i| format!("\"cap_{i}\""))
            .collect();
        let (outcome, _) = handshake(&format!(
            r#"{{"type":"hello","protocol_major":1,"protocol_minor":0,"capabilities":[{}]}}"#,
            names.join(",")
        ))
        .await;
        let error = outcome.expect_err("65 capabilities accepted");
        assert_eq!(error.code, KernelErrorCode::Capability);
    }

    #[tokio::test]
    async fn a_request_before_a_hello_is_refused() {
        let (outcome, _) =
            handshake(r#"{"type":"request","request_id":"r-1","request":{"type":"health"}}"#).await;
        let error = outcome.expect_err("a request served before the handshake");
        assert_eq!(error.code, KernelErrorCode::Handshake);
    }

    #[tokio::test]
    async fn silence_is_closed_on_the_deadline() {
        // A peer that connects and says nothing holds a descriptor. `tokio`'s
        // test clock makes the five seconds pass without spending them.
        tokio::time::pause();
        let (client, mut server) = tokio::io::duplex(64);
        let mut b = budget();
        let mut written = Vec::new();
        let handshaking = tokio::spawn(async move {
            negotiate(&mut server, &mut written, &mut b, ready())
                .await
                .map(|_| ())
        });
        tokio::time::advance(std::time::Duration::from_secs(HELLO_DEADLINE_SECS + 1)).await;
        let error = handshaking
            .await
            .expect("join")
            .expect_err("silence accepted");
        assert_eq!(error.code, KernelErrorCode::Handshake);
        assert!(error.message.contains("hello"), "{error}");
        drop(client);
    }

    #[tokio::test]
    async fn an_oversized_hello_is_refused_at_its_own_lower_cap() {
        // 64 KiB, not the 4 MiB every later frame gets: the handshake is the
        // one frame an unauthenticated peer can send, so it is bounded hardest.
        let mut raw = (HELLO_MAX_BYTES + 1).to_be_bytes().to_vec();
        raw.push(FrameKind::Json.as_u8());
        let mut reader = std::io::Cursor::new(raw);
        let mut written = Vec::new();
        let error = negotiate(&mut reader, &mut written, &mut budget(), ready())
            .await
            .expect_err("oversized hello accepted");
        assert_eq!(error.code, KernelErrorCode::FrameSize);
    }
}
