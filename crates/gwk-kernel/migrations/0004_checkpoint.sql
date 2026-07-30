-- 0004_checkpoint.sql — projection snapshots.
--
-- A checkpoint is a recovery SHORTCUT, never the truth: recovery always
-- replays the suffix after one, so the worst a missing or corrupt checkpoint
-- costs is time. That is why these live in `gwk_internal` rather than in the
-- contract schema — a conforming backend owes the log and the projections, not
-- this engine's way of restarting faster.

CREATE TABLE gwk_internal.checkpoint (
  -- The sequence this snapshot includes THROUGH. Also the identity: two
  -- snapshots at the same sequence would be the same state, and a primary key
  -- here is what makes taking one twice a collision rather than a duplicate.
  through_seq     numeric(20,0) PRIMARY KEY
                    CHECK (through_seq >= 1 AND through_seq <= 18446744073709551615),
  -- bigint, not integer, for the same reason `gwk.event.schema_version` is:
  -- the contract declares this a u32, and integer tops out halfway through
  -- that range. The column has to hold every version the type can express or
  -- the ceiling is a silent one.
  schema_version  bigint NOT NULL
                    CHECK (schema_version >= 1 AND schema_version <= 4294967295),
  -- SHA-256 over the canonical records. Equal to the records blob's own
  -- address by construction — the blob's plaintext IS those bytes — and stored
  -- anyway so validating a checkpoint does not require parsing its reference.
  projection_hash text NOT NULL CHECK (projection_hash ~ '^[0-9a-f]{64}$'),
  -- The full `PayloadRef` to the encrypted blob holding the records.
  records_ref     jsonb NOT NULL,
  created_at      timestamptz NOT NULL
);

-- Recovery reads the NEWEST first and walks backwards through older valid ones,
-- so the index it wants is the primary key's, descending.
CREATE INDEX checkpoint_newest_first ON gwk_internal.checkpoint (through_seq DESC);

-- The barrier's two counters, beside the writer state the append already locks.
-- Here rather than in their own row because the append takes exactly ONE row
-- lock, and a second row to decide "is a checkpoint due" would be a second lock
-- ordering to get wrong.
ALTER TABLE gwk_internal.writer
  ADD COLUMN checkpoint_seq numeric(20,0) NOT NULL DEFAULT 0
    CHECK (checkpoint_seq >= 0 AND checkpoint_seq <= 18446744073709551615),
  -- Defaulted to the row's own creation, not left NULL: NULL would have to mean
  -- either "never, so one is overdue" or "never, so the clock has not started",
  -- and picking the first would checkpoint an empty database on its first
  -- append.
  ADD COLUMN checkpoint_at timestamptz NOT NULL DEFAULT now();
