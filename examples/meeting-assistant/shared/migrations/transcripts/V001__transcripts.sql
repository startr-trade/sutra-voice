-- The `transcripts` store's own table. Run on every activation, so idempotent.
--
-- A PROJECTED table (`structure: {schema: urn:meeting, type: TranscriptSegment}`), keyed by
-- "<callId>:<seq>": one row per final transcript segment of a call, written by the
-- transcript-segment process as each segment completes.

CREATE TABLE IF NOT EXISTS transcripts (
  store_key       VARCHAR(512)  NOT NULL,
  call_id         VARCHAR(64)   NOT NULL,
  seq             INTEGER       NOT NULL,
  speaker         VARCHAR(40)   NOT NULL,
  speaker_source  VARCHAR(40)   NOT NULL,
  start_ms        INTEGER       NOT NULL,
  end_ms          INTEGER       NOT NULL,
  text            VARCHAR(4000) NOT NULL,
  provider        VARCHAR(40)   NOT NULL,
  model           VARCHAR(80)   NOT NULL,
  recorded_at     TIMESTAMP WITH TIME ZONE NOT NULL,
  rev             BIGINT        NOT NULL DEFAULT 1,
  updated_at      TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (store_key)
);

CREATE INDEX IF NOT EXISTS transcripts_call_idx ON transcripts (call_id, seq);
