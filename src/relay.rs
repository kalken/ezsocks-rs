//! Passing data between the client and the upstream connection.
//!
//! On Linux, data is moved with splice() through a pipe per direction, so
//! it never gets copied into this process. Elsewhere, or if pipes can't be
//! created, it is copied through a buffer.

use std::io;

use tokio::net::TcpStream;

const COPY_BUFFER_SIZE: usize = 32 * 1024;

/// Relay until both directions have seen EOF, or either side fails.
/// Returns the number of bytes passed (client to upstream, upstream to
/// client).
pub async fn relay(client: &mut TcpStream, upstream: &mut TcpStream) -> io::Result<(u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        if let (Ok(p1), Ok(p2)) = (splice::Pipe::new(), splice::Pipe::new()) {
            return tokio::try_join!(
                splice::one_way(client, upstream, &p1),
                splice::one_way(upstream, client, &p2),
            );
        }
    }
    tokio::io::copy_bidirectional_with_sizes(client, upstream, COPY_BUFFER_SIZE, COPY_BUFFER_SIZE)
        .await
}

#[cfg(target_os = "linux")]
mod splice {
    use std::io;
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    use tokio::io::Interest;
    use tokio::net::TcpStream;

    /// Upper limit for one splice() call. A pipe holds 64 KiB by default.
    const CHUNK: usize = 64 * 1024;

    pub struct Pipe {
        read: OwnedFd,
        write: OwnedFd,
    }

    impl Pipe {
        pub fn new() -> io::Result<Pipe> {
            let mut fds = [0; 2];
            // SAFETY: pipe2 fills in two new file descriptors on success.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: both descriptors are new and owned by nobody else.
            unsafe {
                Ok(Pipe {
                    read: OwnedFd::from_raw_fd(fds[0]),
                    write: OwnedFd::from_raw_fd(fds[1]),
                })
            }
        }
    }

    fn splice(from: RawFd, to: RawFd, len: usize) -> io::Result<usize> {
        // SAFETY: plain syscall on descriptors that stay open for the call.
        let n = unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                len,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// Move data from `src` to `dst` until EOF, then pass the EOF on.
    ///
    /// The pipe is always emptied before reading more, so a WouldBlock
    /// from either splice() call comes from the socket, never the pipe,
    /// which is what tokio's readiness tracking needs.
    pub async fn one_way(src: &TcpStream, dst: &TcpStream, pipe: &Pipe) -> io::Result<u64> {
        let mut total = 0u64;
        loop {
            let n = loop {
                src.readable().await?;
                match src.try_io(Interest::READABLE, || {
                    splice(src.as_raw_fd(), pipe.write.as_raw_fd(), CHUNK)
                }) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e),
                }
            };
            if n == 0 {
                match socket2::SockRef::from(dst).shutdown(Shutdown::Write) {
                    Err(e) if e.raw_os_error() != Some(libc::ENOTCONN) => return Err(e),
                    _ => return Ok(total),
                }
            }
            let mut left = n;
            while left > 0 {
                dst.writable().await?;
                match dst.try_io(Interest::WRITABLE, || {
                    splice(pipe.read.as_raw_fd(), dst.as_raw_fd(), left)
                }) {
                    Ok(m) => left -= m,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e),
                }
            }
            total += n as u64;
        }
    }
}
