//! `proxy`: the stdio command a crew bundle pins. It reaches the broker's
//! socket, says who it is, then copies bytes both ways until either side
//! closes. Everything it needs is in argv: the launcher gives it a minimal
//! environment.
use crate::sys;
use serde_json::json;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

pub fn run(socket: &Path, wait: Duration, project: Option<&str>) -> i32 {
    let deadline = Instant::now() + wait;
    let stream = loop {
        match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => {
                eprintln!(
                    "privateer-mcp-broker proxy: no broker at {}: {error}",
                    socket.display()
                );
                return 69;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    // The broker's socket lives in a 0700 directory, but check the peer too.
    match sys::peer_uid(stream.as_raw_fd()) {
        Ok(uid) if uid == sys::euid() => {}
        _ => {
            eprintln!("privateer-mcp-broker proxy: the broker socket is not this user's");
            return 69;
        }
    }
    let cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut hello = json!({"privateer_proxy": 1, "cwd": cwd, "pid": std::process::id()});
    if let Some(project) = project {
        hello["project"] = json!(project);
    }
    let mut upward = stream;
    if writeln!(upward, "{hello}").is_err() {
        eprintln!("privateer-mcp-broker proxy: the broker closed the connection");
        return 69;
    }
    let Ok(mut downward) = upward.try_clone() else {
        return 69;
    };
    std::thread::spawn(move || {
        let _ = copy(&mut io::stdin().lock(), &mut upward);
        // The client is gone: so is this connection, and the broker cancels
        // whatever it left in flight.
        let _ = upward.shutdown(std::net::Shutdown::Both);
        std::process::exit(0);
    });
    let _ = copy(&mut downward, &mut io::stdout().lock());
    0
}

/// Copy until end of input or a failed write, flushing each chunk.
fn copy(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
        to.flush()?;
    }
}
