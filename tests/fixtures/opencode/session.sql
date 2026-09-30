-- Synthetic OpenCode schema and rows. The test builds an SQLite database from this file
-- rather than checking a binary one in, so the fixture stays reviewable as text.
CREATE TABLE session (
    id TEXT PRIMARY KEY,
    directory TEXT NOT NULL,
    parent_id TEXT,
    version TEXT,
    model TEXT
);
CREATE TABLE session_message (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    type TEXT NOT NULL,
    time_created INTEGER NOT NULL,
    data TEXT NOT NULL
);
-- The legacy mirror, which is read only for sessions `session_message` does not cover.
CREATE TABLE message (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL,
    data TEXT NOT NULL
);

INSERT INTO session VALUES
    ('current', '/home/example/project', NULL, '1.2.0', '{"providerID":"anthropic","modelID":"model-a"}'),
    ('delegated', '/home/example/project', 'current', '1.2.0', '{"providerID":"anthropic","modelID":"model-a"}'),
    ('legacy', '/home/example/other', NULL, '0.9.0', NULL);

-- 2026-01-01T15:00:00Z is 1767279600000 ms.
INSERT INTO session_message VALUES
    ('m1', 'current', 'user', 1767279600000, '{}'),
    ('m2', 'current', 'assistant', 1767279620000, '{"providerID":"anthropic","modelID":"model-b"}'),
    ('m3', 'current', 'user', 1767279900000, '{}'),
    ('m4', 'delegated', 'user', 1767279630000, '{}');

INSERT INTO message VALUES
    ('l1', 'legacy', 1767283200000, '{"role":"user"}'),
    ('l2', 'legacy', 1767283230000, '{"role":"assistant","providerID":"openai","modelID":"model-c"}');
