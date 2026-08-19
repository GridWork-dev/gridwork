-- base:   aba2f647bc7bb447e7b53307196f63df0bc718d479ec4693f6dd34ec9bf7b545
-- result: 7ebb2adaad295c28c60f1e789030ceef87d6fdd607b37a1186f173dc22647142
-- carries: 0005_pty_delivery, 0006_schema_migration
--
-- The retroactive step: the contract a database initialized at 4d54bba carries,
-- brought to the one this binary carries.
--
-- Three merges moved the contract in between, and this file replays all three
-- in commit order. It reproduces historical DDL and does NOT edit
-- schema/0001_contract.sql, which already describes the result.
--
--   44d20e0 (#103)  gwk.gate.decided_by + its CHECK; pty_session.engine_session_id
--   e680b00 (#112)  two pty_session CHECK rewrites; gwk.pty_session_template
--   b77e78b (#147)  the four Context truth records
--
-- Every statement that touches existing rows is marked IN PLACE below and says
-- what it does to them. There is exactly one that rewrites data rather than
-- adding a nullable column or re-checking a constraint: the gate decision
-- backfill.
--
-- The whole file is one transaction. A half-applied contract is a database that
-- matches no digest at all, and there is nothing to compare it against.
--
-- ONE THING TO KNOW ABOUT THE ESTATE AFTERWARDS. A database that took this step
-- and one initialized fresh from schema/0001_contract.sql report the SAME
-- contract digest and are NOT byte-identical: `gwk.gate.decided_by` and
-- `gwk.pty_session.engine_session_id` sit last in their tables here, because
-- PostgreSQL appends an added column and the contract declares both mid-table.
-- Nothing observes that — every query in the kernel names its columns — and
-- closing it would mean rebuilding both tables and recreating the CAS and
-- append-only triggers on pty_session, which trades a real risk for a
-- difference no caller can see. It is asserted rather than removed, in
-- crates/gwk-kernel/tests/admin_migrate.rs, so that a THIRD difference could
-- never hide behind it.

BEGIN;

-- ---------------------------------------------------------------------------
-- #103 — the receipted PTY input path (44d20e0)
-- ---------------------------------------------------------------------------

-- IN PLACE. Existing `gwk.gate` rows gain a NULL `decided_by`, which every
-- non-pending row is about to violate — the backfill below is what makes the
-- constraint addable, and it runs before the constraint for that reason.
ALTER TABLE gwk.gate ADD COLUMN decided_by jsonb;

-- IN PLACE, and this is the only place in this step where existing rows are
-- rewritten rather than merely re-checked.
--
-- The projector at `crates/gwk-kernel/src/project.rs` derives this column as
-- `(!matches!(verdict, GateVerdict::Pending)).then_some(&event.actor)` — the
-- actor of the deciding event, and nothing else. Replaying the log would land
-- the same values; reading them straight out of it lands them without a replay.
--
-- DISTINCT ON, not a plain join, because a gate may be decided more than once:
-- the projector treats a re-decide as a whole new decision whose write wins, so
-- a gate can carry several `gate_decided` events and only the latest describes
-- the row as it stands. An unqualified join emits one output row per matching
-- event and lets the server choose among them, which is non-determinism in
-- exactly the place this migration exists to be deterministic.
--
-- The count is taken BEFORE the update and compared after. `UPDATE ... WHERE`
-- reports zero rows for "every row was already correct" and for "the join
-- matched nothing", and those are different facts — one is a no-op and the
-- other is an attribution that silently failed to recover.
DO $$
DECLARE
  decided bigint;
  filled  bigint;
BEGIN
  SELECT count(*) INTO decided FROM gwk.gate WHERE verdict <> 'pending';

  UPDATE gwk.gate g
     SET decided_by = d.actor
    FROM (
          SELECT DISTINCT ON (aggregate_id) aggregate_id, actor
            FROM gwk.event
           WHERE event_type = 'gate_decided'
           ORDER BY aggregate_id, seq DESC
         ) d
   WHERE d.aggregate_id = g.id
     AND g.verdict <> 'pending';
  GET DIAGNOSTICS filled = ROW_COUNT;

  IF filled <> decided THEN
    RAISE EXCEPTION
      'gate decision backfill recovered % of % decided rows: a decided gate with no '
      '`gate_decided` event in the log cannot have its actor reconstructed, and the '
      'constraint below would reject it. Resolve the gap before migrating',
      filled, decided;
  END IF;
END $$;

ALTER TABLE gwk.gate ADD CONSTRAINT gate_decision_actor_matches_verdict CHECK (
  (verdict = 'pending' AND decided_by IS NULL)
  OR (verdict <> 'pending' AND decided_by IS NOT NULL)
);

-- IN PLACE, additive only: existing `gwk.pty_session` rows gain a NULL
-- `engine_session_id`, which is the honest value. A session opened before the
-- join existed was never attributed to an engine session, and NULL is what the
-- column means for a headless one anyway.
ALTER TABLE gwk.pty_session
  ADD COLUMN engine_session_id text REFERENCES gwk.engine_session(id);

-- ---------------------------------------------------------------------------
-- #112 — routed PTY control and declared starts (e680b00)
-- ---------------------------------------------------------------------------

-- IN PLACE, widening: `stopping` becomes a legal state. No existing row changes
-- and none can fail the new check, because the old check admitted a strict
-- subset of what this one does.
ALTER TABLE gwk.pty_session DROP CONSTRAINT pty_session_state_check;
ALTER TABLE gwk.pty_session ADD CONSTRAINT pty_session_state_check
  CHECK (state IN ('running', 'stopping', 'closed'));

-- IN PLACE, and NOT a widening — this one can reject a row that the old
-- constraint accepted. Old: closed_at is NULL exactly while running. New:
-- closed_at is set exactly when closed. The two agree on every row a
-- two-state table could hold, which is every row that exists here: `stopping`
-- became legal one statement ago and nothing has written it yet. The
-- difference only appears for a `stopping` row, where the old form demanded a
-- closed_at and the new form forbids it — which is the bug the rewrite fixed.
--
-- PostgreSQL validates an added CHECK against existing rows by default, so if
-- that reasoning is wrong this statement fails rather than recording a lie.
ALTER TABLE gwk.pty_session DROP CONSTRAINT pty_session_closed_iff_terminal;
ALTER TABLE gwk.pty_session ADD CONSTRAINT pty_session_closed_iff_terminal
  CHECK ((state = 'closed') = (closed_at IS NOT NULL));

-- ADDITIVE: a new table, its triggers, and the ENABLE ALWAYS that makes them
-- fire for a replica session too. Nothing existing is touched.

-- Operator-declared executable catalog. Starts carry only this name and a
-- caller-minted session id; command/cwd/env never ride the start request.
CREATE TABLE gwk.pty_session_template (
  name         text PRIMARY KEY,
  version      bigint NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 4294967295),
  state        text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'retired')),
  command      text NOT NULL CHECK (length(command) BETWEEN 1 AND 4096),
  args         jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(args) = 'array'),
  cwd          text,
  env          jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(env) = 'object'),
  cols         integer NOT NULL CHECK (cols BETWEEN 1 AND 1000),
  rows         integer NOT NULL CHECK (rows BETWEEN 1 AND 1000),
  declared_at  timestamptz NOT NULL DEFAULT now(),
  updated_at   timestamptz NOT NULL DEFAULT now(),
  retired_at   timestamptz,
  CONSTRAINT pty_template_retired_iff_terminal
    CHECK ((state = 'active') = (retired_at IS NULL)),
  CONSTRAINT pty_template_grid_cell_bound CHECK (cols * rows <= 100000)
);

