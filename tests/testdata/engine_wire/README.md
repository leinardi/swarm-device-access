# Engine wire captures

What the Go daemon and launcher (1.x) send the Docker Engine, and what a real Docker daemon
answers, recorded on the wire. The Rust Docker client and launcher engine are checked against
these: their fake engine replays the answers, and their tests compare the requests.

Each `api-<version>/` directory holds the captures in which moby client v0.6.0 negotiated that
API version, the lower of the daemon's newest and its own: `/v<version>/` in every path.

| File | Captured by | What it records |
| --- | --- | --- |
| `daemon.jsonl` | `TestCaptureDaemonWire` | The daemon in dry-run (`-dry-run -log-format=json -log-level=debug`): the startup ping, the event subscription with its `since` and `event` filters, the enumeration (list and inspect) of a running opted-in container, the inspects that a start and an unpause event cause, a cut of the event stream with the resubscription since the last event plus one nanosecond and the re-enumeration, then a die and a destroy event, which cause no request (the capture waits until the stream has delivered both) |
| `daemon-ping-fallback.jsonl` | `TestCaptureDaemonPingFallback` | The daemon's startup up to its subscription against an Engine that refuses `HEAD /_ping` with 405 (answered by the proxy, marked `synthetic`), so moby falls back to `GET /_ping` and its `OK` body, for the negotiation and for the subscription's ping |
| `launcher.jsonl` | `TestCaptureLauncherWire` | `swarm-device-access launch -log-format=json -- sh -c <script>`: the launcher's inspect of its own container, the list filtered by the daemon's name with an orphaned daemon in it, the inspect of the orphan's missing launcher (404), the forced remove, the create with the daemon spec, the attach upgrade with a stdout and a stderr frame, the wait for removal (before the start), the start, and on `SIGTERM` the stop with `t=10` |

## Format

JSON Lines, one exchange per line, in the order the requests arrived over all connections:

- `seq`, `conn`: the arrival order, and which client connection the request came on (moby keeps
  connections alive, and streams get their own);
- `method`, `uri`, `host`: the request line and `Host` header as sent;
- `header`, `transferEncoding`, `body`: the other request headers (keys as Go canonicalizes
  them, each value list in the order the lines came), and the body;
- `status`, `responseHeader`, `responseTransferEncoding`: the response;
- `responseBody` (UTF-8) or `responseBodyBase64`: the body the client received;
- `upgraded`, `upgradedFromServerBase64`, `upgradedFromClientBase64`: for the attach's
  101, the raw bytes each side sent on the hijacked connection, stdcopy frames from the server;
- `cut`: the capture closed the connection while the response streamed;
- `synthetic`: the proxy answered itself instead of forwarding.

## The fake inputs

The files name no other container of the host, no user and no checkout path, and the one
privileged container runs nothing but a shell script:

- The daemon capture narrows two answers to the containers it creates: the container list and the
  event stream. The host's other containers would otherwise appear in the files and cause
  inspects of their own. Every request is the daemon's own.
- The launcher runs the real binary in a `busybox:1.36` container (pinned by digest) whose
  `/var/run/docker.sock` is the proxy, so it identifies itself from its mount table as in
  production. It creates the daemon container from that image with the real spec, so that
  container is privileged, in the host's PID, user and cgroup namespaces, with the host's `/dev`,
  `/sys` and Docker socket bound in; the command after `--` is a shell script that prints a line
  on each stream and exits on `SIGTERM`, not the daemon. The orphan it removes is a busybox
  container named `swarm-device-access`, labelled as a daemon whose launcher (`000…0`) does not
  exist.

IDs, times, `Date` headers and the `since` values change with every run, so nothing checks these
files against Go; they are recaptured by hand.

## Recapturing

Needs a Docker daemon on `/var/run/docker.sock` that the user can reach, no container named
`swarm-device-access` (the capture refuses to run otherwise, since the launcher would remove it),
and network access the first time, to pull `busybox:1.36`:

```bash
make wire-capture
```

It builds the binary (`make go-build`), runs `go test -tags=wirecapture -run '^TestCapture'
./internal/wirecapture/` with `SDA_WIRE_BINARY` pointing at it, and rewrites the files of the
local daemon's API version. The recording proxy and the harness are `internal/wirecapture`, built
only under that tag. A killed run can leave containers behind; each carries the label
`swarm-device-access-wirecapture`:

```bash
docker ps -aq --filter label=swarm-device-access-wirecapture | xargs -r docker rm -f
```

When the negotiated version changes, the files land in a new directory: delete the old one,
unless the Rust tests are to keep replaying both. Review the diff before committing: a new
request, a changed header or a reordered sequence is a change of the Go client's behaviour that
the Rust port has to follow.
