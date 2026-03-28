-- Allow users to name their harness instances for easy identification.
ALTER TABLE harness_instances
    ADD COLUMN IF NOT EXISTS name VARCHAR(100);
