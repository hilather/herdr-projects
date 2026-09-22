//! One bounded request, with a single deadline covering connect, write and read.
//! A failure after submission is ambiguous; this transport never retries it.
use super::CAPTURE_LIMIT;
use anyhow::{Context, Result, bail, ensure};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, net::UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn round_trip(path: &Path, line: &str, timeout: Duration) -> Result<String> {
    validate_request(line)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("socket deadline overflow")?;
    check_deadline(deadline)?;
    let mut stream = connect(path, deadline)
        .with_context(|| format!("could not connect to {}", path.display()))?;
    exchange(&mut stream, line, deadline)
}

fn validate_request(line: &str) -> Result<()> {
    ensure!(
        !line.is_empty() && line.len() < CAPTURE_LIMIT,
        "socket request exceeds bounds"
    );
    ensure!(
        !line.contains(['\n', '\r', '\0']),
        "socket request must be one line"
    );
    Ok(())
}

fn check_deadline(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "socket request timed out; submission may be ambiguous"
    );
    Ok(())
}

fn wait(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> Result<()> {
    loop {
        check_deadline(deadline)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let mut fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: the stream owns this descriptor for the duration of poll.
        let ready = unsafe { libc::poll(&mut fd, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        check_deadline(deadline)?;
        if ready > 0 {
            ensure!(
                fd.revents & libc::POLLNVAL == 0,
                "invalid socket descriptor"
            );
            // HUP/ERR are resolved by the next read/write or SO_ERROR check.
            return Ok(());
        }
    }
}

fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is a C structure where zero initializes all fields.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    ensure!(
        !bytes.is_empty() && bytes.len() < address.sun_path.len() && !bytes.contains(&0),
        "invalid Unix socket path"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    // SAFETY: socket returns a new owned descriptor; flags prevent blocking and
    // descriptor leaks through a concurrent exec, including during connect.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    // SAFETY: address contains an initialized, NUL-terminated pathname and the
    // supplied length is within its allocation. stream keeps fd alive.
    let connected = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            length as libc::socklen_t,
        )
    };
    if connected < 0 {
        let error = io::Error::last_os_error();
        // A full Unix listen queue can return EAGAIN without initiating a
        // connection. Refuse it, rather than treating writability as success.
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error.into());
        }
        wait(&stream, libc::POLLOUT, deadline)?;
        if let Some(error) = stream.take_error()? {
            return Err(error.into());
        }
        stream
            .peer_addr()
            .context("socket connection did not complete")?;
    }
    check_deadline(deadline)?;
    Ok(stream)
}

fn exchange(stream: &mut UnixStream, line: &str, deadline: Instant) -> Result<String> {
    validate_request(line)?;
    stream.set_nonblocking(true)?;
    let mut request = Vec::with_capacity(line.len() + 1);
    request.extend_from_slice(line.as_bytes());
    request.push(b'\n');
    let mut written = 0;
    while written < request.len() {
        check_deadline(deadline)?;
        match stream.write(&request[written..]) {
            Ok(0) => bail!("socket closed during request submission"),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait(stream, libc::POLLOUT, deadline)?
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut reply = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        check_deadline(deadline)?;
        match stream.read(&mut buffer) {
            Ok(0) => bail!("socket closed before a complete reply; submission may be ambiguous"),
            Ok(n) => {
                let newline = buffer[..n].iter().position(|byte| *byte == b'\n');
                let keep = newline.map_or(n, |index| index + 1);
                ensure!(
                    reply.len() + keep <= CAPTURE_LIMIT,
                    "socket reply exceeds capture limit; submission may be ambiguous"
                );
                reply.extend_from_slice(&buffer[..keep]);
                if newline.is_some() {
                    check_deadline(deadline)?;
                    return String::from_utf8(reply).context("socket reply is not UTF-8");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait(stream, libc::POLLIN, deadline)?
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn connects_once_to_a_real_path_and_exchanges_one_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut server = loop {
                match listener.accept() {
                    Ok((server, _)) => break server,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "test connection was not established"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("test accept failed: {e}"),
                }
            };
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, "{}\n");
            server.write_all(b"{\"result\":true}\n").unwrap();
        });
        let reply = round_trip(&path, "{}", Duration::from_secs(2));
        thread.join().unwrap();
        assert_eq!(reply.unwrap(), "{\"result\":true}\n");
    }

    fn peer(
        action: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let thread = std::thread::spawn(move || {
            let mut request = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, "{}\n");
            action(server);
        });
        (client, thread)
    }

    #[test]
    fn reply_is_bounded_and_requires_a_complete_utf8_line() {
        for (bytes, expected) in [
            (b"{\"ok\":true}\n".to_vec(), Some("{\"ok\":true}\n")),
            (b"{}".to_vec(), None),
            (vec![], None),
            (vec![255, b'\n'], None),
            (vec![b'x'; CAPTURE_LIMIT + 1], None),
        ] {
            let (mut stream, thread) = peer(move |mut server| {
                let _ = server.write_all(&bytes);
            });
            let result = exchange(&mut stream, "{}", Instant::now() + Duration::from_secs(2));
            match expected {
                Some(value) => assert_eq!(result.unwrap(), value),
                None => assert!(result.is_err()),
            }
            drop(stream);
            thread.join().unwrap();
        }
    }

    #[test]
    fn trickle_reply_cannot_restart_deadline() {
        let (mut stream, thread) = peer(|mut server| {
            for _ in 0..100 {
                if server.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let error = exchange(&mut stream, "{}", start + Duration::from_millis(80)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(800));
        drop(stream);
        thread.join().unwrap();
    }

    #[test]
    fn blocked_submission_obeys_same_deadline() {
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let error = exchange(
            &mut stream,
            &"x".repeat(CAPTURE_LIMIT - 1),
            start + Duration::from_millis(80),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(800));
    }

    #[test]
    fn refuses_invalid_requests_before_connecting() {
        for line in [
            String::new(),
            "{}\n{}".into(),
            "{}\r".into(),
            "\0".into(),
            "x".repeat(CAPTURE_LIMIT),
        ] {
            let error =
                round_trip(Path::new("/missing"), &line, Duration::from_secs(1)).unwrap_err();
            assert!(!error.to_string().contains("connect"));
        }
        assert!(
            round_trip(Path::new("/missing"), "{}", Duration::ZERO)
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }
}
