-- The start time and the end time of a scan change their unit from Unix
-- seconds to Unix milliseconds (docs/design.md sections 3.4 and 3.10). Every
-- stored time is multiplied by 1000. A row without an end time keeps NULL.
UPDATE scans SET started_at = started_at * 1000, finished_at = finished_at * 1000;

-- Whether the scan changed the library. A scan stores the value when it
-- ends. A row that was written at an earlier schema version holds the
-- default 1, because that version did not record what a scan wrote.
ALTER TABLE scans ADD COLUMN changed_library INTEGER NOT NULL DEFAULT 1;

-- The end time of the last scan that changed the library is the last entry
-- of this index among the entries with changed_library = 1.
CREATE INDEX scans_changed ON scans(changed_library, finished_at);

-- No statement reads or writes tracks.has_lyrics. The column is in no index,
-- no constraint, no trigger and no view, so SQLite can remove it. SQLite
-- rewrites each row in place without the column: every row keeps its rowid,
-- its ID and its other values, and no row of another table is written.
ALTER TABLE tracks DROP COLUMN has_lyrics;
