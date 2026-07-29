//! The v1 frame codec: `[u32 big-endian body_length][u8 frame_kind][body]`.
//!
//! `body_length` includes the kind byte and excludes the four-byte prefix, so
//! the reader knows how much it is about to allocate before it allocates it.
//! Every bound is checked against the announced length, never against what
//! arrived — a peer that claims 4 GiB is refused after four bytes, not after
//! four gigabytes.
//!
//! The budget is byte-accounted per CONNECTION and per WINDOW, not per frame. A
//! peer that stays inside the frame cap forever cannot outrun its allowance,
//! which is the bound that actually protects a long-lived connection — and
//! because the allowance refills, a long-lived connection is possible at all.

use std::time::Duration;

use gwk_domain::protocol::{
    CONNECTION_BUDGET_WINDOW_SECS, FRAME_BODY_MAX_BYTES, FRAME_BODY_MIN_BYTES,
    FRAME_KIND_RESERVED_STREAM, FRAME_LENGTH_PREFIX_BYTES, FrameKind, KernelErrorCode,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

use super::WireError;

/// One connection's send and receive allowance, refilling each window.
///
/// A RATE, not a lifetime total. The thing worth bounding is a peer flooding
/// the kernel — how fast bytes arrive, and how fast the kernel is asked to
/// produce them. A running total bounds that, but it bounds every legitimate
/// use with it: a subscription is meant to run for days and a blob read moves
/// as many bytes as the blob holds, so under a total both die at a threshold
/// that says nothing about whether they misbehaved.
///
/// Exceeding the allowance therefore WAITS rather than failing. "Too fast" is
/// answered by going slower; a connection is only killed when it has done
/// something a slower version of would still be wrong. The one exception is a
/// frame larger than a whole window's allowance, which no amount of waiting
/// would ever admit — that is a misconfiguration, and it fails loudly instead
/// of hanging forever.
///
/// Both directions are charged the FULL frame — prefix, kind byte, and body —
/// because that is what the connection actually costs to move. Charging only
/// the body would let a peer spend the allowance on a million one-byte frames
/// and be told it had used a megabyte.
///
/// ponytail: a fixed window, so a peer can spend a full allowance at the end of
/// one and again at the start of the next — a 2x burst across the boundary. For
/// a flood guard that is immaterial; a token bucket if smoothing ever matters.
#[derive(Debug)]
pub struct Budget {
    ingress_per_window: usize,
    egress_per_window: usize,
    window: Duration,
    ingress_spent: usize,
    egress_spent: usize,
    window_started: Instant,
}

impl Budget {
    pub fn new(ingress: usize, egress: usize) -> Self {
        Self::with_window(
            ingress,
            egress,
            Duration::from_secs(CONNECTION_BUDGET_WINDOW_SECS),
        )
    }

    /// The same allowance over a caller-chosen window. Tests use a short one so
    /// a refill is observable; nothing in the daemon does.
    pub fn with_window(ingress: usize, egress: usize, window: Duration) -> Self {
        Self {
            ingress_per_window: ingress,
            egress_per_window: egress,
            window,
            ingress_spent: 0,
            egress_spent: 0,
            window_started: Instant::now(),
        }
    }

    async fn spend_ingress(&mut self, bytes: usize) -> Result<(), WireError> {
        self.spend(bytes, Direction::Ingress).await
    }

    async fn spend_egress(&mut self, bytes: usize) -> Result<(), WireError> {
        self.spend(bytes, Direction::Egress).await
    }

    async fn spend(&mut self, bytes: usize, direction: Direction) -> Result<(), WireError> {
        let per_window = match direction {
            Direction::Ingress => self.ingress_per_window,
            Direction::Egress => self.egress_per_window,
        };
        // Checked before any waiting: this one can never be satisfied, and
        // sleeping on it would turn a bad bound into a hung connection.
        if bytes > per_window {
            return Err(WireError::new(
                KernelErrorCode::FrameSize,
                format!(
                    "a {direction} frame of {bytes} bytes exceeds the whole \
                     {per_window}-byte window allowance"
                ),
            ));
        }
        loop {
            let elapsed = self.window_started.elapsed();
            if elapsed >= self.window {
                self.ingress_spent = 0;
                self.egress_spent = 0;
                self.window_started = Instant::now();
            }
            let spent = match direction {
                Direction::Ingress => &mut self.ingress_spent,
                Direction::Egress => &mut self.egress_spent,
            };
            if *spent + bytes <= per_window {
                *spent += bytes;
                return Ok(());
            }
            // Wait out the remainder of this window, then reconsider. One sleep
            // is always enough for a frame that fits a window at all, but the
            // loop re-reads the clock rather than assuming that.
            tokio::time::sleep(self.window - elapsed).await;
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Ingress,
    Egress,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ingress => "received",
            Self::Egress => "sent",
        })
    }
}

/// One decoded frame. The kind is already known-good; the body is raw bytes
/// that nothing has parsed yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub body: Vec<u8>,
}

/// The peer closed cleanly between frames, which is not an error.
#[derive(Debug)]
pub enum Incoming {
    Frame(Frame),
    Closed,
}

