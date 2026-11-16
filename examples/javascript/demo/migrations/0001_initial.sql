CREATE TABLE bases (
  slug TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL
);

CREATE TABLE supply_requests (
  request_id TEXT PRIMARY KEY,
  base_slug TEXT NOT NULL,
  item TEXT NOT NULL,
  is_complete INTEGER NOT NULL,
  steps_json TEXT NOT NULL,
  retries_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX idx_requests_base ON supply_requests(base_slug, created_at DESC);
