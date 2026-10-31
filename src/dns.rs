//! Host name lookups with a small cache.
//!
//! getaddrinfo() blocks, so lookups run on tokio's blocking thread pool.
//! Concurrent lookups of the same name share one call, and a lookup keeps
//! running even if everybody waiting for it gives up (Happy Eyeballs stops
//! waiting for the other address family once it has connected), so its
//! result still gets cached. Failed lookups are not cached.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

const MAX_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    V4,
    V6,
}

pub type Addrs = Arc<Vec<IpAddr>>;
pub type LookupResult = Result<Addrs, Arc<str>>;

type Key = (String, Family);

pub struct Resolver {
    ttl: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, (Instant, Addrs)>,
    pending: HashMap<Key, watch::Receiver<Option<LookupResult>>>,
}

impl Resolver {
    pub fn new(ttl: Duration) -> Arc<Resolver> {
        Arc::new(Resolver {
            ttl,
            state: Mutex::default(),
        })
    }

    pub async fn lookup(self: &Arc<Self>, name: &str, family: Family) -> LookupResult {
        let key = (name.to_string(), family);
        let mut rx = {
            let mut state = self.state.lock().unwrap();
            match state.entries.get(&key) {
                Some((expires, addrs)) if Instant::now() < *expires => return Ok(addrs.clone()),
                Some(_) => {
                    state.entries.remove(&key);
                }
                None => {}
            }
            match state.pending.get(&key) {
                Some(rx) => rx.clone(),
                None => {
                    let (tx, rx) = watch::channel(None);
                    state.pending.insert(key.clone(), rx.clone());
                    let this = self.clone();
                    tokio::spawn(async move {
                        let lookup_name = key.0.clone();
                        let result =
                            tokio::task::spawn_blocking(move || getaddrinfo(&lookup_name, family))
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()))
                                .map(Arc::new)
                                .map_err(Arc::from);
                        this.finish(key, &result);
                        let _ = tx.send(Some(result));
                    });
                    rx
                }
            }
        };
        let result = match rx.wait_for(Option::is_some).await {
            Ok(result) => result.clone().unwrap(),
            Err(_) => Err(Arc::from("lookup aborted")),
        };
        result
    }

    fn finish(&self, key: Key, result: &LookupResult) {
        let mut state = self.state.lock().unwrap();
        state.pending.remove(&key);
        let Ok(addrs) = result else { return };
        if self.ttl.is_zero() || addrs.is_empty() {
            return;
        }
        let now = Instant::now();
        if state.entries.len() >= MAX_ENTRIES {
            state.entries.retain(|_, (expires, _)| *expires > now);
        }
        if state.entries.len() >= MAX_ENTRIES {
            // Still full of live entries: drop the one closest to expiring.
            if let Some(k) = state
                .entries
                .iter()
                .min_by_key(|(_, (e, _))| *e)
                .map(|(k, _)| k.clone())
            {
                state.entries.remove(&k);
            }
        }
        state.entries.insert(key, (now + self.ttl, addrs.clone()));
    }
}

fn getaddrinfo(name: &str, family: Family) -> Result<Vec<IpAddr>, String> {
    let cname = CString::new(name).map_err(|_| "invalid host name".to_string())?;
    // SAFETY: plain libc calls; the result list is only read before it is
    // freed, and every ai_addr is read according to its ai_family.
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_family = match family {
            Family::V4 => libc::AF_INET,
            Family::V6 => libc::AF_INET6,
        };
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        let rc = libc::getaddrinfo(cname.as_ptr(), std::ptr::null(), &hints, &mut res);
        if rc != 0 {
            if rc == libc::EAI_SYSTEM {
                return Err(std::io::Error::last_os_error().to_string());
            }
            return Err(CStr::from_ptr(libc::gai_strerror(rc))
                .to_string_lossy()
                .into_owned());
        }
        let mut addrs = Vec::new();
        let mut ai = res;
        while !ai.is_null() {
            let a = &*ai;
            let ip = match a.ai_family {
                libc::AF_INET => {
                    let sa = &*(a.ai_addr as *const libc::sockaddr_in);
                    Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr))))
                }
                libc::AF_INET6 => {
                    let sa = &*(a.ai_addr as *const libc::sockaddr_in6);
                    Some(IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr)))
                }
                _ => None,
            };
            if let Some(ip) = ip {
                if !addrs.contains(&ip) {
                    addrs.push(ip);
                }
            }
            ai = a.ai_next;
        }
        libc::freeaddrinfo(res);
        Ok(addrs)
    }
}
