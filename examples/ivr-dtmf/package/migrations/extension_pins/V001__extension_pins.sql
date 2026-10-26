-- The `extension_pins` store's own table and its example rows. Run on every activation, so
-- idempotent.
--
-- A PROJECTED table (`structure: {schema: urn:ivr, type: ExtensionPin}`), keyed by
-- "<extension>:<pin>" — the flow reads exactly one row: the extension the caller chose and the
-- digits they entered.
--
-- CLEAR-TEXT PINS, DELIBERATELY, FOR THE EXAMPLE: they keep it readable. Do not store real PINs
-- like this — the README describes the production shape (a verification service that keeps
-- salted hashes, called synchronously from the flow).

CREATE TABLE IF NOT EXISTS extension_pins (
  store_key   VARCHAR(512) NOT NULL,
  extension   VARCHAR(10)  NOT NULL,
  pin         VARCHAR(12)  NOT NULL,
  note        VARCHAR(120) NOT NULL,
  active      BOOLEAN      NOT NULL,
  expires_at  TIMESTAMP WITH TIME ZONE,
  rev         BIGINT       NOT NULL DEFAULT 1,
  updated_at  TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (store_key)
);

-- Example data: who else may reach the customers-only extensions 202 (support) and 203
-- (accounts). One PIN is inactive, one has expired.
INSERT INTO extension_pins (store_key, extension, pin, note, active, expires_at) VALUES
  ('202:4321', '202', '4321', 'shared with the partner help desk',    TRUE,  NULL),
  ('202:9999', '202', '9999', 'old contractor PIN, switched off',     FALSE, NULL),
  ('203:2468', '203', '2468', 'given to the auditors for this year',   TRUE,  TIMESTAMP WITH TIME ZONE '2099-12-31 23:59:59+00'),
  ('203:1357', '203', '1357', 'trial partner access, ended',          TRUE,  TIMESTAMP WITH TIME ZONE '2024-01-31 23:59:59+00')
ON CONFLICT (store_key) DO NOTHING;
