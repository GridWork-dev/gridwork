//! The v1 blob container: an authenticated header followed by length-prefixed
//! AEAD chunks (ADR 0003).
//!
//! ```text
//! magic            8   b"GWKBLOB\0"
//! format_version   2   u16 big-endian
//! digest          32   raw SHA-256 of the plaintext
//! byte_size        8   u64 big-endian, plaintext length
//! stream_nonce    19   XChaCha20-Poly1305 nonce minus the BE32 stream suffix
//! media_type_len   2   u16 big-endian
//! kek_id_len       2   u16 big-endian
//! media_type       n   UTF-8
//! kek_id           n   UTF-8
//! ------------------- everything above is the AAD -------------------
//! [chunk_len u32 be][ciphertext] ...  1 MiB plaintext per chunk
//! ```
//!
//! **The wrapped DEK is NOT in this file.** It travels beside the container, in
//! the row that describes the blob, and that placement is what makes the two
//! operations ADR 0003 requires cheap and atomic: rewrap replaces one column,
//! and crypto-shred deletes one column. Both would otherwise have to rewrite a
//! multi-gigabyte file — non-atomically — to change 72 bytes near its front.
//! The consequence is stated plainly because it is load-bearing for backups: a
//! container without its row is unreadable ciphertext, by design.
//!
//! Everything in the header is bound as associated data — into the DEK wrap AND
//! into every chunk — so flipping a byte of the declared size, media type, or
//! KEK label fails authentication rather than producing a differently-described
//! blob. The final chunk is sealed with the stream's explicit last-block flag,
//! which is what makes truncation detectable: a reader that hits EOF early
//! tries to open a middle chunk as the last one, and the tag does not verify.

use aead::array::Array;
use aead::consts::{U19, U24, U32};
use aead::{Aead, Generate, KeyInit, Payload};
use aead_stream::{DecryptorBE32, EncryptorBE32, StreamBE32};
use chacha20poly1305::XChaCha20Poly1305;
use gwk_domain::blob::BLOB_CHUNK_BYTES;
use gwk_domain::port::BlobError;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

/// Identifies a GridWork blob on sight, including in a hex dump of a stray
/// file that outlived its database.
pub const MAGIC: [u8; 8] = *b"GWKBLOB\0";

/// The only container version v1 writes or reads. A future format is a new
/// number here, never a parser that tolerates both.
pub const FORMAT_VERSION: u16 = 1;

/// Bytes of key material per blob (ADR 0003).
pub const DEK_BYTES: usize = 32;
/// XChaCha20-Poly1305 nonce width, used whole for the DEK wrap.
pub const WRAP_NONCE_BYTES: usize = 24;
/// A wrapped DEK is the key plus the Poly1305 tag.
pub const WRAPPED_DEK_BYTES: usize = DEK_BYTES + 16;
/// `StreamBE32` spends 5 of the 24 nonce bytes on its counter and last-block
/// flag, so the caller supplies the other 19.
pub const STREAM_NONCE_BYTES: usize = WRAP_NONCE_BYTES - 5;

const DIGEST_BYTES: usize = 32;
const FIXED_HEADER_BYTES: usize = MAGIC.len() + 2 + DIGEST_BYTES + 8 + STREAM_NONCE_BYTES + 2 + 2;

type Stream = StreamBE32<XChaCha20Poly1305>;
type StreamNonce = aead_stream::Nonce<XChaCha20Poly1305, Stream>;

fn integrity(reason: impl Into<String>) -> BlobError {
    BlobError::Integrity(reason.into())
}

/// The authenticated header, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Raw SHA-256 over the plaintext — the same bytes the address hexes.
    pub digest: [u8; DIGEST_BYTES],
    pub byte_size: u64,
    pub media_type: String,
    /// Nonsecret label of the KEK that wrapped this blob's DEK.
    pub kek_id: String,
    pub stream_nonce: [u8; STREAM_NONCE_BYTES],
}

