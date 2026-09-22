PRAGMA application_id = 1129071960;

CREATE TABLE categories (
    name TEXT PRIMARY KEY NOT NULL CHECK(length(trim(name)) > 0),
    kind TEXT NOT NULL CHECK(kind IN ('expense', 'income'))
) STRICT;

INSERT INTO categories(name, kind) VALUES
    ('餐饮', 'expense'), ('交通', 'expense'), ('购物', 'expense'),
    ('住房', 'expense'), ('娱乐', 'expense'), ('医疗', 'expense'),
    ('其他支出', 'expense'), ('工资', 'income'), ('奖金', 'income'),
    ('其他收入', 'income');

CREATE TABLE transactions (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('expense', 'income', 'refund')),
    amount_minor INTEGER NOT NULL CHECK(amount_minor > 0),
    currency TEXT NOT NULL DEFAULT 'CNY' CHECK(currency = 'CNY'),
    category TEXT REFERENCES categories(name),
    date TEXT NOT NULL CHECK(length(date) = 10),
    note TEXT,
    channel TEXT,
    original_id TEXT REFERENCES transactions(id),
    voided INTEGER NOT NULL DEFAULT 0 CHECK(voided IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (
        (kind = 'refund' AND original_id IS NOT NULL AND category IS NULL)
        OR (kind IN ('expense', 'income') AND original_id IS NULL AND category IS NOT NULL)
    ),
    CHECK(original_id IS NULL OR original_id <> id)
) STRICT;

CREATE INDEX transactions_date ON transactions(date, id);
CREATE INDEX transactions_original ON transactions(original_id);
CREATE INDEX transactions_category ON transactions(category);

CREATE TRIGGER refund_original_insert BEFORE INSERT ON transactions
WHEN NEW.kind = 'refund' AND NOT EXISTS (
    SELECT 1 FROM transactions WHERE id = NEW.original_id AND kind = 'expense' AND voided = 0
)
BEGIN
    SELECT RAISE(ABORT, 'refund requires an active expense');
END;

CREATE TRIGGER refund_original_update BEFORE UPDATE OF original_id, kind ON transactions
WHEN NEW.kind = 'refund' AND NOT EXISTS (
    SELECT 1 FROM transactions WHERE id = NEW.original_id AND kind = 'expense' AND voided = 0
)
BEGIN
    SELECT RAISE(ABORT, 'refund requires an active expense');
END;

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY,
    transaction_id TEXT NOT NULL REFERENCES transactions(id),
    action TEXT NOT NULL CHECK(action IN ('create', 'update', 'void')),
    before_json TEXT CHECK(before_json IS NULL OR json_valid(before_json)),
    after_json TEXT NOT NULL CHECK(json_valid(after_json)),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX audit_log_transaction ON audit_log(transaction_id, id);

CREATE TABLE idempotency (
    request_id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    response_json TEXT NOT NULL CHECK(json_valid(response_json)),
    created_at TEXT NOT NULL
) STRICT;
