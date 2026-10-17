// Package ratings records the owner's ratings and favorites. A write goes
// to the local rating store and the outbox. The drain sends the outbox to
// wn-cloud with the server credential, after each write and every minute.
// The reconcile loop pulls rating changes from wn-cloud into the local store
// and sets the rounded values in Navidrome, at start and every 15 minutes.
// The package also resolves Navidrome songs and albums to MusicBrainz
// subjects.
package ratings
