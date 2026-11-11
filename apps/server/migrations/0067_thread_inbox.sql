-- "Mark all threads read" receipts (docs/protocol/THREADS.md): a replay answers
-- the first result and must not read replies that arrived since.
CREATE TABLE thread_read_all_commands (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL,
    threads BIGINT NOT NULL CHECK(threads>=0),
    applied_revision BIGINT NOT NULL CHECK(applied_revision>=0),
    PRIMARY KEY(user_id,operation_id)
);
