//! ezsocks: a small SOCKS5 proxy (CONNECT, no authentication) with Happy
//! Eyeballs for outgoing connections. Host names are resolved where the
//! proxy runs, so running it in a network namespace sends both the
//! connections and the DNS lookups through that namespace.

mod connect;
mod dns;
mod log;
mod relay;
mod socks;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::{TcpListener, TcpStream};

use connect::{Connector, FamilyPolicy};
use log::Level;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_PROBES: u32 = 4;
const LISTEN_BACKLOG: i32 = 1024;

struct Options {
    listen: Vec<SocketAddr>,
    family: FamilyPolicy,
    threads: usize,
    log_level: Level,
    dns_cache_ttl: Duration,
    handshake_timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    keepalive: Option<Duration>,
}

const USAGE: &str = "\
usage: ezsocks [options]

Small SOCKS5 proxy (CONNECT, no authentication) with Happy Eyeballs.

options:
  -l, --listen HOST:PORT    Address or host name to listen on, can be given
                            more than once. IPv6 addresses go in brackets:
                            [::1]:1080.
                            (default: 127.0.0.1:1080 and [::1]:1080)
  -f, --family any|ipv4|ipv6
                            Address families to connect out with. (default: any)
  -t, --threads N|auto      Worker threads. (default: 1)
  --log-level LEVEL         DEBUG, INFO, WARNING or ERROR. INFO logs every
                            connection with its destination. (default: WARNING)
  --dns-cache-ttl SECONDS   How long to cache host name lookups, 0 disables.
                            (default: 60)
  --handshake-timeout SECONDS
                            Time a client gets to send its request, 0 disables.
                            (default: 30)
  --connect-timeout SECONDS Time to connect upstream, 0 disables. (default: 30)
  --keepalive SECONDS       Idle time before TCP keepalive probes, 0 disables.
                            (default: 300)
  -h, --help                Show this help.
  -V, --version             Show the version.
";

fn parse_args() -> Result<Options, String> {
    let mut o = Options {
        listen: Vec::new(),
        family: FamilyPolicy::Any,
        threads: 1,
        log_level: Level::Warning,
        dns_cache_ttl: Duration::from_secs(60),
        handshake_timeout: Some(Duration::from_secs(30)),
        connect_timeout: Some(Duration::from_secs(30)),
        keepalive: Some(Duration::from_secs(300)),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg, None),
        };
        match flag.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("ezsocks {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
        let value = inline
            .or_else(|| args.next())
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let bad = || format!("invalid value for {flag}: {value:?}");
        match flag.as_str() {
            "-l" | "--listen" => {
                // A host name listens on all of its addresses; the same
                // address given twice (localhost and ::1) is listened on once.
                let addrs = std::net::ToSocketAddrs::to_socket_addrs(value.as_str())
                    .map_err(|e| format!("invalid value for {flag}: {value:?}: {e}"))?;
                for addr in addrs {
                    if !o.listen.contains(&addr) {
                        o.listen.push(addr);
                    }
                }
            }
            "-f" | "--family" => {
                o.family = match value.as_str() {
                    "any" => FamilyPolicy::Any,
                    "ipv4" => FamilyPolicy::Ipv4Only,
                    "ipv6" => FamilyPolicy::Ipv6Only,
                    _ => return Err(bad()),
                }
            }
            "-t" | "--threads" => {
                o.threads = match value.as_str() {
                    "auto" => std::thread::available_parallelism().map_or(1, |n| n.get()),
                    v => v.parse().ok().filter(|&n| n > 0).ok_or_else(bad)?,
                }
            }
            "--log-level" => o.log_level = Level::parse(&value).ok_or_else(bad)?,
            "--dns-cache-ttl" => o.dns_cache_ttl = secs(&value).ok_or_else(bad)?,
            "--handshake-timeout" => o.handshake_timeout = nonzero(secs(&value).ok_or_else(bad)?),
            "--connect-timeout" => o.connect_timeout = nonzero(secs(&value).ok_or_else(bad)?),
            "--keepalive" => o.keepalive = nonzero(secs(&value).ok_or_else(bad)?),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    Ok(o)
}

fn secs(s: &str) -> Option<Duration> {
    s.parse::<f64>()
        .ok()
        .and_then(|v| Duration::try_from_secs_f64(v).ok())
}

fn nonzero(d: Duration) -> Option<Duration> {
    (!d.is_zero()).then_some(d)
}

fn listen(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    sock.set_reuse_address(true)?;
    if addr.is_ipv6() {
        // So that [::]:PORT and 0.0.0.0:PORT can both be listened on.
        sock.set_only_v6(true)?;
    }
    sock.bind(&addr.into())?;
    sock.listen(LISTEN_BACKLOG)?;
    sock.set_nonblocking(true)?;
    TcpListener::from_std(sock.into())
}

fn tune(stream: &TcpStream, keepalive: Option<Duration>) {
    let _ = stream.set_nodelay(true);
    if let Some(idle) = keepalive {
        let ka = TcpKeepalive::new()
            .with_time(idle)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_PROBES);
        let _ = SockRef::from(stream).set_tcp_keepalive(&ka);
    }
}

