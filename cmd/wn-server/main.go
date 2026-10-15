// Command wn-server is the waxnerd music server: a supervisor and reverse
// proxy around Navidrome with waxnerd login, MusicBrainz matching, and
// rating sync.
package main

import (
	"os"

	"github.com/waxnerdhq/wn-server/internal/cli"
)

func main() {
	if err := cli.Execute(); err != nil {
		os.Exit(1)
	}
}
