// Package ratings records the owner's ratings and favorites. A write goes
// to the local rating store and the outbox. The drain sends the outbox to
// wn-cloud with the server credential, after each write and every minute.
// The package also resolves Navidrome songs and albums to MusicBrainz
// subjects. The reconcile loop against wn-cloud is added in WAX-944.
package ratings
