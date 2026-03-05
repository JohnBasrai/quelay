# quelay-agent

The deployable Quelay relay daemon. It bridges local Thrift C2I clients (C++,
Rust, Python — anything that speaks Apache Thrift) to a remote Quelay peer over
a QUIC link.

## Quick start

**Server side** (binds the QUIC endpoint, writes its cert to disk):

```bash
quelay-agent server --bind 0.0.0.0:5000
# Writes quelay-server.der to the working directory.
# Thrift C2I becomes available on 127.0.0.1:9090 once a client connects.
```

**Client side** (copy `quelay-server.der` from the server first):

```bash
quelay-agent client --peer 192.168.1.2:5000 --cert ./quelay-server.der
```

Both sides expose a Thrift C2I interface on `127.0.0.1:9090` by
default. Override with `--agent-endpoint`:

```bash
quelay-agent --agent-endpoint 127.0.0.1:9091 server
```

> **Note:** The Thrift server does not start until the QUIC handshake
> completes. Local clients will get connection refused until both ends
> are up. Start the server process first, then the client.

## CLI reference

There is no single flag that prints the full subcommand tree; use `-h` on
each level as needed.

### Top level `quelay-agent` usage message

```
quelay-agent --help
Quelay relay daemon

Usage: quelay-agent [OPTIONS] <COMMAND>

Commands:
  server  Listen for an incoming QUIC connection (satellite ground station or
          server role for this session)
  client  Connect to a remote Quelay agent (example: 192.168.1.10:5000)
  help    Print this message or the help of the given subcommand(s)

Options:
      --agent-endpoint <AGENT_ENDPOINT>
          TCP address on which to expose the local Thrift C2I interface. Quelay
          example clients and other local C2I consumers connect here
          [default: 127.0.0.1:9090]

      --bw-cap-bps <BW_CAP_BPS>
          Uplink bandwidth cap in bits/sec.

          `None` means uncapped (no rate limiting).

          Set via `--bw-cap-bps` on the command line using a value and unit,
          e.g. `--bw-cap-bps 10Mbps`, `--bw-cap-bps 500Kbps`, `--bw-cap-bps 1.5Gbps`.

      --chunk-size-bytes <CHUNK_SIZE_BYTES>
          Chunk payload size in bytes written to the QUIC stream.
          
          Controls spool granularity and ack frequency.  Must be ≤ 65535 (u16
          max — the wire field width).  Defaults to 16 KiB.
          
          The integration test binary sets this to 1024 for
          `small-file-edge-cases` to reproduce the legacy FTA block size and
          exercise multi-block framing boundaries.
          [default: 16384]

      --spool-capacity-bytes <SPOOL_CAPACITY_BYTES>
          In-memory spool capacity per uplink stream in bytes.
          
          The spool absorbs bursts during link outages.  When full the TCP
          reader pauses (back-pressure to the client).  The link-outage test in
          `e2e_test` derives its link-down window from this value. Defaults to 1 MiB.
          [default: 1048576]

  -N, --max-concurrent <MAX_CONCURRENT>
          Maximum concurrent active streams (0 = unlimited).
          
          The DRR test sets this to 1 via the `set_max_concurrent` C2I call so
          that queued streams are reordered by priority before activation. This
          flag sets the startup default; the value can be changed live via
          `set_max_concurrent`.
          [default: 0]

      --max-pending <MAX_PENDING>
          Maximum number of streams allowed in the pending queue (default: 100).
          
          When the pending queue is full, `stream_start` returns
          `queue_position == -1` and an error message.
          [default: 100]

  -h, --help
          Print help (see a summary with '-h')
```

### `quelay-agent` server usage message

```
quelay-agent server --help
Listen for an incoming QUIC connection (satellite ground station or server role
for this session)

Usage: quelay-agent server [OPTIONS]

Options:
      --bind <BIND>  UDP address to bind the QUIC endpoint on
                     [default: 0.0.0.0:5000]
  -h, --help         Print help
```

