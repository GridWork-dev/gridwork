-- base:   be73d92052d5077babc30815264825bc471b706d2b5a2d077b54e94ad6603c75
-- result: d1ab71d5019c67f05ada6229ed3231b5c5825a65ab28a912e665160ff8fb51f6
-- carries:
--
-- A participation states which side of the public/private seam its candidate
-- sits on (8B task 11). ADDITIVE to the validator only: one more required key
-- on each element of gwk.context_manifest.participations, drawn from the same
-- closed set gwk.context_blob.content_class already uses.
--
-- Replacing the function rather than altering a column is the whole migration,
-- because participations are jsonb inside an append-only table. That makes the
-- step safe or unsafe on a question about existing ROWS rather than about the
-- function: a row written before this step carries no class key, and this
-- function refuses it.
--
-- The argument that no such row exists is a REACHABILITY argument, not a census
-- of any database. Nothing can have appended a ManifestResolved fact: there is
-- no KernelRequest or KernelCommand variant carrying a ContextFact, so a
-- Context payload fails at the serving path as a validation refusal, and the
-- handshake refuses protocol major 2 at three further sites. The one entry
-- point, record_context_fact, takes typed Rust values and is reachable only
-- from an in-process caller that task 32 has not written yet. A census would be
-- the stronger evidence and this step does not have one.
--
-- If a row ever does predate this step, the step is no longer additive and owes
-- a backfill: the re-armed CHECK refuses the table its own contents at the next
-- write that touches them, which is loud, but it is loud at write time rather
-- than here.
--
-- The tokens are hand-copied here from the same enum the contract copies them
-- from; xtask's token-parity check holds all three copies to ContentClass.

CREATE OR REPLACE FUNCTION gwk.context_participations_are_valid(value jsonb) RETURNS boolean
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
      WHERE name NOT IN ('digest', 'class', 'state', 'reason', 'detail')
    ) THEN
      RETURN false;
    END IF;
    IF jsonb_typeof(participation -> 'digest') IS DISTINCT FROM 'string'
       OR participation ->> 'digest' !~ '^sha256:[0-9a-f]{64}$' THEN
      RETURN false;
    END IF;

    IF jsonb_typeof(participation -> 'class') IS DISTINCT FROM 'string'
       OR participation ->> 'class' NOT IN ('conformance', 'private') THEN
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

-- IN PLACE. The database now carries the contract this step's header names as
-- its result, and `gwk_internal.schema_fingerprint` is where it says so.
-- Recorded here rather than left to whatever applies the step, so that a chain
-- interrupted between two steps still records exactly how far it got. One row,
-- id 1, by the contract's own construction; counted because an UPDATE that
-- matched nothing reports the same zero as one that changed nothing.
DO $$
DECLARE
  stamped bigint;
BEGIN
  UPDATE gwk_internal.schema_fingerprint
     SET contract_sha256 = 'd1ab71d5019c67f05ada6229ed3231b5c5825a65ab28a912e665160ff8fb51f6'
   WHERE id = 1;
  GET DIAGNOSTICS stamped = ROW_COUNT;
  IF stamped <> 1 THEN
    RAISE EXCEPTION
      'expected to stamp exactly one schema_fingerprint row and stamped %: without it '
      'this database would keep reporting the contract it no longer carries', stamped;
  END IF;
END $$;