impl Header {
    /// The exact bytes that open the container AND serve as associated data.
    ///
    /// One function for both, deliberately: if the AAD were assembled
    /// separately from the bytes on disk, the two could disagree and the
    /// authentication would be checking something the reader never parsed.
    pub fn encode(&self) -> Vec<u8> {
        let media = self.media_type.as_bytes();
        let kek = self.kek_id.as_bytes();
        let mut out = Vec::with_capacity(FIXED_HEADER_BYTES + media.len() + kek.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&self.byte_size.to_be_bytes());
        out.extend_from_slice(&self.stream_nonce);
        // Lengths are u16 and both fields are bounded well under it by
        // `seal_with`, so the casts cannot truncate.
        out.extend_from_slice(&(media.len() as u16).to_be_bytes());
        out.extend_from_slice(&(kek.len() as u16).to_be_bytes());
        out.extend_from_slice(media);
        out.extend_from_slice(kek);
        out
    }

    /// Parse a header, returning it and how many bytes it occupied.
    ///
    /// Strict: unknown magic, an unknown version, a length that runs past the
    /// buffer, or non-UTF-8 in either string is a refusal. Nothing here is
    /// recovered from or guessed at — the header is the thing every other byte
    /// is authenticated against.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), BlobError> {
        if bytes.len() < FIXED_HEADER_BYTES {
            return Err(integrity("container is shorter than a header"));
        }
        // Fixed offsets, named once. Every slice below sits inside the length
        // checked above, and spelling the boundaries out means the layout in
        // the module docs and the parser can be read against each other.
        const VERSION_AT: usize = MAGIC.len();
        const DIGEST_AT: usize = VERSION_AT + 2;
        const SIZE_AT: usize = DIGEST_AT + DIGEST_BYTES;
        const NONCE_AT: usize = SIZE_AT + 8;
        const MEDIA_LEN_AT: usize = NONCE_AT + STREAM_NONCE_BYTES;
        const KEK_LEN_AT: usize = MEDIA_LEN_AT + 2;

        if bytes[..MAGIC.len()] != MAGIC {
            return Err(integrity("not a gwk blob container"));
        }
        let version = u16::from_be_bytes(
            bytes[VERSION_AT..DIGEST_AT]
                .try_into()
                .map_err(|_| integrity("version"))?,
        );
        if version != FORMAT_VERSION {
            return Err(integrity(format!(
                "container format version {version}, expected {FORMAT_VERSION}"
            )));
        }
        let digest: [u8; DIGEST_BYTES] = bytes[DIGEST_AT..SIZE_AT]
            .try_into()
            .map_err(|_| integrity("digest"))?;
        let byte_size = u64::from_be_bytes(
            bytes[SIZE_AT..NONCE_AT]
                .try_into()
                .map_err(|_| integrity("size"))?,
        );
        let stream_nonce: [u8; STREAM_NONCE_BYTES] = bytes[NONCE_AT..MEDIA_LEN_AT]
            .try_into()
            .map_err(|_| integrity("nonce"))?;
        let media_len = usize::from(u16::from_be_bytes(
            bytes[MEDIA_LEN_AT..KEK_LEN_AT]
                .try_into()
                .map_err(|_| integrity("len"))?,
        ));
        let kek_len = usize::from(u16::from_be_bytes(
            bytes[KEK_LEN_AT..FIXED_HEADER_BYTES]
                .try_into()
                .map_err(|_| integrity("len"))?,
        ));
        let mut at = FIXED_HEADER_BYTES;
        if bytes.len() < at + media_len + kek_len {
            return Err(integrity("header runs past the container"));
        }
        let media_type = std::str::from_utf8(&bytes[at..at + media_len])
            .map_err(|_| integrity("media type is not utf-8"))?
            .to_owned();
        at += media_len;
        let kek_id = std::str::from_utf8(&bytes[at..at + kek_len])
            .map_err(|_| integrity("kek id is not utf-8"))?
            .to_owned();
        at += kek_len;
        Ok((
            Self {
                digest,
                byte_size,
                media_type,
                kek_id,
                stream_nonce,
            },
            at,
        ))
    }
}

