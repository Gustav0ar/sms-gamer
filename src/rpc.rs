use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, flock};
use varlink::ConnectionHandler;

const MAX_CLIENTS: usize = 32;
const MAX_REQUEST_BYTES: usize = 65_536;

pub fn default_socket() -> Result<PathBuf> {
    let base =
        env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required; pass --socket")?;
    Ok(PathBuf::from(base).join("sms-gamer/io.smsups.Monitor"))
}

/// Own the socket lock through cleanup, so a second daemon cannot unlink a live socket.
pub struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    _lock: fs::File,
}

impl Endpoint {
    pub fn bind(path: PathBuf) -> Result<Self> {
        ensure!(path.is_absolute(), "The socket path must be absolute");
        let parent = path.parent().context("Socket needs a parent directory")?;
        if !parent.exists() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(PathBuf::from(lock_path))?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .context("Another daemon owns this socket")?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket(),
                    "Refusing to replace a non-socket at {}",
                    path.display()
                );
                match UnixStream::connect(&path) {
                    Ok(_) => bail!("A service is already listening at {}", path.display()),
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        fs::remove_file(&path)?
                    }
                    Err(error) => {
                        return Err(error)
                            .context("Cannot establish whether the existing socket is stale");
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)?;
        let endpoint = Self {
            listener,
            path,
            _lock: lock,
        };
        fs::set_permissions(&endpoint.path, fs::Permissions::from_mode(0o600))?;
        endpoint.listener.set_nonblocking(true)?;
        Ok(endpoint)
    }

    pub fn serve(&self, service: varlink::VarlinkService, stop: Arc<AtomicBool>) -> Result<()> {
        let service = Arc::new(service);
        let clients = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            workers.retain(|worker: &thread::JoinHandle<()>| !worker.is_finished());
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                        drop(stream);
                        continue;
                    }
                    clients.fetch_add(1, Ordering::Relaxed);
                    let service = service.clone();
                    let stop = stop.clone();
                    let clients = clients.clone();
                    workers.push(thread::spawn(move || {
                        if let Err(error) = serve_client(stream, &service, &stop) {
                            // Malformed requests and closed clients must not stop polling.
                            if !stop.load(Ordering::Relaxed) {
                                eprintln!("Varlink connection ended: {error:#}");
                            }
                        }
                        clients.fetch_sub(1, Ordering::Relaxed);
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
        Ok(())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Bound framing and I/O here; the standard Varlink library dispatches generated methods.
fn serve_client(
    mut stream: UnixStream,
    service: &varlink::VarlinkService,
    stop: &AtomicBool,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut frame = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let remaining = MAX_REQUEST_BYTES + 1 - frame.len();
        match reader
            .by_ref()
            .take(remaining as u64)
            .read_until(0, &mut frame)
        {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        ensure!(
            frame.len() <= MAX_REQUEST_BYTES,
            "Varlink request exceeds {MAX_REQUEST_BYTES} bytes"
        );
        if frame.last() != Some(&0) {
            continue;
        }
        let request: varlink::Request<'_> = serde_json::from_slice(&frame[..frame.len() - 1])?;
        ensure!(
            !request.upgrade.unwrap_or(false),
            "Protocol upgrade is not supported"
        );
        let oneway = request.oneway.unwrap_or(false);
        ensure!(
            !(oneway && request.more.unwrap_or(false)),
            "oneway and more cannot be combined"
        );
        let mut input = io::Cursor::new(frame.as_slice());
        let result = if oneway {
            service.handle(&mut input, &mut io::sink(), None)
        } else {
            service.handle(&mut input, &mut stream, None)
        };
        if let Err(error) = result {
            return Err(error.into());
        }
        frame.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_lock_preserves_active_endpoints_and_recovers_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitor");
        let endpoint = Endpoint::bind(path.clone()).unwrap();
        assert!(Endpoint::bind(path.clone()).is_err());
        assert!(UnixStream::connect(&path).is_ok());
        drop(endpoint);
        assert!(!path.exists());
        drop(UnixListener::bind(&path).unwrap());
        assert!(Endpoint::bind(path.clone()).is_ok());
        fs::write(&path, "preserve me").unwrap();
        assert!(Endpoint::bind(path.clone()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "preserve me");
    }
}
