-- 0002_writer.sql — the durable writer state.
--
-- One row, holding the three things every append must agree on. They live
-- together deliberately: the append transaction takes ONE `FOR UPDATE` on this
-- row and gets the epoch to compare, the fence to check, and the sequence to
-- allocate. That row lock is also what makes sequence order EQUAL commit order
-- — it is held until commit, so no second appender can allocate in between.
-- Serializing in the database rather than in one process means a second process
-- is serialized too, which a Rust mutex could never promise.

CREATE TABLE gwk_internal.writer (
  id          integer PRIMARY KEY DEFAULT 1 CHECK (id = 1),

  -- Bumped once per boot. Every mutating transaction compares the value it
  -- booted with against this one, so a deposed process still holding a pooled
  -- connection cannot commit behind its successor's back.
  epoch       bigint NOT NULL DEFAULT 0 CHECK (epoch >= 0),

  -- The current append fence, NULL until the first grant. Once granted, an
  -- append must present exactly this token. Tokens start at 1 so that 0 can
  -- mean "none presented" in a refusal.
  fence_token numeric(20,0)
                CHECK (fence_token IS NULL
                       OR (fence_token >= 1 AND fence_token <= 18446744073709551615)),

  -- The next global sequence to hand out. Allocated under the row lock above,
  -- so it is assigned in commit order: unique and strictly increasing, and NOT
  -- gapless, because a rolled-back append is free to burn nothing at all while
  -- a crashed one may have burned a value.
  next_seq    numeric(20,0) NOT NULL DEFAULT 1
                CHECK (next_seq >= 1 AND next_seq <= 18446744073709551615)
);

INSERT INTO gwk_internal.writer (id) VALUES (1);