/// A sealed blob: the container bytes, plus the key material that belongs in
/// the database rather than the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub container: Vec<u8>,
    pub wrap_nonce: [u8; WRAP_NONCE_BYTES],
    pub wrapped_dek: [u8; WRAPPED_DEK_BYTES],
    /// Lowercase 64-hex of the plaintext digest — the blob's address.
    pub digest_hex: String,
}

/// Seal `plaintext` with caller-supplied key material.
///
/// Every random input is a parameter so the format has exact golden vectors: a
/// function that mints its own DEK cannot be pinned by a test, and an
/// unpinnable byte format silently drifts. [`seal`] is the thin wrapper that
/// generates them for real use.
pub fn seal_with(
    plaintext: &[u8],
    media_type: &str,
    kek: &[u8; DEK_BYTES],
    kek_id: &str,
    dek: &[u8; DEK_BYTES],
    wrap_nonce: &[u8; WRAP_NONCE_BYTES],
    stream_nonce: &[u8; STREAM_NONCE_BYTES],
) -> Result<Sealed, BlobError> {
    if media_type.len() > u16::MAX as usize || kek_id.len() > u16::MAX as usize {
        return Err(integrity("media type or kek id is too long to encode"));
    }
    let digest: [u8; DIGEST_BYTES] = Sha256::digest(plaintext).into();
    let header = Header {
        digest,
        byte_size: plaintext.len() as u64,
        media_type: media_type.to_owned(),
        kek_id: kek_id.to_owned(),
        stream_nonce: *stream_nonce,
    };
    let aad = header.encode();

    let wrapped_dek = wrap_dek(dek, kek, wrap_nonce, &aad)?;

    let mut container = aad.clone();
    let mut encryptor =
        EncryptorBE32::<XChaCha20Poly1305>::new(&Array(*dek), &StreamNonce::from(*stream_nonce));

    // An empty blob still gets one sealed chunk: zero chunks would make an
    // empty blob and a truncated one the same bytes on disk.
    let mut chunks: Vec<&[u8]> = plaintext.chunks(BLOB_CHUNK_BYTES).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    // Split before the loop because `encrypt_last` consumes the encryptor —
    // which is the type system enforcing that a stream has exactly one final
    // chunk and nothing follows it.
    let Some((last, rest)) = chunks.split_last() else {
        return Err(integrity("no chunk to seal"));
    };
    for chunk in rest {
        let sealed = encryptor
            .encrypt_next(Payload {
                msg: chunk,
                aad: &aad,
            })
            .map_err(|_| integrity("chunk encryption failed"))?;
        push_chunk(&mut container, &sealed)?;
    }
    let sealed = encryptor
        .encrypt_last(Payload {
            msg: last,
            aad: &aad,
        })
        .map_err(|_| integrity("final chunk encryption failed"))?;
    push_chunk(&mut container, &sealed)?;

    Ok(Sealed {
        container,
        wrap_nonce: *wrap_nonce,
        wrapped_dek,
        digest_hex: hex_lower(&digest),
    })
}

/// Seal `plaintext`, minting a fresh DEK and nonces from the system CSPRNG.
pub fn seal(
    plaintext: &[u8],
    media_type: &str,
    kek: &[u8; DEK_BYTES],
    kek_id: &str,
) -> Result<Sealed, BlobError> {
    let mut dek: Array<u8, U32> = generate()?;
    let wrap_nonce: Array<u8, U24> = generate()?;
    let stream_nonce: Array<u8, U19> = generate()?;
    let sealed = seal_with(
        plaintext,
        media_type,
        kek,
        kek_id,
        &dek.0,
        &wrap_nonce.0,
        &stream_nonce.0,
    );
    // The wrapped copy is the only one that should outlive this call.
    dek.zeroize();
    sealed
}

