-- Create tables for the nightly bounty generation service
-- These tables support contribution tracking and emission distribution

-- Table: contribution_activities
-- Tracks all contribution activities that earn points
CREATE TABLE IF NOT EXISTS contribution_activities (
    id BIGSERIAL PRIMARY KEY,
    contributor_id BIGINT NOT NULL,
    day_index BIGINT NOT NULL,
    activity_type VARCHAR(50) NOT NULL,
    points BIGINT NOT NULL CHECK (points > 0 AND points <= 2000),
    reference_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for contribution_activities
CREATE INDEX IF NOT EXISTS idx_contribution_activities_day_index ON contribution_activities (day_index);
CREATE INDEX IF NOT EXISTS idx_contribution_activities_contributor_id ON contribution_activities (contributor_id);
CREATE INDEX IF NOT EXISTS idx_contribution_activities_created_at ON contribution_activities (created_at);

-- Table: emission_records
-- Records the daily emission rewards distributed to contributors
CREATE TABLE IF NOT EXISTS emission_records (
    id BIGSERIAL PRIMARY KEY,
    contributor_id BIGINT NOT NULL,
    day_index BIGINT NOT NULL,
    tokens_awarded BIGINT NOT NULL CHECK (tokens_awarded >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Unique constraint: one emission record per contributor per day
    UNIQUE (contributor_id, day_index)
);

-- Indexes for emission_records
CREATE INDEX IF NOT EXISTS idx_emission_records_day_index ON emission_records (day_index);
CREATE INDEX IF NOT EXISTS idx_emission_records_contributor_id ON emission_records (contributor_id);
CREATE INDEX IF NOT EXISTS idx_emission_records_created_at ON emission_records (created_at);

-- Comments for documentation
COMMENT ON TABLE contribution_activities IS 'Tracks all contribution activities that earn points for contributors';
COMMENT ON COLUMN contribution_activities.contributor_id IS 'Unique identifier for the contributor';
COMMENT ON COLUMN contribution_activities.day_index IS 'Day index (0 = genesis day, Jan 1, 2025)';
COMMENT ON COLUMN contribution_activities.activity_type IS 'Type of contribution: BugFix, Feature, Documentation, etc.';
COMMENT ON COLUMN contribution_activities.points IS 'Points earned for this activity (1-2000)';
COMMENT ON COLUMN contribution_activities.reference_id IS 'Optional reference ID (e.g., bounty ID, PR ID)';

COMMENT ON TABLE emission_records IS 'Records daily emission rewards distributed to contributors';
COMMENT ON COLUMN emission_records.contributor_id IS 'Unique identifier for the contributor';
COMMENT ON COLUMN emission_records.day_index IS 'Day index when the emission was distributed';
COMMENT ON COLUMN emission_records.tokens_awarded IS 'Number of AMOS tokens awarded';
