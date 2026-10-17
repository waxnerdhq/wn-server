-- forget_pending is 1 on an entry that was reverted and whose folder beets'
-- incremental history may still list. The next full match run writes the
-- folders of these entries to wn_candidates.forget_folders in beets.yaml,
-- and the wn_candidates plugin removes them from the history. wn-server then
-- clears the flag.
ALTER TABLE match_queue ADD COLUMN forget_pending INTEGER NOT NULL DEFAULT 0;
