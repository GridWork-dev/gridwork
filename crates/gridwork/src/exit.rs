//! What an invocation is worth to a caller: one error shape, one exit table.
//!
//! Every failure — a refusal from the kernel, an unreadable file, a socket that
//! is not there — becomes the SAME JSON object the protocol already defines for
//! a refusal, and its exit code is derived from that object's code. One table,
//! so a script that branches on the exit and a script that reads the JSON can
//! never disagree.
//!
//! The codes answer "what should the caller do?", which is why they group the way
//! they do rather than one per error:
//!
//! * `2` — fix your input. Nothing was attempted.
//! * `3` — the kernel said no on grounds of state, authority, or a fence. The
//!   request was well formed and the answer is still no.
//! * `4` — it is not there.
//! * `5` — the kernel is not usable right now. Retrying later is the fix.
//! * `6` — something does not verify. Retrying is NOT the fix.
//! * `10` — a bug here. The one code no kernel refusal can produce.

use gwk_domain::protocol::KernelErrorCode;

pub const OK: u8 = 0;
pub const USAGE: u8 = 2;
pub const REFUSED: u8 = 3;
pub const NOT_FOUND: u8 = 4;
pub const UNAVAILABLE: u8 = 5;
pub const INTEGRITY: u8 = 6;
pub const INTERNAL: u8 = 10;

/// The exit a refusal earns.
pub const fn exit_for(code: KernelErrorCode) -> u8 {
    match code {
        // The request itself is wrong: malformed, duplicated, or too big.
        KernelErrorCode::Validation
        | KernelErrorCode::DuplicateKey
        | KernelErrorCode::FrameSize => USAGE,

        // Well formed, and refused anyway.
        KernelErrorCode::Sealed
        | KernelErrorCode::AlreadyActive
        | KernelErrorCode::StaleVersion
        | KernelErrorCode::IllegalEdge
        | KernelErrorCode::Authority
        | KernelErrorCode::IdempotencyConflict
        | KernelErrorCode::Fenced => REFUSED,

        KernelErrorCode::NotFound => NOT_FOUND,

        // The command is durably terminal, but the kernel cannot prove whether
        // its side effect applied. Blind retry is unsafe, so this is a refused
        // state rather than transient unavailability.
        KernelErrorCode::Indeterminate => REFUSED,

        // Not usable now. A handshake that cannot be agreed and a role the
        // daemon refuses to run with belong here rather than under "your
        // input": nothing the caller passed would have made them succeed, and
        // the fix — an upgrade, a grant — makes a later attempt work.
        KernelErrorCode::Handshake
        | KernelErrorCode::UnsupportedVersion
        | KernelErrorCode::Capability
        | KernelErrorCode::Overloaded
        | KernelErrorCode::SlowConsumer
        | KernelErrorCode::Privilege
        | KernelErrorCode::Storage => UNAVAILABLE,

        // Does not verify. A schema with no upcaster is in this group and not in
        // the retryable one on purpose: the bytes will not become readable.
        KernelErrorCode::Schema
        | KernelErrorCode::BlobIntegrity
        | KernelErrorCode::BlobTombstoned => INTEGRITY,
    }
}

/// A failure, already carrying the code it will be reported as.
#[derive(Debug)]
pub struct Failure {
    pub code: KernelErrorCode,
    pub message: String,
    /// Overridden only for [`Failure::internal`]; otherwise [`exit_for`].
    pub exit: u8,
}

impl Failure {
    pub fn new(code: KernelErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            exit: exit_for(code),
        }
    }

    /// Bad arguments, an unreadable file, a name that is not a projection.
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(KernelErrorCode::Validation, message)
    }

    /// The kernel could not be reached or the transport failed.
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self::new(KernelErrorCode::Storage, message)
    }

    /// A tool this program delegated to said no — today that is `gh`.
    /// Reported as `storage` because the contract has no code for another
    /// program's refusal (the same gap [`Failure::internal`] documents), and
    /// exited by the one table, as `5`: from here gw cannot tell a transient
    /// no (expired auth, a network, checks still running) from a final one
    /// (a closed PR) — the reason is on the tool's own standard error, which
    /// is where a caller looks before deciding to retry. What must NOT
    /// happen is the JSON and the exit telling two stories.
    pub fn external(message: impl Into<String>) -> Self {
        Self::new(KernelErrorCode::Storage, message)
    }

    /// A fault in this program. Reported as `storage` because the contract has no
    /// code for "the CLI is broken", and exited as `10` because that is what the
    /// caller needs to know. A panic still exits `101` — also a bug, just a
    /// louder one.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: KernelErrorCode::Storage,
            message: message.into(),
            exit: INTERNAL,
        }
    }

    /// The same object the kernel would have sent, so a caller parses one shape.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "code": self.code.as_str(),
            "message": self.message,
        })
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_earns_one_of_the_documented_exits() {
        // The table is exhaustive by construction — `exit_for` has no wildcard —
        // so what this asserts is that no code maps to a number outside the
        // published set, and that none of them lands on 10, which is reserved
        // for a fault in this program rather than an answer from the kernel.
        for code in KernelErrorCode::ALL {
            let exit = exit_for(*code);
            assert!(
                [USAGE, REFUSED, NOT_FOUND, UNAVAILABLE, INTEGRITY].contains(&exit),
                "{code:?} maps to an undocumented exit {exit}"
            );
        }
        assert_eq!(exit_for(KernelErrorCode::NotFound), NOT_FOUND);
        assert_eq!(exit_for(KernelErrorCode::Sealed), REFUSED);
        assert_eq!(exit_for(KernelErrorCode::BlobTombstoned), INTEGRITY);
        assert_eq!(Failure::internal("boom").exit, INTERNAL);
    }

    #[test]
    fn a_local_failure_reports_in_the_protocols_own_error_shape() {
        // A caller that can read a refusal off the wire can read this one too,
        // without a second parser for the CLI's own troubles.
        let json = Failure::usage("no such projection: tsak").to_json();
        assert_eq!(json["type"], "error");
        assert_eq!(json["code"], "validation");
        assert!(
            json["message"].as_str().expect("message").contains("tsak"),
            "{json}"
        );
    }
}
