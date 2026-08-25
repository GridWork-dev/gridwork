-- 0005_pty_delivery.sql — durable post-commit delivery state for PTY controls.
--
-- The command event and this pending row land in one transaction. Delivery
-- attempts take a short durable lease, release the database transaction before
-- touching a host, and settle from the host's application acknowledgement.

CREATE TABLE gwk_internal.pty_delivery (
  project_id      text NOT NULL,
  idempotency_key text NOT NULL,
  command_id      text NOT NULL UNIQUE,
  event_id        text NOT NULL,
  event_seq       numeric(20,0) NOT NULL,
  input_binding   bytea,
  claim_token     text,
  claimed_until   timestamptz,
  delivered_at    timestamptz,
  failed_at       timestamptz,
  indeterminate_at timestamptz,
  failure_code    text,
  failure_message text,
  UNIQUE (event_id),
  CHECK (num_nonnulls(delivered_at, failed_at, indeterminate_at) <= 1),
  CHECK ((claim_token IS NULL) = (claimed_until IS NULL)),
  CHECK (num_nonnulls(delivered_at, failed_at, indeterminate_at) = 0 OR claim_token IS NULL),
  CHECK ((failed_at IS NULL) = (failure_code IS NULL)),
  CHECK ((failed_at IS NULL) = (failure_message IS NULL)),
  PRIMARY KEY (project_id, idempotency_key)
);

CREATE INDEX pty_delivery_pending_seq
  ON gwk_internal.pty_delivery (event_seq)
  WHERE delivered_at IS NULL AND failed_at IS NULL AND indeterminate_at IS NULL;
