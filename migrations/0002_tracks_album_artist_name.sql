-- The album artist of the file of a track, in the spelling of the file
-- (docs/design.md sections 1.3 and 3.3). NULL in a row that was written at
-- schema version 1; a scan sets the value when it reads the file.
ALTER TABLE tracks ADD COLUMN album_artist_name TEXT;