### `quelay-agent` client usage message

```
quelay-agent client --help
Connect to a remote Quelay agent (example: 192.168.1.10:5000)

Usage: quelay-agent client [OPTIONS] --peer <PEER> --cert <CERT>

Options:
      --peer <PEER>                UDP address of the remote agent's QUIC endpoint
      --server-name <SERVER_NAME>  TLS server name — must match the name used
                                   when the server generated its cert
                                   [default: quelay]
      --cert <CERT>                Path to the server's self-signed cert DER
                                   file. The server writes this at startup; copy
                                   it to the client before launching
  -h, --help                       Print help
```

## TLS / certificate pinning

The server generates a self-signed certificate at startup via `rcgen` and writes
the DER-encoded cert to `quelay-server.der` in the working directory. The client
pins this cert directly — there is no CA involved. Copy the file out-of-band
before starting the client. The server name used during cert generation defaults
to `"quelay"`; override with `--server-name` on the client if you generated with
a different name.

## Internal structure (quelay-agent)

```
main.rs            — CLI parsing, QUIC setup, wires the two halves together
config.rs          — Clap structs (Config, Mode)
agent.rs           — Agent: owns SessionManager, async command loop
thrift_srv.rs      — AgentHandler: sync Thrift handler, enqueues AgentCmds
session_manager.rs — SessionManager: reconnection loop, pending queue, accept loop
active_stream.rs   — Uplink/downlink data pumps, SpoolBuffer (three-pointer A/Q/T)
rate_limiter.rs    — Timer-task-based BW cap; wire_bytes_sent sampling for retransmit accounting
framing.rs         — 8-byte stream-open header, 10-byte chunk header, read/write helpers
callback.rs        — CallbackAgent: async Thrift callback push path
bin/e2e_test/      — Quelay integration test binary
bin/bw-cap-test/   — Bandwidth cap integration test
```

`TServer` is launched via `tokio::task::spawn_blocking`, which hands it to a
blocking thread. From there the synchronous Thrift runtime manages its own
per-connection OS threads. The session is async and lives on the tokio
runtime. Rather than exposing the session across that boundary, the two halves
communicate through an `mpsc::channel`. `AgentHandler` holds a
`tokio::runtime::Handle` and uses `Handle::block_on` to reach into the tokio
runtime from the Thrift threads when it needs to enqueue a command or read
shared state. The handler itself is intentionally thin — it validates and
converts wire types, sends an `AgentCmd`, and returns. All `SessionManager`
calls happen in `Agent::run`.

`TServer` is configured with a maximum of 10 threads, created on demand as C2I
connections arrive — none are pre-spawned at startup. When a connection closes
its thread exits, so under typical single-client usage only one Thrift thread is
live at a time.

```
Thrift OS threads                    tokio runtime
┌─────────────────────┐             ┌──────────────────────┐
│  AgentHandler       │  AgentCmd   │  Agent               │
│  (sync, per-call)   │ ──────────► │  (async, event loop) │
│                     │   mpsc      │                      │
│  maps wire↔domain   │             │  owns SessionManager │
│  Handle::block_on   │             │  drives QUIC streams │
└─────────────────────┘             └──────────────────────┘
```

`link_state` is shared as `Arc<Mutex<LinkState>>` so `get_link_state` can read
the current value synchronously without crossing the channel.

## Thrift C2I interface

Defined in `quelay-thrift/idl/quelay.thrift`. The generated service trait is
`QueLayAgentSyncHandler`. Methods currently handled:

| Method | Behaviour |
|:-------|:----------|
| `get_version`                        | Returns the IDL version string from `quelay-thrift`. |
| `stream_start(uuid, info, priority)` | Start or enqueue a stream. Returns `Running` if fewer than `--max-concurrent` streams are active; `Pending` with queue position if at the limit; `QueueFull` if the pending queue is full. |
| `set_callback(endpoint)`             | Registers the callback endpoint for async status pushes. Returns an error string if a callback is already registered — only one active callback is supported; a second call is rejected. |
| `get_link_state`                     | Returns the current `LinkState` snapshot. |
| `get_bandwidth_cap_bps`              | Returns the configured uplink cap in bits/sec; 0 if uncapped. |


