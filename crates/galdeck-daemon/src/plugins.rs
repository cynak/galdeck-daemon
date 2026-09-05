//! Running plugins.
//!
//! Each plugin is a child process the daemon starts, speaking line-delimited
//! JSON over its stdin and stdout. Every byte of that conversation happens on
//! threads belonging to the plugin — a reader, a writer and a drain for its
//! stderr — and the engine only ever does a non-blocking send or receive. A
//! plugin that stops reading, floods its output, or wedges entirely cannot
//! slow the deck down, because there is no point at which the engine waits for
//! one.
//!
//! That is the whole reason plugins are processes rather than something
//! loaded into this one. The thread that owns the HID handle also owns the
//! 500 ms keepalive, and a plugin that blocked it would make the firmware drop
//! out of software mode and wipe what the panel is showing.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use galdeck_core::Waker;
use galdeck_plugin::{decode, encode, FromPlugin, Manifest, ToPlugin, MAX_LINE_BYTES};

/// How long to wait for a plugin to exit after being asked, before killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// Messages that may be queued towards one plugin before we start dropping.
///
/// A plugin that has stopped reading its stdin must not be able to make the
/// daemon buffer without limit, and every message here is a hint about current
/// state rather than a record that must survive.
const TO_PLUGIN_QUEUE: usize = 32;
/// Messages that may be queued from all plugins towards the engine.
const FROM_PLUGIN_QUEUE: usize = 128;
/// First restart delay after a plugin exits, doubling to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Something a plugin said, tagged with which one said it.
#[derive(Debug, Clone)]
pub struct PluginEvent {
    pub plugin: String,
    pub message: FromPlugin,
}

/// One running plugin.
struct Running {
    child: Child,
    outbox: SyncSender<ToPlugin>,
}

/// A plugin the daemon knows about.
pub struct PluginEntry {
    pub id: String,
    pub manifest: Manifest,
    dir: PathBuf,
    running: Option<Running>,
}

impl PluginEntry {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }
}

/// Every plugin, and the channel their messages arrive on.
pub struct PluginHost {
    plugins: BTreeMap<String, PluginEntry>,
    events: SyncSender<PluginEvent>,
    waker: Waker,
}

impl PluginHost {
    /// Discover the plugins under a config directory.
    ///
    /// Nothing is started here; starting happens on demand, so a plugin nobody
    /// has bound a key to costs nothing at all.
    pub fn discover(config_dir: &Path, waker: Waker) -> (Self, Receiver<PluginEvent>) {
        let (events_tx, events_rx) = sync_channel(FROM_PLUGIN_QUEUE);
        let mut plugins = BTreeMap::new();

        let root = config_dir.join("plugins");
        if let Ok(entries) = std::fs::read_dir(&root) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            // Sorted so startup order is the same every time.
            dirs.sort();

            for dir in dirs {
                let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let manifest_path = dir.join("plugin.toml");
                let text = match std::fs::read_to_string(&manifest_path) {
                    Ok(text) => text,
                    Err(_) => continue,
                };
                match toml::from_str::<Manifest>(&text) {
                    Ok(manifest) => {
                        log::info!("found plugin {id:?} ({})", manifest.name);
                        plugins.insert(
                            id.to_string(),
                            PluginEntry {
                                id: id.to_string(),
                                manifest,
                                dir: dir.clone(),
                                running: None,
                            },
                        );
                    }
                    Err(e) => log::warn!("{}: {e}", manifest_path.display()),
                }
            }
        }

