// Package beets runs `beet import` as a subprocess in copy mode. beets copies
// confidently matched albums into sorted_dir and writes the MusicBrainz ids
// into the copies. The wn_candidates beets plugin records every album and
// its candidates in a JSON lines file, which this package parses. After an
// import, the original folder of every applied album gets an empty
// .ndignore, and Navidrome is asked to scan.
//
// Apply imports one album folder with a release that the owner chose.
// Remove takes an applied album out of the beets library and deletes its
// copy in sorted_dir. RemoveMarker deletes the empty .ndignore again.
package beets
