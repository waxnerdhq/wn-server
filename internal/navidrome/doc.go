// Package navidrome supervises the pinned Navidrome binary as a child
// process that listens on a Unix socket in the data directory, and owns
// wn-server's own Navidrome identity.
//
// The supervisor checks the binary version, writes navidrome.toml, starts the
// child, waits for /ping, makes Navidrome's libraries match the config, and
// restarts the child with backoff when it exits. NewTransport dials the
// socket for every caller of Navidrome.
package navidrome
