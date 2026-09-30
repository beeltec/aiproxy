-- Credits of the account, from the usage poll.
ALTER TABLE chatgpt_accounts ADD COLUMN has_credits INTEGER;
ALTER TABLE chatgpt_accounts ADD COLUMN credits_unlimited INTEGER;
ALTER TABLE chatgpt_accounts ADD COLUMN credits_balance TEXT;

-- These go up on every write. A usage poll does not overwrite data that changed during the poll.
ALTER TABLE chatgpt_accounts ADD COLUMN limited_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE chatgpt_quota ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