/// Open a container, given the key material stored beside it.
///
/// Returns the plaintext only if the header authenticates, every chunk
/// authenticates in order, the final chunk was sealed as final, and the
/// plaintext hashes to the digest the header claims.
pub fn open(
    container: &[u8],
    kek: &[u8; DEK_BYTES],
    wrap_nonce: &[u8; WRAP_NONCE_BYTES],
    wrapped_dek: &[u8; WRAPPED_DEK_BYTES],
) -> Result<(Header, Vec<u8>), BlobError> {
    let (header, header_len) = Header::decode(container)?;
    let aad = &container[..header_len];
    let mut dek = unwrap_dek(wrapped_dek, kek, wrap_nonce, aad)?;

    let mut decryptor = DecryptorBE32::<XChaCha20Poly1305>::new(
        &Array(dek),
        &StreamNonce::from(header.stream_nonce),
    );
    dek.zeroize();

    // Frame the whole container before decrypting any of it: the framing is
    // unauthenticated, so a bad length must be a structural refusal rather
    // than something discovered halfway through a stream.
    let mut framed: Vec<&[u8]> = Vec::new();
    let mut at = header_len;
    while at < container.len() {
        let (chunk, next) = read_chunk(container, at)?;
        framed.push(chunk);
        at = next;
    }
    let Some((last, rest)) = framed.split_last() else {
        return Err(integrity("container has no chunks"));
    };

    let mut plaintext = Vec::with_capacity(header.byte_size as usize);
    for chunk in rest {
        let part = decryptor
            .decrypt_next(Payload { msg: chunk, aad })
            .map_err(|_| integrity("chunk failed authentication"))?;
        plaintext.extend_from_slice(&part);
    }
    // Sealed with the stream's last-block flag, so a truncated container
    // arrives here holding what was a MIDDLE chunk and the tag does not
    // verify — which is exactly how truncation is caught.
    let part = decryptor
        .decrypt_last(Payload { msg: last, aad })
        .map_err(|_| integrity("final chunk failed authentication: tampered or truncated"))?;
    plaintext.extend_from_slice(&part);

    if plaintext.len() as u64 != header.byte_size {
        return Err(integrity(format!(
            "container holds {} plaintext bytes, header declares {}",
            plaintext.len(),
            header.byte_size
        )));
    }
    let actual: [u8; DIGEST_BYTES] = Sha256::digest(&plaintext).into();
    if actual != header.digest {
        return Err(integrity("plaintext does not hash to the declared digest"));
    }
    Ok((header, plaintext))
}

/// Rewrap this blob's DEK under a new KEK without touching its ciphertext.
///
/// The AAD is the container's own header bytes, which do not change — the KEK
/// LABEL lives in the header, so a rewrap under a differently-labeled key would
/// invalidate it. Rotation therefore keeps the label and changes the key behind
/// it, which is what "rewrap changes only the wrapped DEK" means in practice.
pub fn rewrap(
    container: &[u8],
    old_kek: &[u8; DEK_BYTES],
    new_kek: &[u8; DEK_BYTES],
    wrap_nonce: &[u8; WRAP_NONCE_BYTES],
    wrapped_dek: &[u8; WRAPPED_DEK_BYTES],
    new_wrap_nonce: &[u8; WRAP_NONCE_BYTES],
) -> Result<[u8; WRAPPED_DEK_BYTES], BlobError> {
    let (_, header_len) = Header::decode(container)?;
    let aad = &container[..header_len];
    let mut dek = unwrap_dek(wrapped_dek, old_kek, wrap_nonce, aad)?;
    let rewrapped = wrap_dek(&dek, new_kek, new_wrap_nonce, aad);
    dek.zeroize();
    rewrapped
}