CREATE TRIGGER pty_session_template_cas
  BEFORE UPDATE ON gwk.pty_session_template
  FOR EACH ROW EXECUTE FUNCTION gwk.assert_version_cas('pty_session_template');
CREATE TRIGGER pty_session_template_no_delete
  BEFORE DELETE ON gwk.pty_session_template
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_state_row_delete('pty_session_template');
CREATE TRIGGER pty_session_template_no_truncate
  BEFORE TRUNCATE ON gwk.pty_session_template
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_state_row_delete('pty_session_template');

ALTER TABLE gwk.pty_session_template ENABLE ALWAYS TRIGGER pty_session_template_cas;
ALTER TABLE gwk.pty_session_template ENABLE ALWAYS TRIGGER pty_session_template_no_delete;
ALTER TABLE gwk.pty_session_template ENABLE ALWAYS TRIGGER pty_session_template_no_truncate;


-- ---------------------------------------------------------------------------
-- #147 — the four immutable Context truth records (b77e78b)
-- ---------------------------------------------------------------------------

-- ADDITIVE throughout: two validator functions, four tables, one trigger
-- function, and the append-only triggers over them. No existing row is read or
-- rewritten. The REVOKE that makes these records immutable is not here — it
-- lives in the kernel's `backend_script`, and this step reproduces it below
-- with the rest of the grant matrix.

