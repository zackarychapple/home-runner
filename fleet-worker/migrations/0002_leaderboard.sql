-- The fleet is a leaderboard: machines send their current state and totals, never job records.
-- Invite codes are replaced by one ENROLL_KEY secret, and per-job rows are no longer needed.
DROP TABLE IF EXISTS jobs;
DROP TABLE IF EXISTS invites;

-- {"all_time": {...}, "days": {"YYYY-MM-DD": {...}}}; see src/view.ts.
ALTER TABLE machines ADD COLUMN totals TEXT NOT NULL DEFAULT '{}';
