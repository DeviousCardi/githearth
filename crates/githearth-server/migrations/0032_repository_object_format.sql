-- Record each repository's Git object format so the lifecycle reconciler can
-- redo an interrupted create with the same hash algorithm the client asked for.
ALTER TABLE repositories ADD COLUMN object_format text NOT NULL DEFAULT 'sha1'
  CHECK (object_format IN ('sha1', 'sha256'));