struct Server {
    connector: Connector,
    handshake_timeout: Option<Duration>,
    keepalive: Option<Duration>,
}

impl Server {
    async fn handle(&self, mut client: TcpStream, peer: SocketAddr) {
        tune(&client, self.keepalive);
        let handshake = socks::handshake(&mut client);
        let target = match self.handshake_timeout {
            Some(t) => match tokio::time::timeout(t, handshake).await {
                Ok(r) => r,
                Err(_) => {
                    info!("handler", "{peer}: no request within {}s", t.as_secs_f64());
                    return;
                }
            },
            None => handshake.await,
        };
        let target = match target {
            Ok(t) => t,
            Err(e) => {
                info!("handler", "{peer}: bad request: {e}");
                return;
            }
        };
        info!("handler", "{peer} -> {target}");
        let mut upstream = match self.connector.connect(&target).await {
            Ok(s) => s,
            Err(e) => {
                info!("handler", "{peer} -> {target}: {e}");
                let _ = socks::reply(&mut client, e.reply_code(), None).await;
                return;
            }
        };
        tune(&upstream, self.keepalive);
        if let Err(e) = socks::reply(
            &mut client,
            socks::REPLY_SUCCESS,
            upstream.local_addr().ok(),
        )
        .await
        {
            info!("handler", "{peer} -> {target}: {e}");
            return;
        }
        match relay::relay(&mut client, &mut upstream).await {
            Ok((up, down)) => debug!(
                "handler",
                "{peer} -> {target}: done, {up} bytes up, {down} down"
            ),
            Err(e) => info!("handler", "{peer} -> {target}: {e}"),
        }
    }
}

async fn serve(listener: TcpListener, server: Arc<Server>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let server = server.clone();
                tokio::spawn(async move { server.handle(stream, peer).await });
            }
            Err(e) => {
                // Usually out of file descriptors: back off instead of
                // spinning until connections close.
                warning!("server", "accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn run(o: Options) -> Result<(), String> {
    let explicit = !o.listen.is_empty();
    let addrs = if explicit {
        o.listen.clone()
    } else {
        vec![
            "127.0.0.1:1080".parse().unwrap(),
            "[::1]:1080".parse().unwrap(),
        ]
    };
    let mut listeners = Vec::new();
    for addr in addrs {
        match listen(addr) {
            Ok(l) => listeners.push(l),
            // The default addresses are a best effort, e.g. a namespace
            // may have IPv6 turned off.
            Err(e) if !explicit => warning!("server", "cannot listen on {addr}: {e}"),
            Err(e) => return Err(format!("cannot listen on {addr}: {e}")),
        }
    }
    if listeners.is_empty() {
        return Err("nothing to listen on".into());
    }

    let server = Arc::new(Server {
        connector: Connector {
            resolver: dns::Resolver::new(o.dns_cache_ttl),
            policy: o.family,
            timeout: o.connect_timeout,
        },
        handshake_timeout: o.handshake_timeout,
        keepalive: o.keepalive,
    });
    for l in listeners {
        if let Ok(a) = l.local_addr() {
            info!("server", "listening on {a}");
        }
        tokio::spawn(serve(l, server.clone()));
    }

    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    tokio::select! {
        _ = term.recv() => info!("server", "received SIGTERM, exiting"),
        _ = int.recv() => info!("server", "received SIGINT, exiting"),
    }
    Ok(())
}

fn main() {
    let o = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ezsocks: {e}\nRun ezsocks --help for options.");
            std::process::exit(2);
        }
    };
    log::set_level(o.log_level);
    let mut builder = if o.threads > 1 {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(o.threads);
        b
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    // Threads for blocking getaddrinfo() calls.
    let runtime = builder
        .enable_all()
        .max_blocking_threads(64)
        .build()
        .expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(o)) {
        error!("server", "{e}");
        std::process::exit(1);
    }
    // Don't wait for connections still being relayed.
    runtime.shutdown_background();
}
