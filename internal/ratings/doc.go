// Package ratings records the owner's ratings and favorites. It writes the
// local rating store, then wn-cloud with the server credential, or the
// outbox when wn-cloud is unreachable. It drains the outbox in the
// background and resolves Navidrome songs and albums to MusicBrainz
// subjects. The reconcile loop against wn-cloud is added in WAX-944.
package ratings
