CREATE TABLE installment_plans (
    id TEXT PRIMARY KEY NOT NULL,
    amount_minor INTEGER NOT NULL CHECK(amount_minor > 0),
    fee_minor INTEGER NOT NULL CHECK(fee_minor >= 0),
    periods INTEGER NOT NULL CHECK(periods BETWEEN 2 AND 360 AND amount_minor >= periods),
    date TEXT NOT NULL CHECK(length(date) = 10),
    occurred_at TEXT CHECK(occurred_at IS NULL OR (length(occurred_at) >= 17 AND substr(occurred_at, 1, 10) = date)),
    first_due_date TEXT NOT NULL CHECK(length(first_due_date) = 10 AND first_due_date >= date),
    category TEXT NOT NULL REFERENCES categories(name),
    merchant TEXT,
    note TEXT,
    channel TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'completed', 'cancelled')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE INDEX installment_plans_date ON installment_plans(status, date, id);

CREATE TRIGGER installment_category_insert BEFORE INSERT ON installment_plans
WHEN NOT EXISTS (SELECT 1 FROM categories WHERE name = NEW.category AND kind = 'expense')
BEGIN SELECT RAISE(ABORT, 'installment requires an expense category'); END;

CREATE TABLE installment_periods (
    plan_id TEXT NOT NULL REFERENCES installment_plans(id),
    period INTEGER NOT NULL CHECK(period BETWEEN 1 AND 360),
    due_date TEXT NOT NULL CHECK(length(due_date) = 10),
    principal_minor INTEGER NOT NULL CHECK(principal_minor > 0),
    fee_minor INTEGER NOT NULL CHECK(fee_minor >= 0),
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'confirmed', 'cancelled')),
    transaction_id TEXT UNIQUE REFERENCES transactions(id),
    confirmed_date TEXT CHECK(confirmed_date IS NULL OR length(confirmed_date) = 10),
    confirmed_occurred_at TEXT CHECK(confirmed_occurred_at IS NULL OR (length(confirmed_occurred_at) >= 17 AND substr(confirmed_occurred_at, 1, 10) = confirmed_date)),
    confirmed_at TEXT,
    -- Avoid SQLite promoting an overflowing INTEGER addition to REAL.
    CHECK(principal_minor <= 9223372036854775807 - fee_minor),
    PRIMARY KEY(plan_id, period),
    CHECK (
        (status = 'confirmed' AND transaction_id IS NOT NULL AND confirmed_date IS NOT NULL AND confirmed_at IS NOT NULL)
        OR (status IN ('pending', 'cancelled') AND transaction_id IS NULL AND confirmed_date IS NULL AND confirmed_occurred_at IS NULL AND confirmed_at IS NULL)
    )
) STRICT;

CREATE INDEX installment_periods_due ON installment_periods(status, due_date, plan_id, period);

CREATE TRIGGER installment_period_insert BEFORE INSERT ON installment_periods
WHEN NEW.status <> 'pending' OR NOT EXISTS (SELECT 1 FROM installment_plans WHERE id = NEW.plan_id AND status = 'active' AND NEW.period <= periods AND NEW.due_date >= first_due_date)
BEGIN SELECT RAISE(ABORT, 'period requires an active installment plan'); END;

CREATE TRIGGER installment_confirm_update BEFORE UPDATE ON installment_periods
WHEN NEW.status = 'confirmed' AND NOT EXISTS (
    SELECT 1 FROM transactions t JOIN installment_plans p ON p.id = NEW.plan_id
    WHERE t.id = NEW.transaction_id AND t.kind = 'expense' AND t.voided = 0
        AND t.amount_minor = NEW.principal_minor + NEW.fee_minor AND t.date = NEW.confirmed_date
        AND t.category = p.category AND t.occurred_at IS NEW.confirmed_occurred_at
)
BEGIN SELECT RAISE(ABORT, 'confirmation requires a matching active expense'); END;

CREATE TRIGGER installment_closed_period_immutable BEFORE UPDATE ON installment_periods
WHEN OLD.status <> 'pending'
BEGIN SELECT RAISE(ABORT, 'closed installment period is immutable'); END;

CREATE TRIGGER installment_transaction_kind BEFORE UPDATE OF kind ON transactions
WHEN NEW.kind <> 'expense' AND EXISTS (SELECT 1 FROM installment_periods WHERE transaction_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'linked installment must remain an expense'); END;

CREATE TABLE installment_audit_log (
    id INTEGER PRIMARY KEY,
    plan_id TEXT NOT NULL REFERENCES installment_plans(id),
    action TEXT NOT NULL CHECK(action IN ('create', 'confirm', 'cancel')),
    before_json TEXT CHECK(before_json IS NULL OR json_valid(before_json)),
    after_json TEXT NOT NULL CHECK(json_valid(after_json)),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX installment_audit_plan ON installment_audit_log(plan_id, id);
