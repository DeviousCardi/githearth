CREATE TABLE IF NOT EXISTS users (
    id BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS memberships (
    tenant BIGINT NOT NULL,
    user_id BIGINT NOT NULL REFERENCES users(id),
    role SMALLINT NOT NULL,
    PRIMARY KEY (tenant, user_id)
);
CREATE TABLE IF NOT EXISTS repositories (
    tenant BIGINT NOT NULL,
    id BIGINT NOT NULL,
    name TEXT NOT NULL,
    policy_version BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (tenant, id),
    UNIQUE (tenant, name)
);
CREATE TABLE IF NOT EXISTS issues (
    tenant BIGINT NOT NULL,
    repo BIGINT NOT NULL,
    id BIGINT NOT NULL,
    state TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    comment_count BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, repo, id),
    FOREIGN KEY (tenant, repo) REFERENCES repositories(tenant, id)
);
CREATE INDEX IF NOT EXISTS issues_list ON issues (tenant, repo, state, id DESC);
CREATE TABLE IF NOT EXISTS comments (
    operation_id TEXT PRIMARY KEY,
    tenant BIGINT NOT NULL,
    repo BIGINT NOT NULL,
    issue BIGINT NOT NULL,
    body TEXT NOT NULL,
    FOREIGN KEY (tenant, repo, issue) REFERENCES issues(tenant, repo, id)
);
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    tenant BIGINT NOT NULL,
    bucket SMALLINT NOT NULL,
    state TEXT NOT NULL,
    claim_id TEXT UNIQUE
);
CREATE INDEX IF NOT EXISTS jobs_ready ON jobs (tenant, bucket, id) WHERE state = 'queued';
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    tenant BIGINT NOT NULL,
    payload TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS probes (
    id TEXT PRIMARY KEY,
    payload TEXT NOT NULL
);
