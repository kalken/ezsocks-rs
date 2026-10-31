//! SOCKS5 server side negotiation (RFC 1928), CONNECT without
//! authentication only.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const REPLY_SUCCESS: u8 = 0x00;
pub const REPLY_GENERAL_FAILURE: u8 = 0x01;
pub const REPLY_NOT_ALLOWED: u8 = 0x02;
pub const REPLY_NETWORK_UNREACHABLE: u8 = 0x03;
pub const REPLY_HOST_UNREACHABLE: u8 = 0x04;
pub const REPLY_CONNECTION_REFUSED: u8 = 0x05;
pub const REPLY_TTL_EXPIRED: u8 = 0x06;
pub const REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
pub const REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// Where the client wants to connect to.
pub enum Target {
    Addr(SocketAddr),
    Name(String, u16),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Target::Addr(a) => write!(f, "{a}"),
            Target::Name(n, p) => write!(f, "{n}:{p}"),
        }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Read the client's greeting and CONNECT request and return the target.
/// For requests we don't support, a reply has already been sent.
///
/// Reads straight from the socket without buffering, so any data the
/// client sends right behind its request stays in the socket for the relay.
pub async fn handshake(s: &mut TcpStream) -> io::Result<Target> {
    let mut buf = [0u8; 255];

    s.read_exact(&mut buf[..2]).await?;
    if buf[0] != 5 {
        return Err(invalid("not a SOCKS5 client"));
    }
    let nmethods = buf[1] as usize;
    s.read_exact(&mut buf[..nmethods]).await?;
    if !buf[..nmethods].contains(&0x00) {
        s.write_all(&[5, 0xff]).await?;
        return Err(invalid("client did not offer \"no authentication\""));
    }
    s.write_all(&[5, 0x00]).await?;

    s.read_exact(&mut buf[..4]).await?;
    let (ver, cmd, rsv, atyp) = (buf[0], buf[1], buf[2], buf[3]);
    if ver != 5 || rsv != 0 {
        return Err(invalid("malformed request"));
    }
    let target = match atyp {
        1 => {
            let mut a = [0u8; 6];
            s.read_exact(&mut a).await?;
            let ip = Ipv4Addr::new(a[0], a[1], a[2], a[3]);
            Target::Addr(SocketAddr::new(
                IpAddr::V4(ip),
                u16::from_be_bytes([a[4], a[5]]),
            ))
        }
        4 => {
            let mut a = [0u8; 18];
            s.read_exact(&mut a).await?;
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&a[..16]).unwrap());
            Target::Addr(SocketAddr::new(
                IpAddr::V6(ip),
                u16::from_be_bytes([a[16], a[17]]),
            ))
        }
        3 => {
            let len = s.read_u8().await? as usize;
            s.read_exact(&mut buf[..len]).await?;
            let port = s.read_u16().await?;
            let name = std::str::from_utf8(&buf[..len])
                .map_err(|_| invalid("host name is not valid UTF-8"))?;
            // Clients sometimes send address literals as names (Firefox
            // does with remote DNS on).
            match name.parse::<IpAddr>() {
                Ok(ip) => Target::Addr(SocketAddr::new(ip, port)),
                Err(_) if name.is_empty() || name.contains('\0') => {
                    reply(s, REPLY_HOST_UNREACHABLE, None).await?;
                    return Err(invalid("invalid host name"));
                }
                Err(_) => Target::Name(name.to_string(), port),
            }
        }
        _ => {
            reply(s, REPLY_ADDRESS_TYPE_NOT_SUPPORTED, None).await?;
            return Err(invalid(format!("unsupported address type {atyp}")));
        }
    };
    if cmd != 1 {
        reply(s, REPLY_COMMAND_NOT_SUPPORTED, None).await?;
        return Err(invalid(format!("unsupported command {cmd}")));
    }
    Ok(target)
}

pub async fn reply(s: &mut TcpStream, code: u8, bound: Option<SocketAddr>) -> io::Result<()> {
    let bound = bound.unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
    let mut msg = Vec::with_capacity(22);
    msg.extend_from_slice(&[5, code, 0]);
    match bound.ip() {
        IpAddr::V4(ip) => {
            msg.push(1);
            msg.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            msg.push(4);
            msg.extend_from_slice(&ip.octets());
        }
    }
    msg.extend_from_slice(&bound.port().to_be_bytes());
    s.write_all(&msg).await
}