## SessionManager

The `SessionManager` layer sits between `Agent` and the raw `QueLaySession`,
handling reconnection, UUID→stream remapping on reconnect, and pending stream
queuing during outages.

Stream lifecycle management flows through the session layer:

- `stream_start` opens a live QUIC stream immediately when the session is up,
  or queues the request in `pending` when the link is down.
- `stream_start` opens a live QUIC stream immediately when the session is up
  and fewer than `--max-concurrent` streams are active. If at the concurrency
  limit, the request is added to the pending queue. If the session is down,
  the request is queued and replayed on reconnect.
- On reconnect, `restore_active` delivers fresh QUIC streams to all in-flight
  uplink pumps; `drain_pending` re-issues queued streams in arrival order.
- The inbound `accept_loop` sibling task dispatches on the stream-open opcode:
  `OP_NEW_STREAM` spawns a new downlink pump; `OP_RECONNECT` delivers a fresh
  stream to the existing downlink pump for replay and resume.
- `link_enable(bool)` is available in non-production builds for integration
  test injection of link-down/up events without restarting agents.

See `docs/contributing/ARCHITECTURE.md` for the intended recovery model and
design details.

## Network Impairment Testing

`scripts/link-sim-test.sh` runs the link simulation test suite using Docker
Compose. Two `quelay-agent` containers communicate over an isolated `quic-net`
bridge; network impairment is applied by Pumba on that bridge. No host kernel
namespaces, `veth` pairs, or `sudo` required.

### Usage

```
./scripts/link-sim-test.sh <profile> [options]

Profiles:
  loss    Packet loss only
  delay   Latency / jitter only
  rate    Bandwidth cap only (via pumba rate)
  both    Loss + delay combined
  clean   No impairment (baseline sanity check)

Options:
  --loss-percent  N    Packet loss % for 'loss' / 'both' profiles  (default: 5)
  --delay-ms      N    Base delay ms for 'delay' / 'both' profiles (default: 600)
  --jitter-ms     N    Jitter ms for 'delay' / 'both' profiles     (default: 100)
  --rate          STR  Bandwidth for 'rate' profile, e.g. 2mbit    (default: 2mbit)
  --bw-cap        STR  Quelay daemon BW cap, e.g. 10Mbps           (default: 10Mbps)
  --size-mb       N    Payload size per stream in MiB              (default: 100)
  --e2e-args      STR  Override entire e2e command after binary    (default: see below)
  --no-build           Skip --build flag (use cached images)
  -h, --help           Show this help
```

### Examples

```bash
# 5% packet loss, 10 MiB payload
./scripts/link-sim-test.sh loss --size-mb 10

# 600ms delay ±100ms jitter
./scripts/link-sim-test.sh delay --size-mb 10

# Loss + delay combined
./scripts/link-sim-test.sh both --loss-percent 5 --delay-ms 300 --size-mb 10

# 2mbit link cap, agent capped at 1Mbps, 2 MiB payload
./scripts/link-sim-test.sh rate --rate 2mbit --bw-cap 1Mbps --size-mb 2

# Baseline — no impairment
./scripts/link-sim-test.sh clean --size-mb 10
```

### Results (debug build, 10 Mbit/s agent cap, `both` profile)

5% packet loss + 600ms ±100ms jitter. All transfers: 4 × 10 MiB bidirectional,
SHA-256 ✓.

| Profile | Loss | Delay        | Elapsed (s) | BW Util |
|:--------|:-----|:-------------|:------------|:--------|
| `both`  | 5%   | 600ms ±100ms | ~8.2        | ~102%   |

QUIC absorbs all impairment transparently — the token bucket rate limiter
remains the binding constraint.
