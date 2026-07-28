-- 0001_kernel_internal.sql — PostgreSQL-only mechanics.
--
-- Everything the CONTRACT requires lives in schema/0001_contract.sql and is
-- backend-neutral: any conforming engine must uphold it. This file is the
-- other half — what this particular engine needs in order to serve that
-- contract — and it is kept in its own schema so the two never blur. A reader
-- can tell "contract" from "PostgreSQL mechanics" by which schema an object
-- is in.
--
-- Applied by `gw admin init` in one implicit transaction alongside the
-- fingerprint row and the runtime grants; never by psql on its own.

CREATE SCHEMA IF NOT EXISTS gwk_internal;

-- Which contract this database carries, and therefore which binaries may
-- serve it. Exactly one row: `id` is pinned to 1, so a second initialization
-- collides on the primary key instead of quietly adding a second answer.
CREATE TABLE gwk_internal.schema_fingerprint (
  id              integer PRIMARY KEY DEFAULT 1 CHECK (id = 1),
  contract_sha256 text NOT NULL CHECK (contract_sha256 ~ '^[0-9a-f]{64}$'),
  applied_at      timestamptz NOT NULL DEFAULT now()
);
