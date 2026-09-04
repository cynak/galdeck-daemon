//! Unix-socket control server: line-delimited JSON requests, answered by
//! the engine thread.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use galdeck_ipc::{Request, Response};

use crate::engine::ControlMsg;

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind the control socket, replacing a stale one from a dead daemon.
///
/// This socket can already switch pages and set brightness, and is about to be
/// able to define what commands a key runs -- so it is bound 0600 and, when it
/// lives in the `/tmp` fallback, inside a directory this user owns.
pub fn bind(path: &Path) -> Result<UnixListener> {
    // Only our own /tmp fallback directory is ours to police. An explicitly
    // configured XDG_RUNTIME_DIR belongs to whoever set it.
    if let Some(parent) = path.parent() {
        if parent == galdeck_ipc::fallback_dir() {
            ensure_private_dir(parent)?;
        }
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!(
                "another galdeck-daemon is already listening on {}",
                path.display()
            );
        }
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }

    // Bind to a private name and rename it into place. Binding directly would
    // leave a window in which the socket exists at its well-known path with
    // whatever the umask happened to allow.
    let staging = path.with_extension(format!("sock.{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    let listener =
        UnixListener::bind(&staging).with_context(|| format!("binding {}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", staging.display()))?;
    std::fs::rename(&staging, path)
        .with_context(|| format!("moving the socket into place at {}", path.display()))?;
    Ok(listener)
}

/// Make sure `dir` exists, is a real directory this user owns, and is not
/// readable by anyone else.
///
/// `/tmp` is world-writable and sticky, so without this another account could
/// pre-create the directory -- or a symlink standing in for it -- and receive
/// the connections meant for this daemon.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(e).with_context(|| format!("creating {}", dir.display()));
        }
    }

    // It was already there. Trust it only if it is exactly what we would have
    // created: symlink_metadata so a symlink is not silently followed.
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} exists but is not a directory", dir.display());
    }
    // Safety: getuid cannot fail and touches no memory.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        bail!(
            "{} is owned by uid {}, not {uid} — refusing to use it",
            dir.display(),
            meta.uid()
        );
    }
    if meta.mode() & 0o077 != 0 {
        bail!(
            "{} is accessible to other users (mode {:o}) — refusing to use it",
            dir.display(),
            meta.mode() & 0o7777
        );
    }
    Ok(())
}

/// Accept connections forever, forwarding requests to the engine.
pub fn serve(listener: UnixListener, control_tx: Sender<ControlMsg>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let tx = control_tx.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, tx) {
                        log::debug!("client connection ended: {e:#}");
                    }
                });
            }
            Err(e) => {
                // A transient accept error (EMFILE, ECONNABORTED) must not
                // permanently kill the control socket.
                log::warn!("accept failed: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

fn handle_client(stream: UnixStream, control_tx: Sender<ControlMsg>) -> Result<()> {
    // Don't let an idle client pin its handler thread forever.
    stream.set_read_timeout(Some(CLIENT_IDLE_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply_tx, reply_rx) = channel();
                if control_tx
                    .send(ControlMsg {
                        request,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    Response::Error {
                        message: "daemon is shutting down".into(),
                    }
                } else {
                    reply_rx
                        .recv_timeout(REPLY_TIMEOUT)
                        .unwrap_or(Response::Error {
                            message: "daemon did not respond in time".into(),
                        })
                }
            }
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        let mut payload = serde_json::to_string(&response)?;
        payload.push('\n');
        writer.write_all(payload.as_bytes())?;
    }
    Ok(())
}
