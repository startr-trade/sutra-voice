-- The `transfer_log` store's own table. Run on every activation, so idempotent. No seed rows:
-- the flow writes one row per transfer, per refused PIN, and per call that gave up or hung up.
--
-- A PROJECTED table (`structure: {schema: urn:ivr, type: TransferLogEntry}`), keyed by
-- "<callId>/<n>". It never holds the digits a caller pressed.

CREATE TABLE IF NOT EXISTS transfer_log (
  store_key      VARCHAR(512) NOT NULL,
  call_id        VARCHAR(64)  NOT NULL,
  caller         VARCHAR(20)  NOT NULL,
  extension      VARCHAR(10),
  department     VARCHAR(40),
  outcome        VARCHAR(40)  NOT NULL,
  method         VARCHAR(40),
  reason         VARCHAR(40),
  note           VARCHAR(120),
  menu_attempts  INTEGER      NOT NULL,
  pin_attempts   INTEGER      NOT NULL,
  logged_at      TIMESTAMP WITH TIME ZONE NOT NULL,
  rev            BIGINT       NOT NULL DEFAULT 1,
  updated_at     TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (store_key)
);

CREATE INDEX IF NOT EXISTS transfer_log_caller_idx ON transfer_log (caller, logged_at);
