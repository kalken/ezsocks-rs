//! Outgoing connections with Happy Eyeballs (RFC 8305): both address
//! families are looked up in parallel, and connection attempts are raced
//! with a short stagger, preferring IPv6. The first one to connect wins.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{sleep_until, Instant};

use crate::dns::{Family, LookupResult, Resolver};
use crate::socks::{self, Target};

/// RFC 8305 section 3: how long to wait for AAAA results once A results
/// are in.
const RESOLUTION_DELAY: Duration = Duration::from_millis(50);
/// RFC 8305 section 5: how long to wait for one connection attempt before
/// starting the next one in parallel.
const CONNECTION_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FamilyPolicy {
    Any,
    Ipv4Only,
    Ipv6Only,
}

impl FamilyPolicy {
    fn allows(self, family: Family) -> bool {
        match self {
            FamilyPolicy::Any => true,
            FamilyPolicy::Ipv4Only => family == Family::V4,
            FamilyPolicy::Ipv6Only => family == Family::V6,
        }
    }
}

fn family_of(ip: &IpAddr) -> Family {
    match ip {
        IpAddr::V4(_) => Family::V4,
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => Family::V4,
        IpAddr::V6(_) => Family::V6,
    }
}

pub enum ConnectError {
    NotAllowed,
    Timeout,
    Lookup(Vec<Arc<str>>),
    Connect(Vec<io::Error>),
}

impl ConnectError {
    pub fn reply_code(&self) -> u8 {
        match self {
            ConnectError::NotAllowed => socks::REPLY_NOT_ALLOWED,
            ConnectError::Timeout => socks::REPLY_TTL_EXPIRED,
            ConnectError::Lookup(_) => socks::REPLY_HOST_UNREACHABLE,
            ConnectError::Connect(errors) => {
                // The most common specific reason among all attempts.
                let mut counts = [0usize; 9];
                for e in errors {
                    counts[io_error_reply(e) as usize] += 1;
                }
                counts[socks::REPLY_GENERAL_FAILURE as usize] = 0;
                match (0..counts.len()).max_by_key(|&i| (counts[i], std::cmp::Reverse(i))) {
                    Some(i) if counts[i] > 0 => i as u8,
                    _ => socks::REPLY_GENERAL_FAILURE,
                }
            }
        }
    }
}

fn io_error_reply(e: &io::Error) -> u8 {
    match e.raw_os_error() {
        Some(libc::ECONNREFUSED) => socks::REPLY_CONNECTION_REFUSED,
        Some(libc::ENETUNREACH) => socks::REPLY_NETWORK_UNREACHABLE,
        Some(libc::EHOSTUNREACH) => socks::REPLY_HOST_UNREACHABLE,
        Some(libc::ETIMEDOUT) => socks::REPLY_TTL_EXPIRED,
        _ => socks::REPLY_GENERAL_FAILURE,
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConnectError::NotAllowed => write!(f, "address family not allowed"),
            ConnectError::Timeout => write!(f, "timed out"),
            ConnectError::Lookup(errors) => {
                write!(f, "lookup failed: ")?;
                join(f, errors.iter())
            }
            ConnectError::Connect(errors) => join(f, errors.iter()),
        }
    }
}

