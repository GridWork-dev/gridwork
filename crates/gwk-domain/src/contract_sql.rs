// The backend-neutral contract DDL, embedded from schema/0001_contract.sql
// by `cargo run -p xtask -- contract`.
// DO NOT EDIT — regenerate instead; CI diffs this file against the source.

/// Every byte of `schema/0001_contract.sql`. The script wraps itself in
/// `BEGIN`/`COMMIT`, so it is applied whole and never inside another
/// transaction.
pub const CONTRACT_SQL: &str = r#"-- 0001_contract.sql — the gwk contract schema, greenfield.
--
-- This file is CONTRACT: shapes, state sets, legal edges, and invariants any
-- conforming storage backend must uphold. Engine-specific mechanics (queues,
-- notification channels, lock strategies, privilege hardening) belong to the
-- backend/deployment layer and are deliberately absent here.
--
-- Conventions:
--   * every identifier snake_case; ids are opaque text
--   * state values are exactly the wire strings of gwk-domain's FSM enums
--   * u64 counters (seq, fence_token, byte_size) are numeric(20,0) IN STORAGE,
--     CHECK-bounded to [0, 2^64-1] (18446744073709551615) — numeric(20,0)
--     alone admits ±(10^20-1), overshooting u64 in both directions. A signed
--     bigint tops out at 2^63-1, half the u64 wire range; the decimal-string
--     rule is a WIRE-format rule (JSON), not a storage rule
--   * u32 version counters (aggregate_version, each state row's version) are
--     bigint IN STORAGE, CHECK-bounded to [1, 2^32-1] (4294967295) — a signed
--     integer tops out at 2^31-1, half the u32 wire range
--   * timestamps are timestamptz

BEGIN;

CREATE SCHEMA IF NOT EXISTS gwk;

-- ============================================================
-- The event log
-- ============================================================

-- seq is ASSIGNED BY THE KERNEL APPEND ACTOR, in COMMIT order, and is the
-- global read/replay cursor: unique, strictly increasing, NOT gapless.
--
-- Deliberately NOT BIGSERIAL/IDENTITY: sequence numbers allocate at INSERT
-- time but transactions COMMIT in a different order, so an allocation-ordered
-- column is not proof of commit order — a reader paging by it can observe
-- seq N+1 before an uncommitted seq N exists, then never see N. A single
-- append actor assigning seq at commit closes that hole by construction.
CREATE TABLE gwk.event (
  seq               numeric(20,0) PRIMARY KEY
                      CHECK (seq >= 0 AND seq <= 18446744073709551615),
  event_id          text NOT NULL UNIQUE,
  project_id        text NOT NULL,
  aggregate_type    text NOT NULL,
  aggregate_id      text NOT NULL,
  aggregate_version bigint NOT NULL CHECK (aggregate_version BETWEEN 1 AND 4294967295),
  event_type        text NOT NULL,
  -- bigint, not integer: the envelope declares `schema_version: u32`, and a
  -- signed integer tops out at 2^31-1 — half that range. The narrow column made
  -- the contract claim a width the storage could not hold, on the one field a
  -- future decoder dispatches on. Same treatment as aggregate_version above.
  schema_version    bigint NOT NULL CHECK (schema_version BETWEEN 1 AND 4294967295),
  occurred_at       timestamptz NOT NULL,
  appended_at       timestamptz NOT NULL,
  actor             jsonb NOT NULL,
  origin            jsonb NOT NULL,
  causation_id      text,
  correlation_id    text,
  idempotency_key   text,
  payload           jsonb NOT NULL,
  payload_ref       jsonb,
  -- one event per aggregate version: the contiguity + CAS anchor
  UNIQUE (aggregate_type, aggregate_id, aggregate_version)
);

-- retry-stable appends: the same keyed write cannot land twice per aggregate.
-- This is the index the REPLAY lookup reads — a retry of the same command on
-- the same aggregate is answered from here with the original events.
CREATE UNIQUE INDEX event_idempotency
  ON gwk.event (aggregate_type, aggregate_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;

-- Idempotency is GLOBAL per (project_id, idempotency_key), not per aggregate:
-- one key names one request, and reusing it for a different one is a caller bug
-- that must be refused rather than silently landed twice. The per-aggregate
-- index above cannot see that case — the two writes differ in aggregate_id, so
-- it admits both. This is the guard; the kernel's own pre-check turns the
-- violation into a typed `idempotency_conflict` before it reaches the index.
CREATE UNIQUE INDEX event_idempotency_project
  ON gwk.event (project_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;

-- Append-only is a CONTRACT property, so it is enforced here as a trigger any
-- backend inherits. Privilege-level enforcement (revoking UPDATE/DELETE from
-- runtime roles) is a deployment MECHANISM layered on top by the kernel's
-- provisioning, not part of the contract.
CREATE FUNCTION gwk.forbid_event_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk.event is append-only (% refused)', TG_OP;
END $$;

CREATE TRIGGER event_append_only
  BEFORE UPDATE OR DELETE ON gwk.event
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_event_mutation();

-- Row-level triggers never fire on TRUNCATE, so append-only needs a
-- statement-level trigger too — without it one statement erases the log.
CREATE TRIGGER event_no_truncate
  BEFORE TRUNCATE ON gwk.event
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_event_mutation();

-- ENABLE ALWAYS on every guard trigger in this file: ordinary triggers are
-- suppressed under SET session_replication_role = 'replica', which would
-- otherwise switch every invariant off in one statement.
ALTER TABLE gwk.event ENABLE ALWAYS TRIGGER event_append_only;
ALTER TABLE gwk.event ENABLE ALWAYS TRIGGER event_no_truncate;

-- ============================================================
-- FSM edge data + the transition guard
-- ============================================================

CREATE TABLE gwk.transition (
  entity     text NOT NULL,
  from_state text NOT NULL,
  to_state   text NOT NULL,
  PRIMARY KEY (entity, from_state, to_state)
);

INSERT INTO gwk.transition (entity, from_state, to_state) VALUES
  ('task', 'submitted', 'working'),
  ('task', 'submitted', 'canceled'),
  ('task', 'working', 'input_required'),
  ('task', 'working', 'completed'),
  ('task', 'working', 'failed'),
  ('task', 'working', 'canceled'),
  ('task', 'input_required', 'working'),
  ('task', 'input_required', 'canceled'),

  ('attempt', 'queued', 'leased'),
  ('attempt', 'queued', 'canceled'),
  ('attempt', 'leased', 'starting'),
  ('attempt', 'leased', 'canceled'),
  ('attempt', 'starting', 'running'),
  ('attempt', 'starting', 'failed'),
  ('attempt', 'starting', 'unknown'),
  ('attempt', 'starting', 'canceling'),
  ('attempt', 'running', 'blocked'),
  ('attempt', 'running', 'canceling'),
  ('attempt', 'running', 'failed'),
  ('attempt', 'running', 'unknown'),
  ('attempt', 'running', 'succeeded'),
  ('attempt', 'blocked', 'running'),
  ('attempt', 'blocked', 'canceling'),
  ('attempt', 'blocked', 'failed'),
  ('attempt', 'blocked', 'unknown'),
  ('attempt', 'canceling', 'canceled'),
  ('attempt', 'canceling', 'unknown'),

  ('message', 'accepted', 'delivered'),
  ('message', 'accepted', 'dead_letter'),
  ('message', 'delivered', 'acknowledged'),
  ('message', 'delivered', 'dead_letter'),
  ('message', 'acknowledged', 'applied'),
  ('message', 'acknowledged', 'rejected'),
  ('message', 'acknowledged', 'dead_letter'),

  ('command', 'issued', 'targeted'),
  ('command', 'targeted', 'signaled'),
  ('command', 'signaled', 'verification_complete');

-- The edge table is the data the transition guard trusts: a plain INSERT here
-- would legalise a previously forbidden edge for every subsequent transition.
-- Seeded above, then frozen — these triggers refuse every later write.
CREATE FUNCTION gwk.forbid_transition_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk.transition is immutable seed data (% refused)', TG_OP;
END $$;

CREATE TRIGGER transition_immutable
  BEFORE INSERT OR UPDATE OR DELETE ON gwk.transition
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_transition_mutation();

CREATE TRIGGER transition_no_truncate
  BEFORE TRUNCATE ON gwk.transition
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_transition_mutation();

ALTER TABLE gwk.transition ENABLE ALWAYS TRIGGER transition_immutable;
ALTER TABLE gwk.transition ENABLE ALWAYS TRIGGER transition_no_truncate;

-- The row-level guard behind every FSM table: edge legality against
-- gwk.transition, and CAS discipline — EVERY update advances version by
-- exactly 1 (terminal immutability follows from terminals having no edges).
-- NOT enforced here: the liveness-producer flip rule (which actor may drive
-- running <-> blocked) lives in the domain crate's transition apply path —
-- every writer must route through it.
CREATE FUNCTION gwk.assert_transition() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.state IS DISTINCT FROM OLD.state
     AND NOT EXISTS (
       SELECT 1 FROM gwk.transition t
       WHERE t.entity = TG_ARGV[0]
         AND t.from_state = OLD.state
         AND t.to_state = NEW.state
     ) THEN
    RAISE EXCEPTION 'illegal % transition: % -> %', TG_ARGV[0], OLD.state, NEW.state;
  END IF;
  IF NEW.version IS DISTINCT FROM OLD.version + 1 THEN
    RAISE EXCEPTION '% version must advance by exactly 1 (got % -> %)',
      TG_ARGV[0], OLD.version, NEW.version;
  END IF;
  RETURN NEW;
END $$;

-- Companion guards on every FSM table. A row must be BORN at its machine's
-- initial state at version 1: a row inserted mid-spine — or born terminal,
-- e.g. a command inserted directly at verification_complete — satisfies every
-- CHECK yet skips the whole edge guard. And rows are never deleted:
-- DELETE-then-INSERT would walk around the edge table the same way.
CREATE FUNCTION gwk.assert_initial_state() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.state IS DISTINCT FROM TG_ARGV[1] OR NEW.version IS DISTINCT FROM 1 THEN
    RAISE EXCEPTION '% rows must be born in state % at version 1 (got %, version %)',
      TG_ARGV[0], TG_ARGV[1], NEW.state, NEW.version;
  END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION gwk.forbid_state_row_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% rows cannot be deleted', TG_ARGV[0];
END $$;

-- ============================================================
-- Vertical-slice entity tables
-- ============================================================

CREATE TABLE gwk.lease (
  id           text PRIMARY KEY,
  version      bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state        text NOT NULL DEFAULT 'held'
                 CHECK (state IN ('held', 'released', 'expired')),
  mode         text NOT NULL DEFAULT 'exclusive'
                 CHECK (mode IN ('exclusive', 'shared')),
  holder       text,
  scope        text,
  repo         text,
  path         text,
  branch       text,
  base_sha     text,
  fence_token  numeric(20,0)
                 CHECK (fence_token >= 0 AND fence_token <= 18446744073709551615),
  heartbeat_at timestamptz,
  expires_at   timestamptz,
  dirty        boolean NOT NULL DEFAULT false,
  unpushed     boolean NOT NULL DEFAULT false,
  disposition  text,
  created_at   timestamptz NOT NULL DEFAULT now(),
  updated_at   timestamptz NOT NULL DEFAULT now()
);

-- gwk.lease carries the fence token — the deposed-writer defense: a lease taken
-- over hands the new holder a strictly higher fence, and any write stamped with
-- a stale (lower) fence must be rejected downstream. A fence that could rewind
-- would re-arm a deposed writer, so it is monotonic non-decreasing here and,
-- once set, never cleared; and,
-- as on the FSM tables, every UPDATE is a versioned CAS: version advances by
-- exactly 1. This binds ALL lease writes — a heartbeat touch or a time-driven
-- expiry write bumps version too. It is a write-discipline invariant (optimistic
-- concurrency), NOT a state-transition rule, so it does not contradict the next
-- line: the lease is NOT one of the four public FSMs (fsm.rs: expiry is
-- time-driven, release holder-driven), so it gets no edge guard, and which
-- held/released/expired edges are legal — expiry vs release semantics — is a
-- backend concern deliberately not modeled here. This contract enforces exactly
-- two things on the lease: fence monotonicity (above) and the version CAS.
CREATE FUNCTION gwk.assert_lease_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.version IS DISTINCT FROM OLD.version + 1 THEN
    RAISE EXCEPTION 'lease version must advance by exactly 1 (got % -> %)',
      OLD.version, NEW.version;
  END IF;
  -- A fence, once set, is monotonic non-decreasing AND may never be cleared.
  -- Guarding only `NEW < OLD` left a two-step launder: 100 -> NULL (NEW null,
  -- skips the check) then NULL -> 50 (OLD null, skips it) rewinds the fence and
  -- re-arms a deposed writer. Refuse clearing or lowering a set fence; a fresh
  -- fence (OLD null -> NEW value) is still allowed.
  IF OLD.fence_token IS NOT NULL
     AND (NEW.fence_token IS NULL OR NEW.fence_token < OLD.fence_token) THEN
    RAISE EXCEPTION 'lease fence_token must not decrease or be cleared (got % -> %)',
      OLD.fence_token, NEW.fence_token;
  END IF;
  RETURN NEW;
END $$;

CREATE TRIGGER lease_guard
  BEFORE UPDATE ON gwk.lease
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_lease_guard();

ALTER TABLE gwk.lease ENABLE ALWAYS TRIGGER lease_guard;

CREATE TABLE gwk.task (
  id          text PRIMARY KEY,
  version     bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state       text NOT NULL DEFAULT 'submitted'
                CHECK (state IN ('submitted', 'working', 'input_required',
                                 'completed', 'failed', 'canceled')),
  kind        text,
  title       text,
  spec_ref    text,
  project     text,
  priority    integer,
  tracker_ref text,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER task_transition
  BEFORE UPDATE ON gwk.task
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_transition('task');

CREATE TRIGGER task_born_initial
  BEFORE INSERT ON gwk.task
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_initial_state('task', 'submitted');

CREATE TRIGGER task_no_delete
  BEFORE DELETE ON gwk.task
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('task');

-- Row-level triggers never fire on TRUNCATE, so the delete guard needs a
-- statement-level partner — else TRUNCATE wipes the table and every id is free
-- to be re-inserted at the initial state, the exact DELETE-then-INSERT
-- walk-around task_no_delete exists to stop, in one statement.
CREATE TRIGGER task_no_truncate
  BEFORE TRUNCATE ON gwk.task
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_state_row_delete('task');

ALTER TABLE gwk.task ENABLE ALWAYS TRIGGER task_transition;
ALTER TABLE gwk.task ENABLE ALWAYS TRIGGER task_born_initial;
ALTER TABLE gwk.task ENABLE ALWAYS TRIGGER task_no_delete;
ALTER TABLE gwk.task ENABLE ALWAYS TRIGGER task_no_truncate;

CREATE TABLE gwk.attempt (
  id                      text PRIMARY KEY,
  version                 bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state                   text NOT NULL DEFAULT 'queued'
                            CHECK (state IN ('queued', 'leased', 'starting',
                                             'running', 'blocked', 'canceling',
                                             'canceled', 'failed', 'unknown',
                                             'succeeded')),
  task_id                 text NOT NULL REFERENCES gwk.task(id),
  engine                  text NOT NULL,
  capability              text,
  role                    text,
  model_lane              text,
  permission_profile      text,
  worktree_lease_id       text REFERENCES gwk.lease(id),
  base_sha                text,
  budget                  jsonb,
  provider_session_ref    text,
  runtime_ref             text,
  runtime_started_at      timestamptz,
  exit_code               integer,
  provider_terminal_event text,
  result_valid            boolean,
  evidence_manifest_ref   text,
  created_at              timestamptz NOT NULL DEFAULT now(),
  updated_at              timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER attempt_transition
  BEFORE UPDATE ON gwk.attempt
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_transition('attempt');

CREATE TRIGGER attempt_born_initial
  BEFORE INSERT ON gwk.attempt
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_initial_state('attempt', 'queued');

CREATE TRIGGER attempt_no_delete
  BEFORE DELETE ON gwk.attempt
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('attempt');

CREATE TRIGGER attempt_no_truncate
  BEFORE TRUNCATE ON gwk.attempt
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_state_row_delete('attempt');

ALTER TABLE gwk.attempt ENABLE ALWAYS TRIGGER attempt_transition;
ALTER TABLE gwk.attempt ENABLE ALWAYS TRIGGER attempt_born_initial;
ALTER TABLE gwk.attempt ENABLE ALWAYS TRIGGER attempt_no_delete;
ALTER TABLE gwk.attempt ENABLE ALWAYS TRIGGER attempt_no_truncate;

CREATE TABLE gwk.engine_session (
  id                   text PRIMARY KEY,
  attempt_id           text NOT NULL REFERENCES gwk.attempt(id),
  engine               text NOT NULL,
  provider_session_ref text,
  started_at           timestamptz NOT NULL,
  ended_at             timestamptz
);

CREATE TABLE gwk.message (
  id                 text PRIMARY KEY,
  version            bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state              text NOT NULL DEFAULT 'accepted'
                       CHECK (state IN ('accepted', 'delivered', 'acknowledged',
                                        'applied', 'rejected', 'dead_letter')),
  idempotency_key    text NOT NULL UNIQUE,
  correlation_id     text,
  reply_to           text REFERENCES gwk.message(id),
  sender             text,
  recipient          text,
  channel            text,
  kind               text,
  payload            jsonb,
  deadline           timestamptz,
  delivery_attempts  integer NOT NULL DEFAULT 0 CHECK (delivery_attempts >= 0),
  dead_letter_reason text,
  -- channel name -> opaque per-channel delivery reference
  delivery_refs      jsonb,
  created_at         timestamptz NOT NULL DEFAULT now(),
  updated_at         timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER message_transition
  BEFORE UPDATE ON gwk.message
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_transition('message');

CREATE TRIGGER message_born_initial
  BEFORE INSERT ON gwk.message
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_initial_state('message', 'accepted');

CREATE TRIGGER message_no_delete
  BEFORE DELETE ON gwk.message
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('message');

CREATE TRIGGER message_no_truncate
  BEFORE TRUNCATE ON gwk.message
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_state_row_delete('message');

ALTER TABLE gwk.message ENABLE ALWAYS TRIGGER message_transition;
ALTER TABLE gwk.message ENABLE ALWAYS TRIGGER message_born_initial;
ALTER TABLE gwk.message ENABLE ALWAYS TRIGGER message_no_delete;
ALTER TABLE gwk.message ENABLE ALWAYS TRIGGER message_no_truncate;

CREATE TABLE gwk.command (
  id              text PRIMARY KEY,
  version         bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state           text NOT NULL DEFAULT 'issued'
                    CHECK (state IN ('issued', 'targeted', 'signaled',
                                     'verification_complete')),
  kind            text NOT NULL,
  target          text,
  actor           jsonb,
  idempotency_key text UNIQUE,
  -- The verified RESULT, distinct from the progress spine: present exactly
  -- when the command is verification_complete, written in the same
  -- transaction as that terminal transition — this CHECK is what makes a
  -- split write unrepresentable.
  outcome         text CHECK (outcome IN ('clean', 'partial', 'unknown')),
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT outcome_iff_verification_complete
    CHECK ((state = 'verification_complete') = (outcome IS NOT NULL))
);

CREATE TRIGGER command_transition
  BEFORE UPDATE ON gwk.command
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_transition('command');

CREATE TRIGGER command_born_initial
  BEFORE INSERT ON gwk.command
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_initial_state('command', 'issued');

CREATE TRIGGER command_no_delete
  BEFORE DELETE ON gwk.command
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('command');

CREATE TRIGGER command_no_truncate
  BEFORE TRUNCATE ON gwk.command
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_state_row_delete('command');

ALTER TABLE gwk.command ENABLE ALWAYS TRIGGER command_transition;
ALTER TABLE gwk.command ENABLE ALWAYS TRIGGER command_born_initial;
ALTER TABLE gwk.command ENABLE ALWAYS TRIGGER command_no_delete;
ALTER TABLE gwk.command ENABLE ALWAYS TRIGGER command_no_truncate;

CREATE TABLE gwk.gate (
  id            text PRIMARY KEY,
  version       bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  attempt_id    text REFERENCES gwk.attempt(id),
  phase_ref     text,
  -- OPEN kind (verify/review/security/eval/cert/...): new gate kinds are
  -- additive, never a schema change
  kind          text,
  -- A relayed permission prompt is a gate: the engine's question and its
  -- offered options travel on open, the chosen option on decide. All three
  -- absent for gates that are not prompts.
  question      text,
  options       jsonb,
  verdict       text NOT NULL DEFAULT 'pending'
                  CHECK (verdict IN ('pending', 'pass', 'fail', 'partial')),
  chosen_option text,
  evidence_ref  text,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE gwk.authority_grant (
  id           text PRIMARY KEY,
  grantee      jsonb NOT NULL,
  action_class text NOT NULL,
  scope        text,
  granted_at   timestamptz NOT NULL DEFAULT now(),
  expires_at   timestamptz,
  -- Set together when the grant is withdrawn. Without them a revoke could only
  -- be expressed by backdating `expires_at`, which makes "revoked for cause"
  -- and "expired on schedule" the same row — and `gw authority list` reads this
  -- table as the audit trail, so that difference is the point of it.
  revoked_at    timestamptz,
  revoke_reason text,
  receipt_id   text
);

-- Append-only attestation ledger (no version/updated_at: receipts are facts).
CREATE TABLE gwk.receipt (
  id             text PRIMARY KEY,
  actor          jsonb NOT NULL,
  action         text NOT NULL,
  subject_type   text NOT NULL,
  subject_id     text NOT NULL,
  from_state     text,
  to_state       text,
  observed_basis text,
  ts             timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION gwk.forbid_receipt_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk.receipt is append-only (% refused)', TG_OP;
END $$;

CREATE TRIGGER receipt_append_only
  BEFORE UPDATE OR DELETE ON gwk.receipt
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_receipt_mutation();

-- Statement-level TRUNCATE cover, as on gwk.event.
CREATE TRIGGER receipt_no_truncate
  BEFORE TRUNCATE ON gwk.receipt
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_receipt_mutation();

ALTER TABLE gwk.receipt ENABLE ALWAYS TRIGGER receipt_append_only;
ALTER TABLE gwk.receipt ENABLE ALWAYS TRIGGER receipt_no_truncate;

CREATE TABLE gwk.evidence (
  id         text PRIMARY KEY,
  -- OPEN kind (transcript/diff/log/...); no format column
  kind       text NOT NULL,
  ref        text NOT NULL,
  digest     text,
  byte_size  numeric(20,0)
               CHECK (byte_size >= 0 AND byte_size <= 18446744073709551615),
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE gwk.attention_item (
  id          text PRIMARY KEY,
  kind        text NOT NULL,
  summary     text NOT NULL,
  subject_ref text,
  raised_by   jsonb,
  -- Rank only, and the same `integer` the task carries. NOT cost-of-waiting:
  -- an urgency-times-age order needs a per-kind decay this contract does not
  -- model, and that clause is explicitly deferred rather than covered by this
  -- column. A reader who takes this as "queue ordering is solved" will ship a
  -- queue where a stale low-priority page never rises.
  priority    integer,
  raised_at   timestamptz NOT NULL DEFAULT now(),
  -- Seen and quiet-until stamps. Both leave `resolved_at` alone ON PURPOSE:
  -- the partial index below is predicated on `resolved_at IS NULL`, so an ack
  -- or a mute keeps the item holding its dedup slot and the muted problem
  -- cannot raise itself again the moment it is silenced. Idempotent
  -- last-write-wins — re-acking moves the stamp and is not an error.
  acked_at    timestamptz,
  muted_until timestamptz,
  resolved_at timestamptz,
  resolution  text
);

-- Attention deduplicates by (kind, subject_ref) WHILE UNRESOLVED: a page fires
-- once per real problem, not once per retry of the command that hit it. The
-- predicate is what makes the same pair raisable again after someone resolves
-- it — a recurring problem is a new item, not a reopened one.
--
-- NULL `subject_ref` stays distinct (the PostgreSQL default): an item about
-- nothing in particular is not the same item as another about nothing in
-- particular. The kernel's own page path always names a subject, so this only
-- affects an explicit RaiseAttention that omits one.
--
-- The predicate is `resolved_at IS NULL` and nothing else. Adding `acked_at`
-- or `muted_until` to it would let a silenced problem raise a second item the
-- instant it was silenced — the opposite of what quieting a row asks for.
CREATE UNIQUE INDEX attention_item_unresolved_dedup
  ON gwk.attention_item (kind, subject_ref)
  WHERE resolved_at IS NULL;

-- The only projection a replay cannot rebuild. `gwk.receipt` is the other one,
-- and it is append-only; this table is not, because resolving an item is an
-- UPDATE. But the kernel's own page path writes the row directly inside the
-- refused command's transaction with no event behind it, so a deleted item is
-- gone from every copy at once — and `derived_records`, which is what a
-- checkpoint and a scratch rebuild hash, deliberately leaves this table out.
-- Nothing else would notice. The refusal's receipt survives either way, so what
-- these guards protect is the operator's queue rather than the audit trail.
CREATE TRIGGER attention_item_no_delete
  BEFORE DELETE ON gwk.attention_item
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('attention_item');

CREATE TRIGGER attention_item_no_truncate
  BEFORE TRUNCATE ON gwk.attention_item
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_state_row_delete('attention_item');

ALTER TABLE gwk.attention_item ENABLE ALWAYS TRIGGER attention_item_no_delete;
ALTER TABLE gwk.attention_item ENABLE ALWAYS TRIGGER attention_item_no_truncate;

-- `released_at`/`disposition` are what make a released worktree distinguishable
-- from a live one in the projection. Without them the release is recoverable
-- only by replaying the log, and `projection get worktree <id>` — a first-class
-- read in the CLI surface — could not answer "is anyone still in this tree?".
-- The lease beside it carries the same pair for the same reason.
CREATE TABLE gwk.worktree (
  id          text PRIMARY KEY,
  repo        text NOT NULL,
  path        text NOT NULL,
  branch      text NOT NULL,
  base_sha    text,
  lease_id    text REFERENCES gwk.lease(id),
  dirty       boolean NOT NULL DEFAULT false,
  unpushed    boolean NOT NULL DEFAULT false,
  released_at timestamptz,
  disposition text,
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- A dispatch node is bookkeeping over the spawn tree, NOT one of the four
-- public state machines: `state` is an OPEN bounded lifecycle label with no
-- seeded edge set, so `assert_transition` (which trusts gwk.transition) does
-- not apply. What DOES apply is the CAS discipline every versioned row owes,
-- because concurrent writers race the same node.
CREATE TABLE gwk.dispatch_node (
  id         text PRIMARY KEY,
  version    bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  parent_id  text REFERENCES gwk.dispatch_node(id),
  attempt_id text REFERENCES gwk.attempt(id),
  kind       text NOT NULL,
  state      text NOT NULL DEFAULT 'registered',
  label      text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Version CAS without edge legality — the guard an open-lifecycle versioned
-- table needs. Deliberately NOT gwk.assert_transition: that one refuses any
-- state change lacking a gwk.transition row, and dispatch_node has no seeded
-- edges by design, so every transition would be refused.
CREATE FUNCTION gwk.assert_version_cas() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.version IS DISTINCT FROM OLD.version + 1 THEN
    RAISE EXCEPTION '% version must advance by exactly 1 (got % -> %)',
      TG_ARGV[0], OLD.version, NEW.version;
  END IF;
  RETURN NEW;
END $$;

CREATE TRIGGER dispatch_node_cas
  BEFORE UPDATE ON gwk.dispatch_node
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_version_cas('dispatch_node');
CREATE TRIGGER dispatch_node_no_delete
  BEFORE DELETE ON gwk.dispatch_node
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('dispatch_node');

ALTER TABLE gwk.dispatch_node ENABLE ALWAYS TRIGGER dispatch_node_cas;
ALTER TABLE gwk.dispatch_node ENABLE ALWAYS TRIGGER dispatch_node_no_delete;

-- The durable workspace tree: workspaces, tabs, panes, and each pane's session
-- binding. Structure ONLY — split sizes, focus, and z-order are the host's
-- transient geometry and never reach a row here; proportions resetting on a
-- host restart is the accepted consequence of that split, not a gap. The whole
-- live tree returns in one query, which is the point of the table.
--
-- Close is a real DELETE, unlike every state-carrying sibling: the wire entity
-- has no closed state, so a closed node simply leaves the tree. The log keeps
-- the history and a replay rebuilds the same live tree, which is why this
-- table carries no forbid-delete trigger. The parent FK's default NO ACTION is
-- the backstop that a node with live children cannot leave first.
--
-- Cold-path note (recorded with the migration): this table is ADDITIVE — no
-- existing projection changes shape. The contract fingerprint still moves, so
-- a live database follows by the cold path: initialize a fresh database with
-- the new contract and replay the event log into it (`gw admin
-- rebuild-projections --scratch-database <name>` is the same replay, with the
-- hash verdict as proof); nothing rewrites live rows in place.
CREATE TABLE gwk.workspace_node (
  id         text PRIMARY KEY,
  version    bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  kind       text NOT NULL CHECK (kind IN ('workspace', 'tab', 'pane')),
  parent_id  text REFERENCES gwk.workspace_node(id),
  session_id text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT workspace_node_session_only_on_pane
    CHECK (kind = 'pane' OR session_id IS NULL)
);

-- Legal parentage, enforced where a single-row CHECK cannot reach: the rule
-- reads the PARENT row's kind, so it lives in a trigger. The kernel refuses
-- illegal parentage before the append; this guard is the backstop that keeps
-- a foreign writer from bending the tree, not the error path.
--
--   workspace: a root — no parent, ever.
--   tab:       under a workspace.
--   pane:      under a tab, or under a pane (a split); never the other way
--              around — a pane cannot parent a tab.
CREATE FUNCTION gwk.assert_workspace_parentage() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  parent_kind text;
BEGIN
  IF NEW.parent_id IS NULL THEN
    IF NEW.kind <> 'workspace' THEN
      RAISE EXCEPTION 'workspace_node %: a % must sit under a parent', NEW.id, NEW.kind;
    END IF;
    RETURN NEW;
  END IF;
  IF NEW.kind = 'workspace' THEN
    RAISE EXCEPTION 'workspace_node %: a workspace is a root and cannot have a parent', NEW.id;
  END IF;
  IF NEW.parent_id = NEW.id THEN
    RAISE EXCEPTION 'workspace_node %: a node cannot parent itself', NEW.id;
  END IF;
  SELECT kind INTO parent_kind FROM gwk.workspace_node WHERE id = NEW.parent_id;
  IF NEW.kind = 'tab' AND parent_kind <> 'workspace' THEN
    RAISE EXCEPTION 'workspace_node %: a tab must sit under a workspace, not a %',
      NEW.id, parent_kind;
  END IF;
  IF NEW.kind = 'pane' AND parent_kind NOT IN ('tab', 'pane') THEN
    RAISE EXCEPTION 'workspace_node %: a pane must sit under a tab or a pane, not a %',
      NEW.id, parent_kind;
  END IF;
  RETURN NEW;
END $$;

CREATE TRIGGER workspace_node_cas
  BEFORE UPDATE ON gwk.workspace_node
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_version_cas('workspace_node');
CREATE TRIGGER workspace_node_parentage
  BEFORE INSERT OR UPDATE ON gwk.workspace_node
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_workspace_parentage();

ALTER TABLE gwk.workspace_node ENABLE ALWAYS TRIGGER workspace_node_cas;
ALTER TABLE gwk.workspace_node ENABLE ALWAYS TRIGGER workspace_node_parentage;

-- One run of a workflow: the choreography as a ledger object. The RUN, not
-- the template — `template_ref` is opaque client-side data, and `step` is an
-- open string because the act taxonomy is template data (decision 17): the
-- kernel asserts nothing about what the steps are called. Closed runs keep
-- their rows; the Board reads history from them, and the delete guard below
-- is what makes "ledger object" a property instead of prose.
CREATE TABLE gwk.workflow_run (
  id              text PRIMARY KEY,
  version         bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state           text NOT NULL DEFAULT 'running',
  step            text,
  template_ref    text NOT NULL,
  template_sha256 text CHECK (template_sha256 IS NULL OR template_sha256 ~ '^[0-9a-f]{64}$'),
  task_id         text REFERENCES gwk.task(id),
  title           text,
  opened_at       timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now(),
  closed_at       timestamptz,
  -- A run is closed exactly when it left `running`; half-closed rows are the
  -- bug this pins down.
  CONSTRAINT workflow_run_closed_iff_terminal
    CHECK ((state = 'running') = (closed_at IS NULL))
);

CREATE TRIGGER workflow_run_cas
  BEFORE UPDATE ON gwk.workflow_run
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_version_cas('workflow_run');
CREATE TRIGGER workflow_run_no_delete
  BEFORE DELETE ON gwk.workflow_run
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('workflow_run');

ALTER TABLE gwk.workflow_run ENABLE ALWAYS TRIGGER workflow_run_cas;
ALTER TABLE gwk.workflow_run ENABLE ALWAYS TRIGGER workflow_run_no_delete;

-- The orchestrator's crash-recovery snapshot, latest-per-orchestrator.
--
-- Distinct from `gwk-domain::checkpoint::Checkpoint`, which is the kernel's own
-- projection-hash checkpoint: this one is the orchestrator's impression of its
-- own in-flight work at crash time. Recovery re-reads the live rows; this is
-- the crash-time impression, not truth.
--
-- `seq` is the monotonic per-orchestrator counter and is guarded below. It is a
-- resume cursor, so a rewind would let recovery restart from state that has
-- already been superseded.
CREATE TABLE gwk.orchestrator_checkpoint (
  orchestrator_id    text PRIMARY KEY,
  seq                numeric(20,0) NOT NULL
                       CHECK (seq >= 0 AND seq <= 18446744073709551615),
  native_session_ref text,
  active_goal        text,
  active_step_ref    text,
  latest_command_ref text,
  open_attempts      jsonb,
  leases             jsonb,
  pending_approvals  jsonb,
  budget_cursor      jsonb,
  updated_at         timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION gwk.assert_checkpoint_seq_advances() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.seq <= OLD.seq THEN
    RAISE EXCEPTION 'orchestrator_checkpoint seq must advance (got % -> %)',
      OLD.seq, NEW.seq;
  END IF;
  RETURN NEW;
END $$;

CREATE TRIGGER orchestrator_checkpoint_seq
  BEFORE UPDATE ON gwk.orchestrator_checkpoint
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_checkpoint_seq_advances();

ALTER TABLE gwk.orchestrator_checkpoint ENABLE ALWAYS TRIGGER orchestrator_checkpoint_seq;

-- Records that entered the log through ingestion rather than through an
-- aggregate's lifecycle: memory, knowledge, graph snapshots, eval verdicts,
-- insights, cost, health, session, config, checkpoints, rounds, findings.
--
-- `kind` is the one CLOSED classification column in this file. Everywhere else
-- an open bounded string is preferred so a new label is additive; here the
-- closed set IS the property — the absence of an import path is load-bearing,
-- and an open column would let `import` in as data. The CHECK
-- lists the twelve accepted kinds so the refusal happens in the database and
-- not only in the process that wrote the row.
--
-- No version, no state, no updated_at: an ingested record is a fact that
-- arrived, like gwk.receipt. `event_seq` points back at the log entry that
-- produced it, which is the only thing that can prove the row — ingestion
-- writes no gwk.transition.
CREATE TABLE gwk.ingested_record (
  id          text PRIMARY KEY,
  kind        text NOT NULL
                CHECK (kind IN ('memory', 'knowledge', 'graph_snapshot',
                                'eval_verdict', 'insight', 'cost', 'health',
                                'session', 'config', 'checkpoint', 'round',
                                'finding')),
  -- Bounded inline content, or a descriptor of the blob beside it. The 64 KiB
  -- bound belongs to the append path (it bounds the whole event payload); a
  -- column CHECK here would be a second, drifting copy of it.
  payload     jsonb NOT NULL,
  payload_ref jsonb,
  ingested_by jsonb,
  event_seq   numeric(20,0) NOT NULL
                CHECK (event_seq >= 0 AND event_seq <= 18446744073709551615),
  ingested_at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION gwk.forbid_ingested_record_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk.ingested_record is append-only (% refused)', TG_OP;
END $$;

CREATE TRIGGER ingested_record_append_only
  BEFORE UPDATE OR DELETE ON gwk.ingested_record
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_ingested_record_mutation();

-- Statement-level TRUNCATE cover, as on gwk.event and gwk.receipt.
CREATE TRIGGER ingested_record_no_truncate
  BEFORE TRUNCATE ON gwk.ingested_record
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_ingested_record_mutation();

ALTER TABLE gwk.ingested_record ENABLE ALWAYS TRIGGER ingested_record_append_only;
ALTER TABLE gwk.ingested_record ENABLE ALWAYS TRIGGER ingested_record_no_truncate;

-- Append-only spend ledger (no version, no state, no updated_at: a cost
-- report is a fact). Rows rather than accumulator columns, so a token report
-- never rides the CAS version an FSM transition uses; totals are a rollup
-- query over this table.
--
-- Token counts are the universal fact — every engine reports them. Currency
-- is not: an engine that reports a figure reports it here as micro-USD with
-- its own estimate flag, and an engine that reports none leaves both NULL.
-- The ledger never converts tokens into money on an engine's behalf.
CREATE TABLE gwk.cost_entry (
  id                  text PRIMARY KEY,
  attempt_id          text REFERENCES gwk.attempt(id),
  engine_session_id   text REFERENCES gwk.engine_session(id),
  dispatch_node_id    text REFERENCES gwk.dispatch_node(id),
  engine              text NOT NULL,
  model               text,
  input_tokens        numeric(20,0)
                        CHECK (input_tokens >= 0 AND input_tokens <= 18446744073709551615),
  cached_input_tokens numeric(20,0)
                        CHECK (cached_input_tokens >= 0 AND cached_input_tokens <= 18446744073709551615),
  cache_write_tokens  numeric(20,0)
                        CHECK (cache_write_tokens >= 0 AND cache_write_tokens <= 18446744073709551615),
  output_tokens       numeric(20,0)
                        CHECK (output_tokens >= 0 AND output_tokens <= 18446744073709551615),
  reasoning_tokens    numeric(20,0)
                        CHECK (reasoning_tokens >= 0 AND reasoning_tokens <= 18446744073709551615),
  cost_micros         numeric(20,0)
                        CHECK (cost_micros >= 0 AND cost_micros <= 18446744073709551615),
  cost_is_estimate    boolean,
  recorded_at         timestamptz NOT NULL DEFAULT now(),
  -- A spend fact about nothing is noise: every entry names at least one
  -- subject it accrues to.
  CONSTRAINT cost_entry_attributed
    CHECK (num_nonnulls(attempt_id, engine_session_id, dispatch_node_id) >= 1),
  -- The estimate flag qualifies a currency figure; without one it asserts
  -- nothing, so the pair travels together.
  CONSTRAINT cost_entry_estimate_qualifies_cost
    CHECK (cost_is_estimate IS NULL OR cost_micros IS NOT NULL)
);

CREATE FUNCTION gwk.forbid_cost_entry_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk.cost_entry is append-only (% refused)', TG_OP;
END $$;

CREATE TRIGGER cost_entry_append_only
  BEFORE UPDATE OR DELETE ON gwk.cost_entry
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_cost_entry_mutation();

-- Statement-level TRUNCATE cover, as on gwk.event and gwk.receipt.
CREATE TRIGGER cost_entry_no_truncate
  BEFORE TRUNCATE ON gwk.cost_entry
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_cost_entry_mutation();

ALTER TABLE gwk.cost_entry ENABLE ALWAYS TRIGGER cost_entry_append_only;
ALTER TABLE gwk.cost_entry ENABLE ALWAYS TRIGGER cost_entry_no_truncate;

COMMIT;
"#;

/// SHA-256 of [`CONTRACT_SQL`], lowercase hex. Initialization records this
/// in `gwk_internal.schema_fingerprint`, so a later boot can tell "this
/// database carries MY contract" from "this database carries another".
// Pre-wrapped: `cargo fmt --all` formats generated files too, and an
// unwrapped 64-hex line lands past 100 columns — the generator and
// rustfmt would then fight, showing up as permanent contract drift.
pub const CONTRACT_SQL_SHA256: &str =
    "c168f2ad47bf61917be18185b74c9b5b288dc4e986b01565250749d8833e9e2d";
