-- Track per-harness database name for tenant isolation.
ALTER TABLE harness_instances ADD COLUMN database_name VARCHAR(128);