fn join<T: fmt::Display>(f: &mut fmt::Formatter, items: impl Iterator<Item = T>) -> fmt::Result {
    for (i, item) in items.enumerate() {
        if i > 0 {
            write!(f, "; ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

pub struct Connector {
    pub resolver: Arc<Resolver>,
    pub policy: FamilyPolicy,
    pub timeout: Option<Duration>,
}

impl Connector {
    pub async fn connect(&self, target: &Target) -> Result<TcpStream, ConnectError> {
        let attempt = async {
            match target {
                Target::Addr(addr) => {
                    if !self.policy.allows(family_of(&addr.ip())) {
                        return Err(ConnectError::NotAllowed);
                    }
                    TcpStream::connect(addr)
                        .await
                        .map_err(|e| ConnectError::Connect(vec![e]))
                }
                Target::Name(name, port) => self.happy_eyeballs(name, *port).await,
            }
        };
        match self.timeout {
            Some(t) => tokio::time::timeout(t, attempt)
                .await
                .unwrap_or(Err(ConnectError::Timeout)),
            None => attempt.await,
        }
    }

    async fn happy_eyeballs(&self, name: &str, port: u16) -> Result<TcpStream, ConnectError> {
        let mut lookups: JoinSet<(Family, LookupResult)> = JoinSet::new();
        let mut v6_pending = false;
        for family in [Family::V6, Family::V4] {
            if self.policy.allows(family) {
                let resolver = self.resolver.clone();
                let name = name.to_string();
                lookups.spawn(async move { (family, resolver.lookup(&name, family).await) });
                v6_pending |= family == Family::V6;
            }
        }

        let mut v6: VecDeque<IpAddr> = VecDeque::new();
        let mut v4: VecDeque<IpAddr> = VecDeque::new();
        let mut last_family = Family::V4; // so that IPv6 goes first
        let mut lookup_errors = Vec::new();
        let mut connect_errors = Vec::new();
        let mut attempts: JoinSet<io::Result<TcpStream>> = JoinSet::new();
        // Until this time, don't start trying IPv4 addresses while the IPv6
        // lookup is still running.
        let mut wait_for_v6_until: Option<Instant> = None;
        let mut next_attempt_at = Instant::now();

        loop {
            let now = Instant::now();
            let may_start =
                !v6_pending || !v6.is_empty() || wait_for_v6_until.is_some_and(|t| now >= t);
            if may_start && now >= next_attempt_at {
                // Alternate between families, starting with IPv6.
                let next = match last_family {
                    Family::V4 => v6.pop_front().or_else(|| v4.pop_front()),
                    Family::V6 => v4.pop_front().or_else(|| v6.pop_front()),
                };
                if let Some(ip) = next {
                    last_family = family_of(&ip);
                    attempts.spawn(TcpStream::connect(SocketAddr::new(ip, port)));
                    next_attempt_at = now + CONNECTION_ATTEMPT_DELAY;
                    continue;
                }
            }

            if lookups.is_empty() && attempts.is_empty() && v6.is_empty() && v4.is_empty() {
                return Err(if connect_errors.is_empty() {
                    if lookup_errors.is_empty() {
                        lookup_errors.push(Arc::from("no addresses found"));
                    }
                    ConnectError::Lookup(lookup_errors)
                } else {
                    ConnectError::Connect(connect_errors)
                });
            }

            let have_addrs = !v6.is_empty() || !v4.is_empty();
            let wake = if !have_addrs {
                None
            } else if !may_start {
                wait_for_v6_until
            } else {
                Some(next_attempt_at)
            };

            tokio::select! {
                Some(joined) = lookups.join_next(), if !lookups.is_empty() => {
                    let Ok((family, result)) = joined else { continue };
                    if family == Family::V6 {
                        v6_pending = false;
                    } else if v6_pending {
                        wait_for_v6_until = Some(Instant::now() + RESOLUTION_DELAY);
                    }
                    match result {
                        Ok(addrs) => {
                            let queue = if family == Family::V6 { &mut v6 } else { &mut v4 };
                            queue.extend(addrs.iter().copied());
                        }
                        Err(e) => lookup_errors.push(e),
                    }
                }
                Some(joined) = attempts.join_next(), if !attempts.is_empty() => {
                    match joined {
                        Ok(Ok(stream)) => return Ok(stream),
                        Ok(Err(e)) => {
                            connect_errors.push(e);
                            // RFC 8305 section 5: start the next attempt as
                            // soon as one fails.
                            next_attempt_at = Instant::now();
                        }
                        Err(_) => {}
                    }
                }
                _ = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {}
            }
        }
    }
}
