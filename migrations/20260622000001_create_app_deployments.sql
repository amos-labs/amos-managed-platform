-- Generic multi-service application hosting.
--
-- An app_deployment is a customer application (e.g. Cuspr) made of N services
-- running on a shared per-deployment network. This is distinct from
-- harness_instances (the single AMOS-harness runtime) — the harness path is
-- untouched. The full AppSpec is stored as JSONB so the deployment is
-- self-describing and re-renderable (Docker now, ECS later).

CREATE TABLE IF NOT EXISTS app_deployments (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name        VARCHAR(255) NOT NULL,
    -- The provider-agnostic AppSpec (services, images, env, volumes, ...).
    spec        JSONB NOT NULL,
    -- Where it runs. 'docker' = local bollard; 'ecs' = Fargate (future).
    provider    VARCHAR(20) NOT NULL DEFAULT 'docker'
                CHECK (provider IN ('docker', 'ecs')),
    -- The user-defined Docker network id (docker provider).
    network_id  VARCHAR(128),
    status      VARCHAR(30) NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'provisioning', 'running',
                                  'partial', 'stopped', 'error', 'deprovisioned')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_app_deployments_tenant_id
    ON app_deployments (tenant_id);

CREATE TRIGGER update_app_deployments_updated_at
    BEFORE UPDATE ON app_deployments
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- One row per service container within a deployment.
CREATE TABLE IF NOT EXISTS app_services (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployment_id   UUID NOT NULL REFERENCES app_deployments(id) ON DELETE CASCADE,
    service_name    VARCHAR(128) NOT NULL,
    container_id    VARCHAR(128),
    container_name  VARCHAR(255),
    image           VARCHAR(512),
    expose_public   BOOLEAN NOT NULL DEFAULT FALSE,
    -- Mapped host ports, e.g. [{"container_port":8000,"host_port":54321}].
    host_ports      JSONB NOT NULL DEFAULT '[]',
    status          VARCHAR(30) NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'provisioning', 'running',
                                      'stopped', 'error', 'deprovisioned')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    UNIQUE (deployment_id, service_name)
);

CREATE INDEX IF NOT EXISTS idx_app_services_deployment_id
    ON app_services (deployment_id);

CREATE TRIGGER update_app_services_updated_at
    BEFORE UPDATE ON app_services
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

COMMENT ON TABLE app_deployments IS 'Multi-service customer applications hosted on AMOS (generic; distinct from harness_instances).';
COMMENT ON COLUMN app_deployments.spec IS 'Provider-agnostic AppSpec (see src/provisioning/app_spec.rs).';
COMMENT ON TABLE app_services IS 'Individual service containers within an app_deployment.';
