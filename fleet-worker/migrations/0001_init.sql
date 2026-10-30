-- A fleet is a group of home-runner machines (possibly in different houses) that report here.
CREATE TABLE fleets (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

CREATE TABLE invites (
  code TEXT PRIMARY KEY,
  fleet_id TEXT NOT NULL REFERENCES fleets(id),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  uses_left INTEGER NOT NULL
);

-- One row per machine. Only a SHA-256 of its token is stored.
CREATE TABLE machines (
  id TEXT PRIMARY KEY,
  fleet_id TEXT NOT NULL REFERENCES fleets(id),
  token_hash TEXT NOT NULL UNIQUE,
  host TEXT NOT NULL,
  house TEXT NOT NULL,
  share TEXT NOT NULL DEFAULT 'repos',
  version TEXT NOT NULL DEFAULT '',
  os TEXT NOT NULL DEFAULT '',
  arch TEXT NOT NULL DEFAULT '',
  cpus INTEGER NOT NULL DEFAULT 0,
  online INTEGER NOT NULL DEFAULT 0,
  joined_at INTEGER NOT NULL,
  last_seen INTEGER NOT NULL DEFAULT 0,
  -- The machine's latest pools and slots (JSON), already cut down to its share level.
  pools TEXT NOT NULL DEFAULT '[]'
);
CREATE UNIQUE INDEX machines_fleet_host ON machines(fleet_id, host);

-- Job records as the machine sent them (already redacted), with the numbers pulled out for totals.
CREATE TABLE jobs (
  machine_id TEXT NOT NULL REFERENCES machines(id),
  id TEXT NOT NULL,
  fleet_id TEXT NOT NULL,
  house TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  duration_secs REAL NOT NULL DEFAULT 0,
  repo TEXT,
  result TEXT,
  saved_vs_github_usd REAL NOT NULL DEFAULT 0,
  saved_vs_depot_usd REAL NOT NULL DEFAULT 0,
  own_usd REAL NOT NULL DEFAULT 0,
  record TEXT NOT NULL,
  PRIMARY KEY (machine_id, id)
);
CREATE INDEX jobs_fleet_started ON jobs(fleet_id, started_at);
