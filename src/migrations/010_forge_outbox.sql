CREATE TABLE IF NOT EXISTS forge_outbox (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    repo TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    head_sha TEXT,
    body TEXT NOT NULL,
    target_url TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Pending',
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER,
    locked_at INTEGER,
    error_log TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id)
);
CREATE INDEX IF NOT EXISTS idx_forge_outbox_status ON forge_outbox(status);
