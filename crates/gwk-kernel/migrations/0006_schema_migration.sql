-- 0006_schema_migration.sql — the append-only record of applied schema steps.
--
-- `gwk_internal.schema_fingerprint` says WHICH contract this database carries.
-- It is one row, overwritten in place by every step, and it therefore cannot
-- say how the database got here. This table is the other half: one row per
-- applied step, never rewritten, so the sequence a database actually took is
-- reconstructible afterwards rather than inferred from where it ended up.
--
-- It lives in `gwk_internal` because it is this engine's bookkeeping and not
-- part of the contract — a second backend owes nobody this table. That siting
-- has a consequence worth stating: `backend_script`'s blanket
-- `GRANT ... ON ALL TABLES IN SCHEMA gwk` does not reach here, so the table
-- arrives with no privileges at all and gets exactly the one it needs.
--
-- Append-only is enforced by TRIGGER, not by withholding a grant. A grant
-- binds the runtime role and nothing else; the role that applies a migration
-- is the admin one, and a superuser is bound by neither. A trigger declared
-- ENABLE ALWAYS fires for all of them.

-- Bounded, one-dimensional, and shaped like the file stems on disk. The column
-- is the only place a later reader learns which files a database took, so a
-- value that is not a file name is a row that cannot be checked against
-- anything.
CREATE FUNCTION gwk_internal.migration_names_are_valid(names text[]) RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
SELECT
  COALESCE(array_ndims(names), 1) = 1
  AND (cardinality(names) = 0 OR array_lower(names, 1) = 1)
  AND cardinality(names) <= 64
  AND NOT EXISTS (
    SELECT 1
    FROM unnest(names) AS migration(name)
    WHERE name IS NULL OR name !~ '^[0-9]{4}_[a-z_]+$'
  );
$$;

CREATE TABLE gwk_internal.schema_migration (
  -- A surrogate key, not `step_id`. A database restored from a backup and
  -- migrated again applies the same step a second time, and that is a second
  -- fact about what happened rather than a correction of the first.
  seq             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  base_sha256     text NOT NULL CHECK (base_sha256 ~ '^[0-9a-f]{64}$'),
  result_sha256   text NOT NULL CHECK (result_sha256 ~ '^[0-9a-f]{64}$'),
  -- The step's file name under `schema/steps/`, which is the identity the
  -- generator, the resolver, and this row all agree to call it by.
  step_id         text NOT NULL CHECK (length(step_id) BETWEEN 1 AND 128),
  applied_at      timestamptz NOT NULL DEFAULT now(),
  -- The applying binary's `GWK_PUBLIC_REVISION` stamp. Nullable because a
  -- locally built binary genuinely has none, and writing 'unknown' would be a
  -- string that sorts, groups, and reads like a revision without being one.
  public_revision text,
  -- The digest of the backup taken before the step ran. Nullable for the same
  -- reason: a step applied without one is a real state, and recording that
  -- honestly is worth more than a column that cannot represent it.
  backup_sha256   text CHECK (backup_sha256 ~ '^[0-9a-f]{64}$'),
  -- The backend migrations this run carried, in the order it applied them.
  --
  -- Recorded separately from the step because they are not the step's work.
  -- `gwk_internal` has no digest and no chain: `BACKEND_MIGRATIONS` runs at
  -- initialization and never again, so a migration added later reaches every
  -- new database and no existing one, and the applier closes that by carrying
  -- the ones a step declares. Crediting them to the step would make the ledger
  -- claim the contract DDL created relations it never mentions.
  --
  -- Empty is a real answer — a step that carries none says so — which is why
  -- this is NOT NULL over an empty array rather than nullable.
  backend_migrations text[] NOT NULL
                      CHECK (gwk_internal.migration_names_are_valid(backend_migrations)),
  -- Whether the operator asserted the base digest by hand rather than the
  -- applier reading it out of `schema_fingerprint`. An asserted base is a
  -- claim about the database; a read one is an observation of it, and a later
  -- reader must be able to tell which this row rests on.
  asserted_base   boolean NOT NULL,
  -- A step that arrives where it started is not a step; the resolver refuses
  -- one and the ledger must not be able to record what it refuses.
  CONSTRAINT schema_migration_step_moves CHECK (base_sha256 <> result_sha256)
);

-- The first trigger function in `gwk_internal`. The contract's own
-- `gwk.forbid_context_truth_mutation` does the same job on the other side of
-- the schema line, and that line is the one thing 0001 asks a reader to rely
-- on — so this is a sibling rather than a reach across it.
CREATE FUNCTION gwk_internal.forbid_schema_migration_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'gwk_internal.% is append-only (% refused)', TG_TABLE_NAME, TG_OP;
END $$;

CREATE TRIGGER schema_migration_append_only
  BEFORE UPDATE OR DELETE ON gwk_internal.schema_migration
  FOR EACH ROW EXECUTE FUNCTION gwk_internal.forbid_schema_migration_mutation();

-- A row-level guard never fires on TRUNCATE: there are no rows to fire per.
-- The statement-level cover is what makes "append-only" true of the table
-- rather than only of its rows.
CREATE TRIGGER schema_migration_no_truncate
  BEFORE TRUNCATE ON gwk_internal.schema_migration
  FOR EACH STATEMENT EXECUTE FUNCTION gwk_internal.forbid_schema_migration_mutation();

-- ENABLE ALWAYS, not the default ORIGIN. A default trigger is skipped when the
-- session is a replica, which is precisely the session a restore runs in.
ALTER TABLE gwk_internal.schema_migration ENABLE ALWAYS TRIGGER schema_migration_append_only;
ALTER TABLE gwk_internal.schema_migration ENABLE ALWAYS TRIGGER schema_migration_no_truncate;
