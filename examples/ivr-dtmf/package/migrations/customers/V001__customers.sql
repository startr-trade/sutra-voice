-- The `customers` store's own table and its example rows. Run against the store's own
-- connection on every activation, so everything here is idempotent.
--
-- A PROJECTED table (datastores.yaml `structure: {schema: urn:ivr, type: Customer}`): control
-- columns plus one column per field of the Customer complexType, in declared order.
--
--   store_key  -- the <q:store key="..."> value — here the phone number; the PRIMARY KEY
--   rev        -- optimistic-concurrency revision, bumped on every write
--   updated_at -- write timestamp

CREATE TABLE IF NOT EXISTS customers (
  store_key   VARCHAR(512) NOT NULL,
  number      VARCHAR(20)  NOT NULL,
  name        VARCHAR(40)  NOT NULL,
  since       DATE,
  rev         BIGINT       NOT NULL DEFAULT 1,
  updated_at  TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (store_key)
);

-- Example data (fictional numbers). A call from one of these numbers reaches the
-- customers-only extensions (2 and 3) without a PIN.
INSERT INTO customers (store_key, number, name, since) VALUES
  ('+15557654321', '+15557654321', 'Avery Example', DATE '2024-03-01'),
  ('+15550002222', '+15550002222', 'Blake Example', DATE '2025-11-15')
ON CONFLICT (store_key) DO NOTHING;
