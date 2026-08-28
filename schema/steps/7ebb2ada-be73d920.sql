-- base:   7ebb2adaad295c28c60f1e789030ceef87d6fdd607b37a1186f173dc22647142
-- result: be73d92052d5077babc30815264825bc471b706d2b5a2d077b54e94ad6603c75
-- carries:
--
-- The Context CAS classification table (8B task 7). ADDITIVE throughout: one
-- table, its two mutation guards, and the ENABLE ALWAYS that makes them fire
-- for a replica session too. No existing row is read or rewritten, and no
-- backend migration rides along — the sweep-side and grant-side halves live in
-- kernel code and the privilege matrix, which `gw admin migrate` re-applies
-- wholesale from the kernel's own `backend_script` after this file.
--
-- The DDL below reproduces the table exactly as `schema/0001_contract.sql`
-- declares it at this step's result digest; the contract file already
-- describes the result and this file does not edit it.

-- Context blob classification: one row per classified CAS object, beside the
-- blob and never inside the container (the v1 container bytes are untouched;
-- classification is a metadata-layer concern, exactly how evidence pinning was
-- added without a format change).
--
-- Each class column is a CLOSED set, like `ingested_record.kind`: the closed
-- set IS the property. `content_class` is the KEK domain — one key-encryption
-- key per content class, so the public-conformance/private-real seam is a
-- cryptographic boundary, not a convention. `redaction_class` records what
-- treatment the plaintext received before sealing. `retention_class` is the
-- family the retention sweep keys on — the class set is contract, while each
-- class's window is backend configuration, and a class with no configured
-- window is retained (the sweep fails safe toward keeping bytes).
--
-- Digest-keyed and append-only: one digest is one blob, a blob is sealed under
-- exactly one class KEK, and reclassification is not a thing a row can mean —
-- the same bytes under a different classification are a refused write. The row
-- outlives the blob's bytes on purpose, like `gwk.evidence`: after a retention
-- sweep or a crypto-shred it is the auditable record that classified content
-- existed and what class it was, which is precisely what a retention audit
-- asks.
CREATE TABLE gwk.context_blob (
  digest           text PRIMARY KEY
                     CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
  content_class    text NOT NULL
                     CHECK (content_class IN ('conformance', 'private')),
  redaction_class  text NOT NULL
                     CHECK (redaction_class IN ('none', 'redacted')),
  retention_class  text NOT NULL
                     CHECK (retention_class IN ('permanent', 'manifest',
                                                'release', 'observation')),
  created_at       timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER context_blob_append_only
  BEFORE UPDATE OR DELETE ON gwk.context_blob
  FOR EACH ROW EXECUTE FUNCTION gwk.forbid_context_truth_mutation();
CREATE TRIGGER context_blob_no_truncate
  BEFORE TRUNCATE ON gwk.context_blob
  FOR EACH STATEMENT EXECUTE FUNCTION gwk.forbid_context_truth_mutation();

ALTER TABLE gwk.context_blob ENABLE ALWAYS TRIGGER context_blob_append_only;
ALTER TABLE gwk.context_blob ENABLE ALWAYS TRIGGER context_blob_no_truncate;

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
     SET contract_sha256 = 'be73d92052d5077babc30815264825bc471b706d2b5a2d077b54e94ad6603c75'
   WHERE id = 1;
  GET DIAGNOSTICS stamped = ROW_COUNT;
  IF stamped <> 1 THEN
    RAISE EXCEPTION
      'expected to stamp exactly one schema_fingerprint row and stamped %: without it '
      'this database would keep reporting the contract it no longer carries', stamped;
  END IF;
END $$;