-- Context truth records. Their concrete table is their truth stage: no shared
-- `stage` or `kind` discriminator exists. All payload-like content stays out of
-- these rows; the records carry bounded metadata and content digests only.
CREATE FUNCTION gwk.context_evidence_ids_are_valid(ids text[]) RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
SELECT
  COALESCE(array_ndims(ids), 1) = 1
  AND (cardinality(ids) = 0 OR array_lower(ids, 1) = 1)
  AND cardinality(ids) <= 64
  AND NOT EXISTS (
    SELECT 1
    FROM unnest(ids) AS evidence(id)
    WHERE id IS NULL OR id !~ '^[A-Za-z0-9._:-]{1,128}$'
  );
$$;

CREATE FUNCTION gwk.context_participations_are_valid(value jsonb) RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
  participation jsonb;
  participation_state text;
BEGIN
  IF jsonb_typeof(value) <> 'array' OR jsonb_array_length(value) > 4096 THEN
    RETURN false;
  END IF;

  FOR participation IN
    SELECT element FROM jsonb_array_elements(value) AS entry(element)
  LOOP
    IF jsonb_typeof(participation) <> 'object' THEN
      RETURN false;
    END IF;
    IF EXISTS (
      SELECT 1
      FROM jsonb_object_keys(participation) AS field(name)
      WHERE name NOT IN ('digest', 'state', 'reason', 'detail')
    ) THEN
      RETURN false;
    END IF;
    IF jsonb_typeof(participation -> 'digest') IS DISTINCT FROM 'string'
       OR participation ->> 'digest' !~ '^sha256:[0-9a-f]{64}$' THEN
      RETURN false;
    END IF;

    participation_state := participation ->> 'state';
    IF jsonb_typeof(participation -> 'state') IS DISTINCT FROM 'string'
       OR participation_state NOT IN
          ('available', 'selectable', 'active', 'excluded', 'unavailable') THEN
      RETURN false;
    END IF;

    IF participation_state IN ('excluded', 'unavailable') THEN
      IF jsonb_typeof(participation -> 'reason') IS DISTINCT FROM 'string'
         OR participation ->> 'reason' NOT IN
            ('precedence_loss', 'permission_denied', 'budget_cut', 'quarantined',
             'rejected', 'pin_drift', 'not_eligible', 'unavailable') THEN
        RETURN false;
      END IF;
    ELSIF participation ? 'reason'
          AND participation -> 'reason' <> 'null'::jsonb THEN
      RETURN false;
    END IF;

    IF participation ? 'detail'
       AND participation -> 'detail' <> 'null'::jsonb
       AND (participation_state NOT IN ('excluded', 'unavailable')
            OR jsonb_typeof(participation -> 'detail') IS DISTINCT FROM 'string'
            OR octet_length(participation ->> 'detail') > 1024) THEN
      RETURN false;
    END IF;
  END LOOP;

  RETURN true;
END;
$$;

