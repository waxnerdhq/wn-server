// Package navidrome supervises the pinned Navidrome binary as a child
// process bound to 127.0.0.1, and owns wn-server's own Navidrome identity.
//
// The supervisor checks the binary version, writes navidrome.toml, starts the
// child, waits for /ping, makes Navidrome's libraries match the config, and
// restarts the child with backoff when it exits.
package navidrome
