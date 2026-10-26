-- The `analysis` data store's schema: the generic key -> value table the sql store provider
-- reads and writes (rows keyed by the namespace + store name + key; the value is JSON text).
-- Idempotent, run once on first use against the store's own connection.
CREATE TABLE IF NOT EXISTS data_store (
  tenant_id       VARCHAR(64)  NOT NULL,
  module_id       VARCHAR(256) NOT NULL,
  module_version  VARCHAR(64)  NOT NULL,
  store_name      VARCHAR(128) NOT NULL,
  store_key       VARCHAR(512) NOT NULL,
  store_value     TEXT         NOT NULL,
  rev             BIGINT       NOT NULL DEFAULT 1,
  updated_at      TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (tenant_id, module_id, module_version, store_name, store_key)
);