fn wrap_dek(
    dek: &[u8; DEK_BYTES],
    kek: &[u8; DEK_BYTES],
    nonce: &[u8; WRAP_NONCE_BYTES],
    aad: &[u8],
) -> Result<[u8; WRAPPED_DEK_BYTES], BlobError> {
    let sealed = XChaCha20Poly1305::new(&Array(*kek))
        .encrypt(&Array(*nonce), Payload { msg: dek, aad })
        .map_err(|_| integrity("wrapping the data key failed"))?;
    sealed
        .try_into()
        .map_err(|_| integrity("wrapped key is the wrong length"))
}

fn unwrap_dek(
    wrapped: &[u8; WRAPPED_DEK_BYTES],
    kek: &[u8; DEK_BYTES],
    nonce: &[u8; WRAP_NONCE_BYTES],
    aad: &[u8],
) -> Result<[u8; DEK_BYTES], BlobError> {
    let dek = XChaCha20Poly1305::new(&Array(*kek))
        .decrypt(
            &Array(*nonce),
            Payload {
                msg: wrapped.as_slice(),
                aad,
            },
        )
        // Deliberately not distinguished from a tampered header: the same
        // failure covers "wrong KEK" and "header edited", and telling them
        // apart tells an attacker which half of the guess was right.
        .map_err(|_| integrity("unwrapping the data key failed: wrong kek or altered header"))?;
    dek.try_into()
        .map_err(|_| integrity("unwrapped key is the wrong length"))
}

