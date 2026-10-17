// Package match runs the MusicBrainz matching work for the match queue:
// a full import run, applying a chosen release to an entry, dismissing an
// entry, and reverting an applied entry. The wn-server match commands and
// the web UI's match page both call it. Jobs lets the web UI run one of
// these as a background job.
package match
