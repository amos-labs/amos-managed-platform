-- Add ALB routing columns to harness_instances for subdomain-based routing.
-- When a harness task reaches RUNNING, the platform creates a target group
-- and ALB listener rule so it's accessible at {subdomain}.custom.amoslabs.com.

ALTER TABLE harness_instances
    ADD COLUMN IF NOT EXISTS target_group_arn VARCHAR(512),
    ADD COLUMN IF NOT EXISTS listener_rule_arn VARCHAR(512);
