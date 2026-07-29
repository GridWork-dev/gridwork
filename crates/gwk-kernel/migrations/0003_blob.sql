-- 0003_blob.sql — the blob spine's metadata.
--
-- The ciphertext lives on the filesystem; everything that makes it READABLE
-- lives here. That split is the whole reason rewrap and crypto-shred are cheap
-- (ADR 0003): each is one column write against this table, where a wrapped key
-- inside the container would make both a non-atomic rewrite of a file that may
-- be gigabytes long.

CREATE TABLE gwk_internal.blob (
  -- The bare 64-hex plaintext digest. The `sha256:` scheme lives in the
  -- address type, not in storage: this column is also what the sharded
  -- filesystem path is derived from, and the CHECK is what makes that
  -- derivation safe without any path sanitizing on top of it.
  digest        text PRIMARY KEY CHECK (digest ~ '^[0-9a-f]{64}$'),
  media_type    text NOT NULL,
  byte_size     bigint NOT NULL CHECK (byte_size >= 0),
  -- Nonsecret label. The KEK itself is never persisted anywhere.
  kek_id        text NOT NULL,
  wrap_nonce    bytea,
  wrapped_dek   bytea,
  created_at    timestamptz NOT NULL DEFAULT now(),
  tombstoned_at timestamptz,
  -- "Shredded" and "has no key" are the same fact stated twice, and a row
  -- carrying one without the other is a half-finished shred — either a blob
  -- that reads fine while the audit log calls it destroyed, or one that is
  -- unreadable for no recorded reason.
  CONSTRAINT blob_key_matches_tombstone
    CHECK ((wrapped_dek IS NULL) = (tombstoned_at IS NOT NULL)
           AND (wrap_nonce IS NULL) = (wrapped_dek IS NULL))
);

-- Evidence pins. One blob may be pinned by many pieces of evidence, and sweep
-- is blocked until the LAST one is released — so this is a set, not a flag.
CREATE TABLE gwk_internal.blob_pin (
  digest      text NOT NULL REFERENCES gwk_internal.blob (digest) ON DELETE CASCADE,
  evidence_id text NOT NULL,
  pinned_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (digest, evidence_id)
);

-- In-flight uploads. `written` is the authoritative offset the next chunk
-- lands at, which is what makes a retried chunk overwrite its own bytes
-- instead of appending them a second time.
CREATE TABLE gwk_internal.blob_upload (
  upload_id  text PRIMARY KEY,
  media_type text NOT NULL,
  byte_size  bigint NOT NULL CHECK (byte_size >= 0),
  written    bigint NOT NULL DEFAULT 0 CHECK (written >= 0),
  next_chunk bigint NOT NULL DEFAULT 0 CHECK (next_chunk >= 0),
  started_at timestamptz NOT NULL DEFAULT now()
);

-- The question sweep asks: is any event still pointing at this blob? Without
-- this index that predicate is a sequential scan of the entire log, once per
-- candidate blob. The expression matches the extraction exactly, and the
-- partial predicate keeps it to the few events that carry a reference at all.
CREATE INDEX event_payload_ref_digest
  ON gwk.event ((payload_ref ->> 'digest'))
  WHERE payload_ref IS NOT NULL;
