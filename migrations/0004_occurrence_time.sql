-- Unknown historical occurrence times remain NULL, including audit/idempotency snapshots.
ALTER TABLE transactions ADD COLUMN occurred_at TEXT
    CHECK(occurred_at IS NULL OR (length(occurred_at) >= 17 AND substr(occurred_at, 1, 10) = date));

ALTER TABLE pending_expenses ADD COLUMN occurred_at TEXT
    CHECK(occurred_at IS NULL OR (length(occurred_at) >= 17 AND substr(occurred_at, 1, 10) = date));
