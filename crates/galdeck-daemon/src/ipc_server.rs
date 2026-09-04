//! Unix-socket control server: line-delimited JSON requests, answered by
//! the engine thread.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use galdeck_ipc::{Request, Response};

use crate::engine::ControlMsg;

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind the control socket, replacing a stale one from a dead daemon.
pub fn bind(path: &Path) -> Result<UnixListener> {
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
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
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
