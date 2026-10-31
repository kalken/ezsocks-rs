# ezsocks-rs

A small SOCKS5 proxy with Happy Eyeballs. Browsers and other programs
connect to it, and it makes their connections and DNS lookups for them,
from wherever it runs.

Run it inside a network namespace with its own VPN, and programs on your
network can send their traffic, DNS included, through that VPN instead
of your own connection.

It is written for low-powered home routers: a single binary of under
1 MB, about 2 MB of memory per instance, and on Linux the data is passed
with `splice()` so it never gets copied through the process.

## What it does

- **SOCKS5 `CONNECT`, no authentication.** TCP connections only, no
  `BIND` or UDP. Anyone who can reach the port can use it, so keep it on
  your LAN and restrict who can reach it with the firewall.
- **Happy Eyeballs** ([RFC 8305](https://www.rfc-editor.org/rfc/rfc8305)):
  IPv6 and IPv4 addresses are looked up in parallel, and connection
  attempts are raced 250 ms apart, preferring IPv6. A broken IPv6 route
  costs a short delay instead of a long timeout. `--family ipv4` skips
  IPv6 completely, for networks without it.
- **DNS cache:** lookups are cached for 60 seconds, and concurrent
  lookups of the same name share one query. Failed lookups are not
  cached.
- **Timeouts:** clients that don't send their request within 30 seconds
  are disconnected, and connecting to the destination gives up after
  30 seconds.
- **TCP keepalive** on both sides, so connections to peers that vanished
  without closing are dropped instead of being kept forever.

## Running it

```sh
cargo build --release
./target/release/ezsocks --listen 0.0.0.0:1080
curl --socks5-hostname 127.0.0.1:1080 https://example.com
```

In Firefox, check "Proxy DNS when using SOCKS v5", so that DNS lookups
go through the proxy too. Other browsers do that by themselves.

To run it inside a network namespace, start it with `ip netns exec`, and
listen on an address that the rest of your network can reach, for
example the namespace's end of a veth pair:

```sh
ip netns exec vpn ezsocks --listen 10.200.1.2:1080
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `-l`, `--listen HOST:PORT` | `127.0.0.1:1080` and `[::1]:1080` | Address or host name to listen on, can be given more than once. A host name listens on all of its addresses; IPv6 addresses go in brackets |
| `-f`, `--family any\|ipv4\|ipv6` | `any` | Address families to connect out with |
| `-t`, `--threads N\|auto` | `1` | Worker threads; `auto` uses one per CPU core |
| `--log-level LEVEL` | `WARNING` | `DEBUG`, `INFO`, `WARNING` or `ERROR`. `INFO` logs every connection with its destination |
| `--dns-cache-ttl SECONDS` | `60` | How long to cache lookups; `0` disables the cache |
| `--handshake-timeout SECONDS` | `30` | Time a client gets to send its request; `0` disables |
| `--connect-timeout SECONDS` | `30` | Time to connect to the destination; `0` disables |
| `--keepalive SECONDS` | `300` | Idle time before TCP keepalive probes; `0` disables |

## Notes

- Each proxied connection uses six file descriptors on Linux (two
  sockets and two pipes), so raise the open-file limit above the usual
  1024 if you expect many connections (`ulimit -n`, or `LimitNOFILE=`
  for a systemd service).
- Builds and runs on Linux and macOS. On macOS (and if pipes can't be
  created) data is copied through a 32 KiB buffer per direction instead
  of `splice()`.

## License

MIT, see [LICENSE](LICENSE). It was inspired by
[ezsocks](https://github.com/kalken/ezsocks) and its upstream
[prettysocks](https://github.com/twisteroidambassador/prettysocks).
