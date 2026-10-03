-- Durable idempotency for saved checkpoints.
--
-- A save_checkpoint retried with the same caller and idempotency key returns
-- the checkpoint the first attempt saved instead of appending another one,
-- also after the engine restarts. The engine derives the receipt id from the
-- caller and the key (the key itself is never stored) and writes the receipt
-- in the same transaction as the task update, the decisions and the
-- checkpoint it describes, so either all of them exist or none do.
CREATE TABLE checkpoint_receipt (
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  id              uuid NOT NULL,
  task_id         uuid NOT NULL,
  seq             bigint NOT NULL CHECK (seq >= 1),
  created_at      timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (organization_id, id),
  CONSTRAINT checkpoint_receipt_task_fk FOREIGN KEY (task_id, organization_id)
    REFERENCES task (id, organization_id) ON DELETE CASCADE,
  CONSTRAINT checkpoint_receipt_checkpoint_fk FOREIGN KEY (task_id, seq)
    REFERENCES task_checkpoint (task_id, seq) ON DELETE CASCADE
);
CREATE INDEX checkpoint_receipt_task_idx ON checkpoint_receipt (task_id);
CREATE TRIGGER checkpoint_receipt_append_only
  BEFORE UPDATE ON checkpoint_receipt
  FOR EACH ROW EXECUTE FUNCTION knowell_reject_update();
