-- Expand the currency CHECK without changing the previously committed migration
-- history. SQLx runs this entire migration in one transaction with foreign keys
-- enabled. Copy the parent and audit child before dropping either original.
CREATE TABLE pending_expenses_new (
    id TEXT PRIMARY KEY NOT NULL,
    currency TEXT NOT NULL CHECK(currency IN ('USD', 'JPY', 'EUR', 'GBP', 'HKD', 'SGD', 'AUD', 'CAD', 'CHF', 'NZD', 'KRW', 'TWD')),
    amount_minor INTEGER NOT NULL CHECK(amount_minor > 0),
    date TEXT NOT NULL CHECK(length(date) = 10),
    category TEXT NOT NULL REFERENCES categories(name),
    merchant TEXT,
    note TEXT,
    channel TEXT,
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'confirmed', 'cancelled')),
    remind_on TEXT NOT NULL CHECK(length(remind_on) = 10 AND remind_on >= date),
    transaction_id TEXT UNIQUE REFERENCES transactions(id),
    confirmed_amount_minor INTEGER CHECK(confirmed_amount_minor > 0),
    confirmed_at TEXT,
    posted_date TEXT CHECK(posted_date IS NULL OR (length(posted_date) = 10 AND posted_date >= date)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (
        (status = 'confirmed' AND transaction_id IS NOT NULL AND confirmed_amount_minor IS NOT NULL AND confirmed_at IS NOT NULL)
        OR (status IN ('pending', 'cancelled') AND transaction_id IS NULL AND confirmed_amount_minor IS NULL AND confirmed_at IS NULL AND posted_date IS NULL)
    )
) STRICT;

INSERT INTO pending_expenses_new (
    id, currency, amount_minor, date, category, merchant, note, channel,
    status, remind_on, transaction_id, confirmed_amount_minor, confirmed_at,
    posted_date, created_at, updated_at
)
SELECT
    id, currency, amount_minor, date, category, merchant, note, channel,
    status, remind_on, transaction_id, confirmed_amount_minor, confirmed_at,
    posted_date, created_at, updated_at
FROM pending_expenses;

CREATE TABLE pending_audit_log_new (
    id INTEGER PRIMARY KEY,
    pending_id TEXT NOT NULL REFERENCES pending_expenses_new(id),
    action TEXT NOT NULL CHECK(action IN ('create', 'confirm', 'cancel', 'snooze')),
    before_json TEXT CHECK(before_json IS NULL OR json_valid(before_json)),
    after_json TEXT NOT NULL CHECK(json_valid(after_json)),
    created_at TEXT NOT NULL
) STRICT;

INSERT INTO pending_audit_log_new (id, pending_id, action, before_json, after_json, created_at)
SELECT id, pending_id, action, before_json, after_json, created_at FROM pending_audit_log;

-- This trigger is attached to transactions and would otherwise refer to an
-- absent table during the drop/rename interval. Parent-attached triggers and
-- indexes are removed by DROP TABLE and recreated below with the same rules.
DROP TRIGGER pending_linked_transaction_kind;
DROP TABLE pending_audit_log;
DROP TABLE pending_expenses;
ALTER TABLE pending_expenses_new RENAME TO pending_expenses;
ALTER TABLE pending_audit_log_new RENAME TO pending_audit_log;

CREATE INDEX pending_expenses_date ON pending_expenses(status, date, id);
CREATE INDEX pending_expenses_due ON pending_expenses(status, remind_on);
CREATE INDEX pending_audit_log_pending ON pending_audit_log(pending_id, id);

CREATE TRIGGER pending_category_insert BEFORE INSERT ON pending_expenses
WHEN NOT EXISTS (SELECT 1 FROM categories WHERE name = NEW.category AND kind = 'expense')
BEGIN SELECT RAISE(ABORT, 'pending expense requires an expense category'); END;

CREATE TRIGGER pending_category_update BEFORE UPDATE OF category ON pending_expenses
WHEN NOT EXISTS (SELECT 1 FROM categories WHERE name = NEW.category AND kind = 'expense')
BEGIN SELECT RAISE(ABORT, 'pending expense requires an expense category'); END;

CREATE TRIGGER pending_transaction_insert BEFORE INSERT ON pending_expenses
WHEN NEW.transaction_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM transactions WHERE id = NEW.transaction_id AND kind = 'expense' AND currency = 'CNY' AND voided = 0
        AND amount_minor = NEW.confirmed_amount_minor AND date = NEW.date
)
BEGIN SELECT RAISE(ABORT, 'confirmation requires an active CNY expense'); END;

CREATE TRIGGER pending_transaction_update BEFORE UPDATE OF transaction_id, status ON pending_expenses
WHEN NEW.transaction_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM transactions WHERE id = NEW.transaction_id AND kind = 'expense' AND currency = 'CNY' AND voided = 0
        AND amount_minor = NEW.confirmed_amount_minor AND date = NEW.date
)
BEGIN SELECT RAISE(ABORT, 'confirmation requires an active CNY expense'); END;

CREATE TRIGGER pending_closed_immutable BEFORE UPDATE ON pending_expenses
WHEN OLD.status <> 'pending'
BEGIN SELECT RAISE(ABORT, 'closed pending expense is immutable'); END;

CREATE TRIGGER pending_linked_transaction_kind BEFORE UPDATE OF kind ON transactions
WHEN NEW.kind <> 'expense' AND EXISTS (SELECT 1 FROM pending_expenses WHERE transaction_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'linked foreign expense must remain an expense'); END;
