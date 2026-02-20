# quelay-agent

The deployable Quelay relay daemon. It bridges local Thrift C2I clients (C++, Rust, Python — anything that speaks Apache Thrift) to a remote Quelay peer over a QUIC link.

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

```
cargo run -qp quelay-agent -- --help
Quelay relay daemon

Usage: quelay-agent [OPTIONS] <COMMAND>

Commands:
  server  Listen for an incoming QUIC connection (satellite / ground station in server role for this session)
  client  Connect to a remote Quelay agent (example: 192.168.1.10:5000)
  help    Print this message or the help of the given subcommand(s)

Options:
      --agent-endpoint <AGENT_ENDPOINT>
          TCP address on which to expose the local Thrift C2I interface.
          Quelay example clients and other local C2I consumers connect here
          [default: 127.0.0.1:9090]
  -h, --help
          Print help
```

```
cargo run -qp quelay-agent -- server --help
Listen for an incoming QUIC connection (satellite / ground station in server role for this session)

Usage: quelay-agent server [OPTIONS]

Options:
      --bind <BIND>  UDP address to bind the QUIC endpoint on [default: 0.0.0.0:5000]
  -h, --help         Print help
```

```
cargo run -qp quelay-agent -- client --help
Connect to a remote Quelay agent (example: 192.168.1.10:5000)

Usage: quelay-agent client [OPTIONS] --peer <PEER> --cert <CERT>

Options:
      --peer <PEER>                UDP address of the remote agent's QUIC endpoint
      --server-name <SERVER_NAME>  TLS server name — must match the name used when the server
                                   generated its cert [default: quelay]
      --cert <CERT>                Path to the server's self-signed cert DER file.
                                   The server writes this at startup; copy it to the client
                                   before launching
  -h, --help                       Print help
```

## TLS / certificate pinning

The server generates a self-signed certificate at startup via `rcgen` and writes the DER-encoded cert to `quelay-server.der` in the working directory. The client pins this cert directly — there is no CA involved. Copy the file out-of-band before starting the client. The server name used during cert generation defaults to `"quelay"`; override with `--server-name` on the client if you generated with a different name.

## Internal structure

```
main.rs          — CLI parsing, QUIC setup, wires the two halves together
config.rs        — Clap structs (Config, Mode)
agent.rs         — Agent: owns QueLaySession, async command loop
thrift_srv.rs    — AgentHandler: sync Thrift handler, enqueues AgentCmds
```

The Thrift `TServer` manages its own pool of OS threads (up to the configured max); this is the synchronous Thrift runtime from the `thrift` crate, not tokio's `spawn_blocking` pool. The session is async and lives on the tokio runtime. Rather than exposing the session across that boundary, the two halves communicate through an `mpsc::channel`. `AgentHandler` holds a `tokio::runtime::Handle` and uses `Handle::block_on` to reach into the tokio runtime from the Thrift threads when it needs to enqueue a command or read shared state. The handler itself is intentionally thin — it validates and converts wire types, sends an `AgentCmd`, and returns. All `QueLaySession` calls happen in `Agent::run`.

```
Thrift OS threads                    tokio runtime
┌─────────────────────┐             ┌──────────────────────┐
│  AgentHandler       │  AgentCmd   │  Agent               │
│  (sync, per-call)   │ ──────────► │  (async, event loop) │
│                     │   mpsc      │                      │
│  maps wire↔domain   │             │  owns QueLaySession  │
│  Handle::block_on   │             │  drives QUIC streams │
└─────────────────────┘             └──────────────────────┘
```

`link_state` is shared as `Arc<Mutex<LinkState>>` so `get_link_state` can read the current value synchronously without crossing the channel.

## Thrift C2I interface

Defined in `quelay-thrift/idl/quelay.idl` (the file uses the `.idl` extension; the Thrift
compiler accepts it via `thrift-compile.sh`; add `// -*- mode: thrift -*-` as the first line
to get syntax highlighting in Emacs once the `thrift` package is installed from MELPA).
The generated service trait is `QueLayAgentSyncHandler`. Methods currently handled:

| Method | Behaviour |
|:-------|:----------|
| `get_version` | Returns the IDL version string from `quelay-thrift`. |
| `stream_start(uuid, info, priority)` | Enqueues a `StreamStart` command; returns queue position. |
| `set_callback(endpoint)` | Registers the callback endpoint for async status pushes. Returns an error string if a callback is already registered — only one active callback is supported; a second call is rejected. |
| `get_link_state` | Returns the current `LinkState` snapshot. |

The callback push path (`QueLayCallback`) is registered via `set_callback` but the outbound Thrift client is not yet wired — status callbacks are a no-op until the session manager iteration lands.

## What's not implemented yet

The session manager layer — reconnection, UUID→stream remapping on reconnect, and spool-to-disk on `LinkState::Failed` — sits between `Agent` and the raw `QueLaySession`. Until it lands, `Agent` calls `open_stream` directly and a lost QUIC connection is fatal. See `docs/contributing/ARCHITECTURE.md` for the intended design.
