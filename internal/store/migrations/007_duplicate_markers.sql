-- The release that a duplicate entry duplicates: the release id on the
-- duplicate line, which is the release that beets chose for the folder. It is
-- NULL for an entry that has no duplicate line with a release id. wn-server
-- hides a duplicate folder with a .ndignore when an applied entry has this
-- release id as its applied_release_id. marker_written is then 1 for the
-- duplicate entry too, and a revert of the applied entry removes the marker
-- again.
ALTER TABLE match_queue ADD COLUMN duplicate_of_release_id TEXT NULL;