/// Read one frame, refusing an illegal length before allocating for it.
///
/// `max_body` is the caller's ceiling for THIS frame, which is how the hello's
/// tighter 64 KiB cap is applied without a second codec: the handshake passes
/// `HELLO_MAX_BYTES` and every frame after it passes `FRAME_BODY_MAX_BYTES`.
/// A value above the protocol maximum is clamped rather than trusted.
pub async fn read_frame<R>(
    reader: &mut R,
    max_body: u32,
    budget: &mut Budget,
) -> Result<Incoming, WireError>
where
    R: AsyncRead + Unpin,
{
    let ceiling = max_body.min(FRAME_BODY_MAX_BYTES);
    let mut prefix = [0u8; FRAME_LENGTH_PREFIX_BYTES];
    // The FIRST byte alone decides whether there is a frame at all. Reading the
    // whole prefix in one call cannot tell "the peer hung up between frames"
    // from "three bytes of a length arrived and then the stream ended" — both
    // are `UnexpectedEof` — and calling the second one a clean close would let a
    // truncated stream end a session silently.
    match reader.read_exact(&mut prefix[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Incoming::Closed),
        Err(e) => return Err(WireError::io("read frame length", e)),
    }
    reader
        .read_exact(&mut prefix[1..])
        .await
        .map_err(|e| WireError::io("read frame length", e))?;
    let announced = u32::from_be_bytes(prefix);

    if announced < FRAME_BODY_MIN_BYTES {
        // Zero is its own refusal rather than "a frame with no kind": a peer
        // that can announce an empty frame can hold a reader in a loop that
        // allocates nothing and never ends.
        return Err(WireError::new(
            KernelErrorCode::FrameSize,
            format!(
                "frame body_length {announced} is below the {FRAME_BODY_MIN_BYTES}-byte minimum"
            ),
        ));
    }
    if announced > ceiling {
        return Err(WireError::new(
            KernelErrorCode::FrameSize,
            format!("frame body_length {announced} exceeds the {ceiling}-byte maximum"),
        ));
    }

    let total = FRAME_LENGTH_PREFIX_BYTES + announced as usize;
    budget.spend_ingress(total).await?;

    let mut body = vec![0u8; announced as usize];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| WireError::io("read frame body", e))?;

    // The kind byte is the first body byte, so it is bounded by the same length
    // that was just checked. Splitting it off here keeps every caller from
    // re-deriving that the body starts at offset one.
    let kind_byte = body.remove(0);
    let kind = FrameKind::from_u8(kind_byte).ok_or_else(|| {
        let detail = if kind_byte == FRAME_KIND_RESERVED_STREAM {
            " (reserved for 5′ and not accepted in v1)"
        } else {
            ""
        };
        WireError::new(
            KernelErrorCode::Handshake,
            format!("unknown frame kind 0x{kind_byte:02x}{detail}"),
        )
    })?;
    Ok(Incoming::Frame(Frame { kind, body }))
}