        (
            Self {
                plugins,
                events: events_tx,
                waker,
            },
            events_rx,
        )
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.plugins.keys().map(String::as_str)
    }

    pub fn get(&self, id: &str) -> Option<&PluginEntry> {
        self.plugins.get(id)
    }

    pub fn known(&self, id: &str) -> bool {
        self.plugins.contains_key(id)
    }

    /// Start a plugin if it is not already running.
    pub fn ensure_started(&mut self, id: &str) {
        let Some(entry) = self.plugins.get_mut(id) else {
            return;
        };
        if entry.running.is_some() {
            return;
        }
        match spawn(entry, self.events.clone(), self.waker.clone()) {
            Ok(running) => {
                log::info!("started plugin {id:?}");
                entry.running = Some(running);
            }
            Err(e) => log::warn!("starting plugin {id:?}: {e}"),
        }
    }

    /// Send a message to a plugin, starting it if necessary.
    ///
    /// Never blocks. A full queue means the plugin has stopped reading, and
    /// every message here describes current state rather than a record that
    /// must survive, so dropping is the right answer.
    pub fn send(&mut self, id: &str, message: ToPlugin) {
        self.ensure_started(id);
        let Some(entry) = self.plugins.get_mut(id) else {
            return;
        };
        let Some(running) = entry.running.as_ref() else {
            return;
        };
        match running.outbox.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                log::debug!("plugin {id:?} is not keeping up; dropping a message")
            }
            Err(TrySendError::Disconnected(_)) => {
                // Its writer thread is gone, so the process is on its way out.
                entry.running = None;
            }
        }
    }

    /// Reap plugins that have exited, so they can be started again.
    ///
    /// Called from the engine's loop, which is why it must not block: `try_wait`
    /// asks without waiting.
    pub fn reap(&mut self) {
        for entry in self.plugins.values_mut() {
            let Some(running) = entry.running.as_mut() else {
                continue;
            };
            match running.child.try_wait() {
                Ok(Some(status)) => {
                    log::warn!("plugin {:?} exited ({status})", entry.id);
                    entry.running = None;
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("waiting on plugin {:?}: {e}", entry.id);
                    entry.running = None;
                }
            }
        }
    }

    /// Ask every plugin to stop, then make sure of it.
    pub fn shutdown(&mut self) {
        for entry in self.plugins.values_mut() {
            let Some(running) = entry.running.as_mut() else {
                continue;
            };
            let _ = running.outbox.try_send(ToPlugin::Shutdown);

            // Asked politely, then not. A plugin that ignores the request must
            // not be able to keep the daemon from exiting.
            let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
            loop {
                match running.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() >= deadline => {
                        log::warn!("plugin {:?} did not stop; killing it", entry.id);
                        let _ = running.child.kill();
                        let _ = running.child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            entry.running = None;
        }
    }
}

fn spawn(
    entry: &PluginEntry,
    events: SyncSender<PluginEvent>,
    waker: Waker,
) -> std::io::Result<Running> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&entry.manifest.command)
        // From its own directory, so a plugin can ship files beside itself and
        // refer to them by a relative path.
        .current_dir(&entry.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let id = entry.id.clone();
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Writer. Owning stdin here is what keeps the engine from ever blocking on
    // a plugin that has stopped reading.
    let (outbox, to_write) = sync_channel::<ToPlugin>(TO_PLUGIN_QUEUE);
    let writer_id = id.clone();
    std::thread::Builder::new()
        .name(format!("galdeck-plugin-{id}-in"))
        .spawn(move || {
            for message in to_write {
                let Ok(line) = encode(&message) else { continue };
                if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
                    log::debug!("plugin {writer_id:?} closed its input");
                    return;
                }
            }
        })?;

    // Reader.
    let reader_id = id.clone();
    std::thread::Builder::new()
        .name(format!("galdeck-plugin-{id}-out"))
        .spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.len() > MAX_LINE_BYTES {
                    log::warn!("plugin {reader_id:?} sent an oversized line; ignoring it");
                    continue;
                }
                match decode::<FromPlugin>(&line) {
                    Ok(message) => {
                        let event = PluginEvent {
                            plugin: reader_id.clone(),
                            message,
                        };
                        // A full queue means the engine is behind; these are
                        // state updates, so the next one supersedes this.
                        if events.try_send(event).is_ok() {
                            waker.notify();
                        }
                    }
                    Err(e) => log::debug!("plugin {reader_id:?} sent something unreadable: {e}"),
                }
            }
        })?;

    // Its stderr goes to the log, tagged, rather than to the daemon's own.
    let stderr_id = id.clone();
    std::thread::Builder::new()
        .name(format!("galdeck-plugin-{id}-err"))
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log::warn!("[{stderr_id}] {line}");
            }
        })?;

    let hello = ToPlugin::Hello {
        protocol: galdeck_plugin::PROTOCOL_VERSION,
        plugin: id,
    };
    let _ = outbox.try_send(hello);

    Ok(Running { child, outbox })
}

/// How long to wait before restarting a plugin that has exited `failures`
/// times in a row.
///
/// A plugin that crashes immediately and forever should not become a fork
/// bomb, so the wait doubles up to a minute.
pub fn backoff(failures: u32) -> Duration {
    let doubled = FIRST_BACKOFF.saturating_mul(1u32 << failures.min(6));
    doubled.min(MAX_BACKOFF)
}