CREATE TABLE gwk.context_manifest (
  id                text PRIMARY KEY
                      CHECK (id ~ '^[A-Za-z0-9._:-]{1,128}$'),
  attempt_id        text NOT NULL REFERENCES gwk.attempt(id),
  manifest_digest   text NOT NULL
                      CHECK (manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
  route_digest      text NOT NULL
                      CHECK (route_digest ~ '^sha256:[0-9a-f]{64}$'),
  authority_digest  text NOT NULL
                      CHECK (authority_digest ~ '^sha256:[0-9a-f]{64}$'),
  source_count      integer NOT NULL
                      CHECK (source_count BETWEEN 0 AND 65535),
  source_bytes      numeric(20,0) NOT NULL
                      CHECK (source_bytes >= 0 AND source_bytes <= 18446744073709551615),
  participations    jsonb NOT NULL
                      CHECK (gwk.context_participations_are_valid(participations)),
  evidence_ids      text[] NOT NULL
                      CHECK (gwk.context_evidence_ids_are_valid(evidence_ids)),
  resolved_at       timestamptz NOT NULL,
  CONSTRAINT context_manifest_one_per_attempt UNIQUE (attempt_id)
);

CREATE TABLE gwk.context_release (
  id                  text PRIMARY KEY
                        CHECK (id ~ '^[A-Za-z0-9._:-]{1,128}$'),
  manifest_id         text NOT NULL REFERENCES gwk.context_manifest(id),
  rendered_digest     text NOT NULL
                        CHECK (rendered_digest ~ '^sha256:[0-9a-f]{64}$'),
  tool_schema_digest  text NOT NULL
                        CHECK (tool_schema_digest ~ '^sha256:[0-9a-f]{64}$'),
  rendered_bytes      numeric(20,0) NOT NULL
                        CHECK (rendered_bytes >= 0
                               AND rendered_bytes <= 18446744073709551615),
  tool_schema_count   integer NOT NULL
                        CHECK (tool_schema_count BETWEEN 0 AND 65535),
  evidence_ids        text[] NOT NULL
                        CHECK (gwk.context_evidence_ids_are_valid(evidence_ids)),
  released_at         timestamptz NOT NULL,
  CONSTRAINT context_release_one_per_manifest UNIQUE (manifest_id)
);

CREATE TABLE gwk.context_observation (
  id                    text PRIMARY KEY
                          CHECK (id ~ '^[A-Za-z0-9._:-]{1,128}$'),
  manifest_id           text NOT NULL REFERENCES gwk.context_manifest(id),
  observation_index     integer NOT NULL
                          CHECK (observation_index BETWEEN 1 AND 65535),
  fact_digest           text NOT NULL
                          CHECK (fact_digest ~ '^sha256:[0-9a-f]{64}$'),
  observed_bytes        numeric(20,0) NOT NULL
                          CHECK (observed_bytes >= 0
                                 AND observed_bytes <= 18446744073709551615),
  visible_source_count  integer NOT NULL
                          CHECK (visible_source_count BETWEEN 0 AND 65535),
  truncated             boolean NOT NULL,
  evidence_ids          text[] NOT NULL
                          CHECK (gwk.context_evidence_ids_are_valid(evidence_ids)),
  observed_at           timestamptz NOT NULL,
  CONSTRAINT context_observation_stable_order
    UNIQUE (manifest_id, observation_index)
);

CREATE TABLE gwk.context_finalization (
  id                    text PRIMARY KEY
                          CHECK (id ~ '^[A-Za-z0-9._:-]{1,128}$'),
  manifest_id           text NOT NULL REFERENCES gwk.context_manifest(id),
  output_digest         text NOT NULL
                          CHECK (output_digest ~ '^sha256:[0-9a-f]{64}$'),
  verification_digest   text NOT NULL
                          CHECK (verification_digest ~ '^sha256:[0-9a-f]{64}$'),
  approval_count        integer NOT NULL
                          CHECK (approval_count BETWEEN 0 AND 65535),
  observation_count     integer NOT NULL
                          CHECK (observation_count BETWEEN 0 AND 65535),
  final_event_root      text NOT NULL
                          CHECK (final_event_root ~ '^sha256:[0-9a-f]{64}$'),
  lifecycle_complete    boolean NOT NULL,
  assurance             text NOT NULL
                          CHECK (assurance IN ('trace', 'deterministic')),
  evidence_ids          text[] NOT NULL
                          CHECK (gwk.context_evidence_ids_are_valid(evidence_ids)),
  finalized_at          timestamptz NOT NULL,
  CONSTRAINT context_finalization_one_per_manifest UNIQUE (manifest_id)
);

CREATE FUNCTION gwk.forbid_context_truth_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk.% is append-only (% refused)', TG_TABLE_NAME, TG_OP;
END $$;

CREATE TRIGGER context_manifest_append_only
  BEFORE UPDATE OR DELETE ON gwk.context_manifest
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_context_truth_mutation();
CREATE TRIGGER context_manifest_no_truncate
  BEFORE TRUNCATE ON gwk.context_manifest
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_context_truth_mutation();

CREATE TRIGGER context_release_append_only
  BEFORE UPDATE OR DELETE ON gwk.context_release
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_context_truth_mutation();
CREATE TRIGGER context_release_no_truncate
  BEFORE TRUNCATE ON gwk.context_release
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_context_truth_mutation();

CREATE TRIGGER context_observation_append_only
  BEFORE UPDATE OR DELETE ON gwk.context_observation
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_context_truth_mutation();
CREATE TRIGGER context_observation_no_truncate
  BEFORE TRUNCATE ON gwk.context_observation
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_context_truth_mutation();

CREATE TRIGGER context_finalization_append_only
  BEFORE UPDATE OR DELETE ON gwk.context_finalization
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_context_truth_mutation();
CREATE TRIGGER context_finalization_no_truncate
  BEFORE TRUNCATE ON gwk.context_finalization
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_context_truth_mutation();

ALTER TABLE gwk.context_manifest ENABLE ALWAYS TRIGGER context_manifest_append_only;
ALTER TABLE gwk.context_manifest ENABLE ALWAYS TRIGGER context_manifest_no_truncate;
ALTER TABLE gwk.context_release ENABLE ALWAYS TRIGGER context_release_append_only;
ALTER TABLE gwk.context_release ENABLE ALWAYS TRIGGER context_release_no_truncate;
ALTER TABLE gwk.context_observation ENABLE ALWAYS TRIGGER context_observation_append_only;
ALTER TABLE gwk.context_observation ENABLE ALWAYS TRIGGER context_observation_no_truncate;
ALTER TABLE gwk.context_finalization ENABLE ALWAYS TRIGGER context_finalization_append_only;
ALTER TABLE gwk.context_finalization ENABLE ALWAYS TRIGGER context_finalization_no_truncate;


-- ---------------------------------------------------------------------------
-- The backend half — carried by the applier, not by this file
-- ---------------------------------------------------------------------------
--
-- `schema/0001_contract.sql` is engine-neutral and says nothing about
-- `gwk_internal` or about who may read what. Both moved over these same three
-- merges, and an earlier draft of this step reproduced them inline: migration
-- 0005's DDL copied in, and the whole privilege matrix replayed by a DO block
-- that read the runtime role back off the database.
--
-- Both are gone, and their absence is the point. `gw admin migrate` applies
-- this file, then the backend migrations named on the `-- carries:` header
-- above, then the privilege statements — read out of the kernel's own
-- `backend_script`, so there is exactly one place the grant matrix is written
-- down. A copy here would have been a second, and the ledger row could not
-- have said which of the two a database actually took.

-- IN PLACE. The database now carries the contract this step's header names as
-- its result, and `gwk_internal.schema_fingerprint` is where it says so —
-- `admin::inspect` reads that row and nothing else to decide what a target is.
-- Recorded here rather than left to whatever applies the step, so that a chain
-- interrupted between two steps still records exactly how far it got.
--
-- One row, id 1, by the contract's own construction. Counted for the same
-- reason every other fold in this step is: an UPDATE that matched nothing
-- reports the same zero as an UPDATE that changed nothing.
DO $$
DECLARE
  stamped bigint;
BEGIN
  UPDATE gwk_internal.schema_fingerprint
     SET contract_sha256 = '7ebb2adaad295c28c60f1e789030ceef87d6fdd607b37a1186f173dc22647142'
   WHERE id = 1;
  GET DIAGNOSTICS stamped = ROW_COUNT;
  IF stamped <> 1 THEN
    RAISE EXCEPTION
      'expected to stamp exactly one schema_fingerprint row and stamped %: without it '
      'this database would keep reporting the contract it no longer carries', stamped;
  END IF;
END $$;

COMMIT;