fn push_chunk(out: &mut Vec<u8>, chunk: &[u8]) -> Result<(), BlobError> {
    let len = u32::try_from(chunk.len()).map_err(|_| integrity("chunk is too large to frame"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(chunk);
    Ok(())
}

fn read_chunk(container: &[u8], at: usize) -> Result<(&[u8], usize), BlobError> {
    let end = at + 4;
    if container.len() < end {
        return Err(integrity("truncated chunk length"));
    }
    let len = u32::from_be_bytes(
        container[at..end]
            .try_into()
            .map_err(|_| integrity("chunk length"))?,
    ) as usize;
    let stop = end
        .checked_add(len)
        .ok_or_else(|| integrity("chunk length overflows the container"))?;
    if container.len() < stop {
        return Err(integrity("chunk runs past the container"));
    }
    Ok((&container[end..stop], stop))
}

/// Fresh bytes from the system CSPRNG, sized by the type at the call site.
///
/// A failure here is a Storage error rather than an integrity one: the machine
/// could not produce randomness, which says nothing about any blob.
fn generate<N: aead::array::ArraySize>() -> Result<Array<u8, N>, BlobError> {
    Array::try_generate()
        .map_err(|e| BlobError::Storage(format!("system randomness unavailable: {e}")))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEK: [u8; DEK_BYTES] = [0x11; DEK_BYTES];
    const DEK: [u8; DEK_BYTES] = [0x22; DEK_BYTES];
    const WRAP_NONCE: [u8; WRAP_NONCE_BYTES] = [0x33; WRAP_NONCE_BYTES];
    const STREAM_NONCE: [u8; STREAM_NONCE_BYTES] = [0x44; STREAM_NONCE_BYTES];

    fn seal_fixture(plaintext: &[u8]) -> Sealed {
        seal_with(
            plaintext,
            "application/json",
            &KEK,
            "kek-test",
            &DEK,
            &WRAP_NONCE,
            &STREAM_NONCE,
        )
        .expect("seal")
    }

    #[test]
    fn a_sealed_blob_opens_to_exactly_what_went_in() {
        for plaintext in [
            b"".to_vec(),
            b"one chunk".to_vec(),
            vec![0xab; BLOB_CHUNK_BYTES],         // exactly one chunk
            vec![0xcd; BLOB_CHUNK_BYTES + 1],     // spills to a second
            vec![0xef; BLOB_CHUNK_BYTES * 2 + 7], // three, last one short
        ] {
            let sealed = seal_fixture(&plaintext);
            let (header, opened) = open(
                &sealed.container,
                &KEK,
                &sealed.wrap_nonce,
                &sealed.wrapped_dek,
            )
            .expect("open");
            assert_eq!(opened, plaintext);
            assert_eq!(header.byte_size, plaintext.len() as u64);
            assert_eq!(header.media_type, "application/json");
            assert_eq!(header.kek_id, "kek-test");
        }
    }

    /// The whole container for `b"gridwork"`, byte for byte.
    ///
    /// Every structural field was verified BY HAND against the layout in the
    /// module docs, and the plaintext digest against `sha256sum` and `openssl
    /// dgst` independently of this code. The 24 trailing AEAD bytes are pinned
    /// by observation — the honest scope of a golden when this crate is the
    /// only implementation of the format.
    ///
    /// ```text
    /// 47574b424c4f4200  magic "GWKBLOB\0"
    /// 0001              format version 1
    /// 43e2..5d1d        sha256("gridwork"), independently verified
    /// 0000000000000008  byte_size 8
    /// 44 x19            stream nonce
    /// 0010 0008         media_type_len 16, kek_id_len 8
    /// 6170..6f6e        "application/json"
    /// 6b65..7374        "kek-test"
    /// 00000018          chunk length 24 = 8 plaintext + 16 tag
    /// 658b..5722        ciphertext and tag
    /// ```
    #[test]
    fn the_format_is_pinned_byte_for_byte() {
        // A golden, not a round trip: a round trip still passes when both
        // halves drift together, which is exactly how a byte format breaks
        // silently between releases.
        let sealed = seal_fixture(b"gridwork");
        let hex: String = sealed
            .container
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "47574b424c4f4200000143e27ffff32e033107110d7303fa2fcf4eb11ee1cfe54f5017f9a653\
             084c5d1d00000000000000084444444444444444444444444444444444444400100008617070\
             6c69636174696f6e2f6a736f6e6b656b2d7465737400000018658bce0c2761f96df661755b52\
             019f8186b90db3313e5722"
                .replace([' ', '\n'], "")
        );
        // The digest is the address, and it is the one field a second
        // implementation must agree on byte for byte.
        assert_eq!(
            sealed.digest_hex,
            "43e27ffff32e033107110d7303fa2fcf4eb11ee1cfe54f5017f9a653084c5d1d"
        );
        // 97 header (8+2+32+8+19+2+2+16+8) + 4 length + 8 plaintext + 16 tag.
        assert_eq!(sealed.container.len(), 125);
        assert_eq!(sealed.wrapped_dek.len(), WRAPPED_DEK_BYTES);
    }

    #[test]
    fn the_header_is_associated_data_so_editing_it_fails_the_open() {
        let sealed = seal_fixture(b"payload");
        // Every field, one at a time. The offsets follow the layout in the
        // module docs; a change there should break this test loudly.
        for (label, at) in [
            ("version", 9),
            ("digest", 12),
            ("byte_size", 44),
            ("stream_nonce", 52),
            ("media_type", 79),
            ("kek_id", 92),
        ] {
            let mut tampered = sealed.container.clone();
            tampered[at] ^= 0xff;
            let err =
                open(&tampered, &KEK, &sealed.wrap_nonce, &sealed.wrapped_dek).expect_err(label);
            assert!(matches!(err, BlobError::Integrity(_)), "{label}: {err:?}");
        }
    }

    #[test]
    fn a_flipped_ciphertext_byte_fails_authentication() {
        let sealed = seal_fixture(b"payload");
        let mut tampered = sealed.container.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(matches!(
            open(&tampered, &KEK, &sealed.wrap_nonce, &sealed.wrapped_dek),
            Err(BlobError::Integrity(_))
        ));
    }

    #[test]
    fn truncation_is_detected_because_the_last_chunk_is_sealed_as_last() {
        // Two chunks, then drop the second entirely. The remaining chunk is a
        // structurally valid container whose only flaw is that its final chunk
        // was encrypted as a middle one.
        let plaintext = vec![0x5a; BLOB_CHUNK_BYTES + 32];
        let sealed = seal_fixture(&plaintext);
        let (_, header_len) = Header::decode(&sealed.container).expect("header");
        let (first, after_first) = read_chunk(&sealed.container, header_len).expect("chunk");
        assert!(after_first < sealed.container.len(), "expected two chunks");

        let mut truncated = sealed.container[..header_len].to_vec();
        push_chunk(&mut truncated, first).expect("frame");
        let err = open(&truncated, &KEK, &sealed.wrap_nonce, &sealed.wrapped_dek)
            .expect_err("truncated container");
        assert!(matches!(err, BlobError::Integrity(_)), "{err:?}");
    }

    #[test]
    fn the_wrong_kek_cannot_open_it_and_says_nothing_about_why() {
        let sealed = seal_fixture(b"secret");
        let wrong = [0x99; DEK_BYTES];
        let err = open(
            &sealed.container,
            &wrong,
            &sealed.wrap_nonce,
            &sealed.wrapped_dek,
        )
        .expect_err("wrong kek");
        let BlobError::Integrity(message) = err else {
            panic!("expected an integrity failure");
        };
        assert!(message.contains("wrong kek or altered header"), "{message}");
    }

    #[test]
    fn rewrapping_changes_only_the_key_not_the_ciphertext() {
        let sealed = seal_fixture(b"rotate me");
        let new_kek = [0x77; DEK_BYTES];
        let new_nonce = [0x88; WRAP_NONCE_BYTES];
        let rewrapped = rewrap(
            &sealed.container,
            &KEK,
            &new_kek,
            &sealed.wrap_nonce,
            &sealed.wrapped_dek,
            &new_nonce,
        )
        .expect("rewrap");

        assert_ne!(rewrapped, sealed.wrapped_dek);
        // The container is untouched, which is the whole point: rotation must
        // not rewrite blob bytes.
        let (_, opened) =
            open(&sealed.container, &new_kek, &new_nonce, &rewrapped).expect("open rewrapped");
        assert_eq!(opened, b"rotate me");
        // And the old KEK no longer opens it.
        assert!(open(&sealed.container, &KEK, &new_nonce, &rewrapped).is_err());
    }

    #[test]
    fn a_generated_seal_uses_fresh_material_every_time() {
        let a = seal(b"same bytes", "text/plain", &KEK, "kek-test").expect("seal");
        let b = seal(b"same bytes", "text/plain", &KEK, "kek-test").expect("seal");
        // Same plaintext, same address — that is what content addressing means.
        assert_eq!(a.digest_hex, b.digest_hex);
        // Different ciphertext, because nonce reuse under one key is the one
        // mistake this construction cannot survive.
        assert_ne!(a.container, b.container);
        assert_ne!(a.wrap_nonce, b.wrap_nonce);
        let (_, opened) = open(&a.container, &KEK, &a.wrap_nonce, &a.wrapped_dek).expect("open");
        assert_eq!(opened, b"same bytes");
    }

    #[test]
    fn a_container_that_is_not_one_is_refused_before_any_crypto() {
        for (label, bytes) in [
            ("empty", Vec::new()),
            ("short", vec![0u8; 10]),
            ("bad magic", {
                let mut v = seal_fixture(b"x").container;
                v[0] = b'X';
                v
            }),
            ("future version", {
                let mut v = seal_fixture(b"x").container;
                v[9] = 0x02;
                v
            }),
        ] {
            assert!(
                matches!(Header::decode(&bytes), Err(BlobError::Integrity(_))),
                "{label} decoded"
            );
        }
    }
}