/// Write one frame, charging the connection's egress budget for all of it.
pub async fn write_frame<W>(
    writer: &mut W,
    kind: FrameKind,
    body: &[u8],
    budget: &mut Budget,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    // The kind byte counts toward body_length, so the check is on body + 1 and
    // an off-by-one here would ship a frame no conforming reader can accept.
    let announced = u32::try_from(body.len() + 1).map_err(|_| {
        WireError::new(
            KernelErrorCode::FrameSize,
            format!("frame body {} bytes does not fit a u32 length", body.len()),
        )
    })?;
    if announced > FRAME_BODY_MAX_BYTES {
        return Err(WireError::new(
            KernelErrorCode::FrameSize,
            format!(
                "frame body_length {announced} exceeds the {FRAME_BODY_MAX_BYTES}-byte maximum"
            ),
        ));
    }
    budget
        .spend_egress(FRAME_LENGTH_PREFIX_BYTES + announced as usize)
        .await?;

    // One buffer, one write: two writes would let a reader on the other side
    // observe a length with no body behind it if this task is cancelled between
    // them.
    let mut out = Vec::with_capacity(FRAME_LENGTH_PREFIX_BYTES + announced as usize);
    out.extend_from_slice(&announced.to_be_bytes());
    out.push(kind.as_u8());
    out.extend_from_slice(body);
    writer
        .write_all(&out)
        .await
        .map_err(|e| WireError::io("write frame", e))?;
    writer
        .flush()
        .await
        .map_err(|e| WireError::io("flush frame", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Budget {
        Budget::new(1 << 20, 1 << 20)
    }

    async fn read_bytes(raw: &[u8], max_body: u32) -> Result<Incoming, WireError> {
        read_frame(
            &mut std::io::Cursor::new(raw.to_vec()),
            max_body,
            &mut budget(),
        )
        .await
    }

    #[tokio::test]
    async fn a_frame_survives_its_own_round_trip() {
        let mut wire = Vec::new();
        let mut out = budget();
        write_frame(
            &mut wire,
            FrameKind::Json,
            b"{\"type\":\"health\"}",
            &mut out,
        )
        .await
        .expect("write");
        // body_length counts the kind byte: 17 bytes of JSON plus one.
        assert_eq!(&wire[..4], &18u32.to_be_bytes());
        assert_eq!(wire[4], FrameKind::Json.as_u8());

        match read_bytes(&wire, FRAME_BODY_MAX_BYTES).await.expect("read") {
            Incoming::Frame(frame) => {
                assert_eq!(frame.kind, FrameKind::Json);
                assert_eq!(frame.body, b"{\"type\":\"health\"}");
            }
            Incoming::Closed => panic!("closed on a whole frame"),
        }
    }

    #[tokio::test]
    async fn an_illegal_length_is_refused_from_the_prefix_alone() {
        // Four bytes in, nothing allocated: the whole point of putting the
        // length first. Both cases carry ONLY a prefix — if either were being
        // decided after reading a body, these would hang instead of refusing.
        for (announced, expected) in [(0u32, "below"), (FRAME_BODY_MAX_BYTES + 1, "exceeds")] {
            let error = read_bytes(&announced.to_be_bytes(), FRAME_BODY_MAX_BYTES)
                .await
                .expect_err("illegal length accepted");
            assert_eq!(error.code, KernelErrorCode::FrameSize);
            assert!(error.message.contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn the_hello_cap_is_the_same_codec_with_a_lower_ceiling() {
        let announced = gwk_domain::protocol::HELLO_MAX_BYTES + 1;
        let error = read_bytes(
            &announced.to_be_bytes(),
            gwk_domain::protocol::HELLO_MAX_BYTES,
        )
        .await
        .expect_err("oversized hello accepted");
        assert_eq!(error.code, KernelErrorCode::FrameSize);
        // The same bytes are fine once the handshake is over.
        let mut whole = announced.to_be_bytes().to_vec();
        whole.push(FrameKind::Json.as_u8());
        whole.extend(std::iter::repeat_n(b' ', announced as usize - 1));
        assert!(matches!(
            read_bytes(&whole, FRAME_BODY_MAX_BYTES)
                .await
                .expect("read"),
            Incoming::Frame(_)
        ));
    }

    #[tokio::test]
    async fn the_reserved_5p_kind_is_refused_by_name() {
        let mut raw = 1u32.to_be_bytes().to_vec();
        raw.push(FRAME_KIND_RESERVED_STREAM);
        let error = read_bytes(&raw, FRAME_BODY_MAX_BYTES)
            .await
            .expect_err("reserved kind accepted");
        assert_eq!(error.code, KernelErrorCode::Handshake);
        assert!(error.message.contains("5′"), "{error}");
    }

    #[tokio::test]
    async fn a_clean_hangup_between_frames_is_not_an_error() {
        assert!(matches!(
            read_bytes(&[], FRAME_BODY_MAX_BYTES).await.expect("eof"),
            Incoming::Closed
        ));
        // A PARTIAL prefix is a different thing: bytes were lost.
        assert!(read_bytes(&[0, 0], FRAME_BODY_MAX_BYTES).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_outruns_its_allowance_waits_instead_of_dying() {
        // Twelve bytes a window against a frame that costs seven: the first
        // fits and the second cannot, until the allowance refills.
        let mut small = Budget::new(12, 12);
        let mut wire = Vec::new();
        let mut writing = Budget::new(1 << 20, 1 << 20);
        write_frame(&mut wire, FrameKind::Json, b"ab", &mut writing)
            .await
            .expect("write");
        let mut stream = std::io::Cursor::new([wire.clone(), wire].concat());

        let started = Instant::now();
        read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut small)
            .await
            .expect("first frame");
        // 4 prefix + 1 kind + 2 body = 7 charged, not 2, which is why twelve
        // does not cover two of them.
        assert!(
            started.elapsed() < Duration::from_millis(1),
            "the first frame should not have waited"
        );

        read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut small)
            .await
            .expect("the second frame arrives after the refill");
        // It ARRIVED. The connection was throttled, not killed, and that is the
        // whole difference between a rate and a lifetime cap — under a total
        // this peer would be finished, having done nothing but read two frames.
        assert!(
            started.elapsed() >= Duration::from_secs(CONNECTION_BUDGET_WINDOW_SECS),
            "the second frame was not made to wait for its window"
        );
    }

    #[tokio::test]
    async fn a_frame_larger_than_a_whole_window_fails_rather_than_hanging() {
        let mut sink = Vec::new();
        // 4 prefix + 1 kind + 4 body = 9 bytes against a 6-byte window. No
        // amount of waiting ever admits it, so waiting would be a hang dressed
        // as a limit.
        let mut small = Budget::new(0, 6);
        let error = write_frame(&mut sink, FrameKind::Json, b"abcd", &mut small)
            .await
            .expect_err("an unaffordable frame was not refused");
        assert_eq!(error.code, KernelErrorCode::FrameSize);
        assert!(error.message.contains("sent"), "{error}");
        // Nothing reached the writer: a frame that cannot be afforded is not
        // half-sent.
        assert!(sink.is_empty());
    }
}
