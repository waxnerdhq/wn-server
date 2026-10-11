# waxnerd-server

Headless, native-built, long-running server meant to grant access to a streamable digital music collection.

## How it works

waxnerd-server runs on your personal computer or on a dedicated server. It relies on local access to your digital media collection.

It can run in a local-only state, making it accessible via the local area network. Any client apps on the same home network can stream music from waxnerd-server. Simple username / password auth by default, with the option to turn off (marked unsafe).

To grant remote access (eg. streaming from outside the home network), waxnerd-server is required to register with waxnerd cloud under an associated waxnerd cloud account (the owner's).

Waxnerd cloud will charge $3.99 monthly for remote mode. The owner pays this cost per server. Guests do not pay for access.

Any client apps that wish to connect to the waxnerd server remotely will do so via waxnerd cloud.

Waxnerd cloud acts as a dumb relay between server and client - it has no knowledge of server contents or client actions outside of establishing connection. It uses TLS to accomplish this; the relay passes encrypted bytes thru to server where TLS terminates. Bitrate is transcoded to lossy (e.g. Opus 192 kbps) over remote stream, lossless when local. Users may configure option to circumvent relay and configure direct connection - in those cases, bitrate can be lossless. Direct connections will still be brokered by the cloud but traffic will not be relayed.

When registered with waxnerd cloud, server owners have the ability to invite users to stream music from their server. The cap for invited users ('guests') is 10 and is meant for friends and family only. Guests cannot upload to the server.

Guests are required to sign in via waxnerd cloud in order to access a waxnerd server as a guest.

Waxnerd mobile will provide apps to allow streaming from waxnerd servers. The servers will be compatible with any OpenSubsonic API client as well (the download endpoint will be disabled).

Guests who choose to use their own OpenSubsonic API compatible client will have the option to generate a username/password login via waxnerd-server UI (via relay) for the servers they have access to. They won't be able to set the password for security reasons (it's used to generate tokens in SubSonic API); it will be randomly generated and shown once at create time.

## Notes

-   Bandwidth costs of waxnerd cloud relay are expected to be surpassed by revenue generously.
