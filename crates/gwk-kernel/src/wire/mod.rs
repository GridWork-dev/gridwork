//! The local wire: frames in, control values out.
//!
//! Three layers, each refusing what the one above it cannot see:
//!
//! * [`frame`] — the `[u32 body_length][u8 kind][body]` codec. Length bounds
//!   are checked against the ANNOUNCED length, before allocation, and the
//!   connection's byte budget is charged for the whole frame.
//! * [`strict`] — the validating JSON decoder ADR 0001 requires: recursive
//!   duplicate keys, unknown fields, invalid UTF-8, trailing bytes.
//! * [`hello`] — the handshake. Major matches exactly, minor negotiates down,
//!   capabilities intersect, and the whole thing is on a deadline.
//! * [`listen`] — the socket itself: where it may live, who may connect, and
//!   what is removed when the daemon stops.
//!
//! Authentication is [`listen`]'s, not the handshake's. The UDS peer credential
//! is the only identity this kernel has (ADR 0002) and it is checked before a
//! single byte of the connection is read, so an unauthorized peer never reaches
//! the decoder at all. Nothing in a hello can widen what a connection may do —
//! `client` is a log label and the capability set is a feature list.

pub mod frame;
pub mod hello;
pub mod listen;
pub mod pty;
pub mod serve;
pub mod strict;
pub mod subscribe;

use gwk_domain::protocol::{KernelErrorCode, KernelResult};

/// A refusal on the wire, already carrying the code it will be sent as.
///
/// Same shape as [`crate::project::Refusal`] and deliberately separate: that
/// one answers a COMMAND and can be turned into a `KernelResult`, while this
/// one can also mean the connection itself is unusable — an I/O failure or a
/// budget exhausted mid-frame has no request to answer.
#[derive(Debug)]
pub struct WireError {
    pub code: KernelErrorCode,
    pub message: String,
    /// True when the connection cannot continue: the framing is out of step, or
    /// the transport failed. A refusal that only concerns one request leaves
    /// this false and the connection open.
    pub fatal: bool,
}

impl WireError {
    pub fn new(code: KernelErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            // Every framing and handshake refusal is fatal by construction: the
            // reader cannot know where the next frame starts once a length or a
            // kind was wrong. Per-request refusals are built by `KernelResult`
            // and never travel as a `WireError`.
            fatal: true,
        }
    }

    pub fn io(what: &str, error: std::io::Error) -> Self {
        Self::new(KernelErrorCode::Storage, format!("{what}: {error}"))
    }

    /// The value form, for the one case where a refusal still owes a response.
    pub fn into_result(self) -> KernelResult {
        KernelResult::Error {
            code: self.code,
            message: self.message,
            detail: None,
        }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}
