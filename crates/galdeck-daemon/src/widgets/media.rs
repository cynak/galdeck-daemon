//! What is playing, over MPRIS, and telling it what to do.
//!
//! MPRIS is the D-Bus interface every Linux media player worth mentioning
//! speaks — Spotify, browsers, mpv, VLC, Rhythmbox — so one client covers them
//! all without knowing any of them by name.
//!
//! Polled rather than subscribed to. `Position` is the one property a
//! progress bar needs and the one MPRIS deliberately never announces, so a
//! subscriber would have to poll it anyway; polling everything keeps one code
//! path and no signal plumbing, at the cost of three small calls a second.
//!
//! The widget and the knob choose a player the same way, through [`pick`] and
//! one shared [`sticky`] name, so the knob acts on what the card shows.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// How long any one D-Bus call may take.
///
/// zbus waits for a reply forever unless told otherwise, and a player that
/// has hung — a browser stuck on a tab, say — would freeze whichever thread
/// asked it: the card's progress bar, or every knob turn queued behind it.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const MICROS_PER_SECOND: i64 = 1_000_000;
/// The furthest one seek goes either way, in microseconds.
///
/// Ten minutes: already more than any sensible step times a fast spin, and it
/// keeps a runaway number from reaching a player that would act on it.
const MAX_SEEK: i64 = 600 * MICROS_PER_SECOND;
/// Largest cover image worth downloading.
///
/// Spotify's are 640px JPEGs of around 100K; anything this size is not a
/// cover, and would be decoded only to be shrunk to a few hundred pixels.
const MAX_ART_BYTES: u64 = 4 * 1024 * 1024;
/// Covers are kept at most this big; nothing on the deck draws one larger.
const ART_PIXELS: u32 = 320;
const ART_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

impl Status {
    /// Whatever is playing, then whatever is paused, then anything: a browser
    /// tab left paused should not hide Spotify actually playing.
    fn rank(self) -> u8 {
        match self {
            Status::Playing => 0,
            Status::Paused => 1,
            Status::Stopped => 2,
        }
    }
}

/// A cover image. Compared by where it came from, so a track change repaints
/// and a position change does not re-compare a few hundred kilobytes.
#[derive(Clone, Debug)]
pub struct Art {
    pub url: String,
    pub image: Arc<image::RgbaImage>,
}

impl PartialEq for Art {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    /// The player's bus name without the MPRIS prefix: `spotify`.
    pub player: String,
    pub status: Status,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub position: Option<Duration>,
    pub length: Option<Duration>,
    pub art: Option<Art>,
}

impl Media {
    /// How far through the track, 0 to 1, when that is known.
    pub fn progress(&self) -> Option<f32> {
        let (position, length) = (self.position?, self.length?);
        (!length.is_zero()).then(|| (position.as_secs_f32() / length.as_secs_f32()).clamp(0.0, 1.0))
    }
}

/// `2:05`, or `1:02:05` past the hour.
pub fn timestamp(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The player last seen playing or last sent a command, by bus name.
///
/// Process-wide because two threads need the same answer: the media worker
/// drawing the card and the controls worker acting on the knob. With one
/// each, a paused Spotify and a paused browser tab would tie, the two could
/// break the tie differently, and pressing play would resume something other
/// than what the card shows.
static STICKY: Mutex<Option<String>> = Mutex::new(None);

/// Make `bus_name` the player that wins ties from now on.
pub fn remember(bus_name: &str) {
    // Nothing is half-written under this lock, so a panic elsewhere while it
    // was held leaves a name that is still good.
    let mut sticky = STICKY.lock().unwrap_or_else(PoisonError::into_inner);
    if sticky.as_deref() != Some(bus_name) {
        *sticky = Some(bus_name.to_string());
    }
}

/// The player that wins ties, if one has been playing or been sent a command.
pub fn sticky() -> Option<String> {
    STICKY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The player to show or control among `players`, by bus name.
///
/// Only players whose name contains `source`, ignoring case, count. Playing
/// beats paused beats stopped; among equals the `sticky` player wins, then
/// the alphabetically first. Without the sticky tie-break, pausing Spotify
/// while a browser tab sits paused would hand the next press to the browser,
/// whose name sorts first.
pub fn pick(
    players: &[(String, Status)],
    source: Option<&str>,
    sticky: Option<&str>,
) -> Option<String> {
    players
        .iter()
        .filter(|(name, _)| matches(name, source))
        .min_by_key(|(name, status)| (status.rank(), Some(name.as_str()) != sticky, name))
        .map(|(name, _)| name.clone())
}

/// Whether the bus name `name` is a player `source` asks for.
///
/// A substring rather than an exact name: browsers register as
/// `firefox.instance_1_85`, with a number that changes every launch.
fn matches(name: &str, source: Option<&str>) -> bool {
    source.is_none_or(|source| short(name).to_lowercase().contains(&source.to_lowercase()))
}

/// A bus name without the MPRIS prefix, which is the same on every player.
fn short(name: &str) -> &str {
    name.strip_prefix(PREFIX).unwrap_or(name)
}

/// A session-bus connection, opened on first use and reopened after a
/// failure, plus the last cover fetched.
#[derive(Default)]
pub struct MediaClient {
    connection: Option<Connection>,
    art: Option<Art>,
    /// A URL that failed, so a broken cover is not fetched every second.
    bad_art: Option<String>,
    http: Option<ureq::Agent>,
}

/// Whether any player is on the bus at all, as opposed to the bus failing.
pub enum Poll {
    Playing(Media),
    Nothing,
    Failed,
}

impl MediaClient {
    /// What the chosen player is doing. Blocking.
    pub fn poll(&mut self, source: Option<&str>) -> Poll {
        let connection = match connect(&mut self.connection) {
            Ok(connection) => connection,
            Err(e) => {
                log::debug!("no session bus for the media widget: {e}");
                return Poll::Failed;
            }
        };
        match self.read(&connection, source) {
            Ok(Some(mut media)) => {
                media.art = self.cover(media.art.take().map(|a| a.url));
                Poll::Playing(media)
            }
            Ok(None) => Poll::Nothing,
            Err(e) => {
                log::debug!("reading media players: {e}");
                // The bus may have gone with the session; try a fresh one.
                self.connection = None;
                Poll::Failed
            }
        }
    }

    fn read(&self, connection: &Connection, source: Option<&str>) -> zbus::Result<Option<Media>> {
        let mut players: Vec<(String, Status)> = statuses(connection, source)?
            .into_iter()
            .filter_map(|(name, status)| {
                // Passed over, where a command would stop (see `choose`):
                // a card showing the next best player beats a blank one.
                let status = status.map_err(|e| skipped(&name, &e)).ok()?;
                Some((name, status))
            })
            .collect();
        let sticky = sticky();
        while let Some(chosen) = pick(&players, source, sticky.as_deref()) {
            // Taken out now, so that if it fails the next pick is another.
            let Some(at) = players.iter().position(|(name, _)| *name == chosen) else {
                break;
            };
            let (name, status) = players.swap_remove(at);
            match player(connection, &name).and_then(|proxy| describe(&proxy, &name, status)) {
                Ok(media) => {
                    // What the card shows playing is what the knob should
                    // act on once it stops.
                    if media.status == Status::Playing {
                        remember(&name);
                    }
                    return Ok(Some(media));
                }
                // One player misbehaving must not hide the others.
                Err(e) => skipped(&name, &e),
            }
        }
        Ok(None)
    }

    /// The cover at `url`, reusing the last one when it has not changed.
    fn cover(&mut self, url: Option<String>) -> Option<Art> {
        let url = url?;
        if let Some(art) = &self.art {
            if art.url == url {
                return Some(art.clone());
            }
        }
        if self.bad_art.as_deref() == Some(url.as_str()) {
            return None;
        }
        match self.load_cover(&url) {
            Some(image) => {
                let art = Art {
                    url,
                    image: Arc::new(image),
                };
                self.art = Some(art.clone());
                Some(art)
            }
            None => {
                self.bad_art = Some(url);
                None
            }
        }
    }

    fn load_cover(&mut self, url: &str) -> Option<image::RgbaImage> {
        let bytes = if let Some(path) = url.strip_prefix("file://") {
            read_art_file(path)?
        } else if url.starts_with("https://") || url.starts_with("http://") {
            let agent = self.http.get_or_insert_with(|| {
                ureq::Agent::config_builder()
                    .timeout_global(Some(ART_TIMEOUT))
                    .build()
                    .into()
            });
            agent
                .get(url)
                .call()
                .map_err(|e| log::debug!("fetching cover {url}: {e}"))
                .ok()?
                .body_mut()
                .with_config()
                .limit(MAX_ART_BYTES)
                .read_to_vec()
                .ok()?
        } else {
            return None;
        };
        let image = image::load_from_memory(&bytes)
            .map_err(|e| log::debug!("decoding cover {url}: {e}"))
            .ok()?;
        Some(image.thumbnail(ART_PIXELS, ART_PIXELS).to_rgba8())
    }
}

/// A cover from disk, refused unless it is a regular file of cover size.
///
/// The path is whatever a player put in its metadata. A FIFO there would
/// block the worker for good and `/dev/zero` would never end, so only a
/// regular file is read; and a file can grow between the size check and
/// the read, so the read is capped as well.
fn read_art_file(path: &str) -> Option<Vec<u8>> {
    // Looked at before opening, since opening some devices does something —
    // spins up a disc drive, say — even when nothing is then read.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    read_at_most(open_cover(path)?, MAX_ART_BYTES)
}

/// `path` opened for reading, if what was opened is a regular file of cover
/// size.
///
/// Checked on the open file, not the path again: a player can swap the
/// file for a FIFO between a look at the path and the open. For the same
/// reason the open does not block, which a FIFO with no writer otherwise
/// would for ever, and cannot make a terminal the daemon's own.
fn open_cover(path: &str) -> Option<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.len() <= MAX_ART_BYTES).then_some(file)
}

/// All of `reader`, or `None` if it holds more than `limit` bytes.
///
/// Reads one byte past the limit to tell "exactly the limit" from "more".
fn read_at_most(reader: impl Read, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

/// Something the knob or a key can ask of a player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    /// Move by this many microseconds, back when negative. Relative, because
    /// a browser reports its position as always 0 and the same track id for
    /// every track, which an absolute `SetPosition` would need.
    Seek(i64),
}

impl MediaCommand {
    /// A seek by `seconds`, back when negative: what a knob's step times its
    /// detents asks for. Saturates rather than overflowing; what is sent is
    /// clamped to ten minutes either way regardless.
    pub fn seek_seconds(seconds: i64) -> Self {
        MediaCommand::Seek(seconds.saturating_mul(MICROS_PER_SECOND))
    }

    /// The Player method that carries it out.
    fn method(self) -> &'static str {
        match self {
            MediaCommand::PlayPause => "PlayPause",
            MediaCommand::Next => "Next",
            MediaCommand::Previous => "Previous",
            MediaCommand::Seek(_) => "Seek",
        }
    }

    /// The method's one argument, if it takes one: a seek's offset, clamped.
    fn argument(self) -> Option<i64> {
        match self {
            MediaCommand::Seek(offset) => Some(seek_offset(offset)),
            _ => None,
        }
    }

    /// The property that says whether the method would do anything, and
    /// what to tell the user when it would not.
    ///
    /// MPRIS makes these silent no-ops rather than errors — a YouTube tab
    /// answers `Next` happily and changes nothing — so asking first is the
    /// only way to know. `PlayPause` has no such property: a player that
    /// cannot pause is meant to answer with an error, which is reported.
    fn capability(self) -> Option<(&'static str, &'static str)> {
        match self {
            MediaCommand::PlayPause => None,
            MediaCommand::Next => Some(("CanGoNext", "no next track")),
            MediaCommand::Previous => Some(("CanGoPrevious", "no previous track")),
            MediaCommand::Seek(_) => Some(("CanSeek", "cannot seek")),
        }
    }
}

/// The offset a seek sends: `offset` microseconds, at most ten minutes
/// either way.
fn seek_offset(offset: i64) -> i64 {
    offset.clamp(-MAX_SEEK, MAX_SEEK)
}

/// What became of a [`MediaCommand`].
///
/// `player` is the bus name without the MPRIS prefix, as in
/// [`Media::player`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done {
        player: String,
    },
    /// The player says the command would do nothing, so it was not sent.
    Unsupported {
        player: String,
        why: &'static str,
    },
    /// No player matching the source is on the bus.
    NoPlayer,
}

/// Sends commands to the player the media widget would show.
///
/// Its own session-bus connection rather than the widget's: the widget's
/// lives on the media worker, and a command must not wait behind a poll.
#[derive(Default)]
pub struct MediaControl {
    connection: Option<Connection>,
}

impl MediaControl {
    /// Send `command` to the player [`pick`] chooses among those matching
    /// `source`. Blocking, though no one call waits on a player for longer
    /// than two seconds.
    ///
    /// `Err` is the bus or a player failing, or a player that could not say
    /// what it is doing when that decides which one the command is for; a
    /// player that would ignore the command is [`Outcome::Unsupported`], not
    /// an error.
    pub fn send(&mut self, command: MediaCommand, source: Option<&str>) -> Result<Outcome, String> {
        let connection =
            connect(&mut self.connection).map_err(|e| format!("no session bus: {e}"))?;
        let outcome = control(&connection, command, source);
        if outcome.is_err() {
            // The bus may have gone with the session, or be wedged behind a
            // player that timed out; start afresh next time.
            self.connection = None;
        }
        outcome
    }
}

/// [`MediaControl::send`] on `connection`. An error is logged here and says
/// what was being done, naming the player, for the report.
fn control(
    connection: &Connection,
    command: MediaCommand,
    source: Option<&str>,
) -> Result<Outcome, String> {
    let players = statuses(connection, source).map_err(|e| failed("listing media players", &e))?;
    let name = match choose(players, source, sticky().as_deref()) {
        Choice::Player(name) => name,
        Choice::Nobody => return Ok(Outcome::NoPlayer),
        Choice::Unsure(name, e) => {
            return Err(failed(
                &format!("asking {} what it is doing", short(&name)),
                &e,
            ));
        }
    };
    let shown = short(&name).to_string();
    let performed = player(connection, &name).and_then(|proxy| {
        perform(
            command,
            |property| proxy.get_property(property),
            |method, argument| match argument {
                Some(argument) => proxy.call(method, &(argument,)),
                None => proxy.call(method, &()),
            },
        )
    });
    match performed {
        Ok(Some(why)) => Ok(Outcome::Unsupported { player: shown, why }),
        Ok(None) => {
            remember(&name);
            Ok(Outcome::Done { player: shown })
        }
        Err(e) => Err(failed(
            &format!("sending {} to {shown}", command.method()),
            &e,
        )),
    }
}

/// `error`, met while doing `what`, logged and put the way a report says it.
fn failed(what: &str, error: &zbus::Error) -> String {
    skipped(what, error);
    format!("{what}: {error}")
}

/// Which player a command is for.
enum Choice {
    /// This one, by bus name.
    Player(String),
    /// None matching the source is on the bus.
    Nobody,
    /// It turns on what this player is doing, and it did not say: why not.
    Unsure(String, zbus::Error),
}

/// The player a command goes to among `players`, as [`pick`] chooses it.
///
/// A player that could not say what it is doing might be playing, so it
/// counts as playing. If it would then win, the right choice turns on what
/// it is doing, and a guess sends the command elsewhere: a hung Spotify
/// playing next to a paused browser tab, and play-pause starts the tab too.
/// [`Choice::Unsure`] says why instead — also when every player refuses a
/// confined daemon, which would otherwise look like no player at all. A
/// player that has left the bus since it was listed is simply not there.
fn choose(
    players: Vec<(String, zbus::Result<Status>)>,
    source: Option<&str>,
    sticky: Option<&str>,
) -> Choice {
    let mut ranked = Vec::with_capacity(players.len());
    let mut unread = Vec::new();
    for (name, status) in players {
        match status {
            Ok(status) => ranked.push((name, status)),
            Err(e) if gone(&e) => {}
            Err(e) => {
                ranked.push((name.clone(), Status::Playing));
                unread.push((name, e));
            }
        }
    }
    let Some(chosen) = pick(&ranked, source, sticky) else {
        return Choice::Nobody;
    };
    match unread.into_iter().find(|(name, _)| *name == chosen) {
        Some((name, e)) => Choice::Unsure(name, e),
        None => Choice::Player(chosen),
    }
}

/// Carry out `command`, unless the player says it would do nothing: then
/// `Some(why)`, and nothing is sent.
///
/// `can` reads one of the player's boolean properties and `call` sends a
/// method with its argument, if any — the player's proxy in use, fakes in
/// the tests, which is the point of taking them apart.
fn perform(
    command: MediaCommand,
    can: impl FnMut(&'static str) -> zbus::Result<bool>,
    call: impl FnOnce(&'static str, Option<i64>) -> zbus::Result<()>,
) -> zbus::Result<Option<&'static str>> {
    if let Some(why) = refusal(command, can)? {
        return Ok(Some(why));
    }
    call(command.method(), command.argument())?;
    Ok(None)
}

/// Why `command` would do nothing on a player whose boolean properties
/// `can` reads, if it would.
fn refusal(
    command: MediaCommand,
    mut can: impl FnMut(&'static str) -> zbus::Result<bool>,
) -> zbus::Result<Option<&'static str>> {
    // With CanControl false every other Can… is false too, and the reason is
    // the player, not the command.
    if !can("CanControl")? {
        return Ok(Some("the player takes no commands"));
    }
    let Some((property, why)) = command.capability() else {
        return Ok(None);
    };
    Ok((!can(property)?).then_some(why))
}

/// Every player on the session bus: its name without the MPRIS prefix, what
/// it is doing, and its title. Empty when there is no bus. Blocking.
pub fn players() -> Vec<(String, Status, Option<String>)> {
    let Ok(connection) = session() else {
        return Vec::new();
    };
    names(&connection)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| {
            let media = read_player(&connection, &name)
                .map_err(|e| skipped(&name, &e))
                .ok()?;
            Some((media.player, media.status, media.title))
        })
        .collect()
}

/// A session-bus connection whose calls give up after [`CALL_TIMEOUT`].
fn session() -> zbus::Result<Connection> {
    zbus::blocking::connection::Builder::session()?
        .method_timeout(CALL_TIMEOUT)
        .build()
}

/// The connection in `slot`, opening one first if there is none.
fn connect(slot: &mut Option<Connection>) -> zbus::Result<Connection> {
    if let Some(connection) = slot {
        return Ok(connection.clone());
    }
    let connection = session()?;
    *slot = Some(connection.clone());
    Ok(connection)
}

/// Every MPRIS player's bus name, sorted.
fn names(connection: &Connection) -> zbus::Result<Vec<String>> {
    let mut names: Vec<String> = zbus::blocking::fdo::DBusProxy::new(connection)?
        .list_names()?
        .into_iter()
        .map(|name| name.to_string())
        .filter(|name| name.starts_with(PREFIX))
        .collect();
    names.sort();
    Ok(names)
}

/// Every player matching `source`, by bus name, and what it says it is
/// doing: all [`pick`] needs, and one call per player.
///
/// A player that does not answer keeps its error rather than failing the
/// lot or vanishing: the widget can pass over it, but a command must not
/// go elsewhere just because the player it was meant for was slow to say
/// it is playing.
fn statuses(
    connection: &Connection,
    source: Option<&str>,
) -> zbus::Result<Vec<(String, zbus::Result<Status>)>> {
    Ok(names(connection)?
        .into_iter()
        .filter(|name| matches(name, source))
        .map(|name| {
            let status = player(connection, &name).and_then(|proxy| status(&proxy));
            (name, status)
        })
        .collect())
}

/// The Player interface of the player at `name`.
fn player(connection: &Connection, name: &str) -> zbus::Result<Proxy<'static>> {
    zbus::blocking::proxy::Builder::<Proxy>::new(connection)
        .destination(name.to_string())?
        .path(PATH)?
        .interface(PLAYER)?
        // Caching would subscribe to change signals, which `Position` never
        // sends; and every value here is read fresh each time anyway.
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
}

fn status(proxy: &Proxy<'_>) -> zbus::Result<Status> {
    let status: String = proxy.get_property("PlaybackStatus")?;
    Ok(match status.as_str() {
        "Playing" => Status::Playing,
        "Paused" => Status::Paused,
        _ => Status::Stopped,
    })
}

fn read_player(connection: &Connection, name: &str) -> zbus::Result<Media> {
    let proxy = player(connection, name)?;
    describe(&proxy, name, status(&proxy)?)
}

/// The rest of what the card shows, for a player already known to be in
/// `status`.
fn describe(proxy: &Proxy<'_>, name: &str, status: Status) -> zbus::Result<Media> {
    let metadata: HashMap<String, OwnedValue> = proxy.get_property("Metadata")?;
    // Not every player implements Position; a missing one is no progress bar,
    // not a missing player.
    let position = proxy
        .get_property::<i64>("Position")
        .ok()
        .and_then(microseconds);

    let text = |key: &str| {
        metadata
            .get(key)
            .and_then(|v| string(v))
            .filter(|s| !s.is_empty())
    };
    let artist = metadata
        .get("xesam:artist")
        .and_then(|value| match &**value {
            Value::Array(array) => {
                let names: Vec<String> = array.iter().filter_map(string).collect();
                (!names.is_empty()).then(|| names.join(", "))
            }
            other => string(other),
        });
    let length = metadata
        .get("mpris:length")
        .and_then(|value| match &**value {
            Value::I64(n) => microseconds(*n),
            Value::U64(n) => i64::try_from(*n).ok().and_then(microseconds),
            _ => None,
        });

    Ok(Media {
        player: short(name).to_string(),
        status,
        title: text("xesam:title"),
        artist,
        album: text("xesam:album"),
        position,
        length,
        art: text("mpris:artUrl").map(|url| Art {
            url,
            image: Arc::new(image::RgbaImage::new(0, 0)),
        }),
    })
}

/// Whether the bus refused to deliver a message, rather than a player
/// failing to answer one.
fn denied(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"
        }
        zbus::Error::FDO(error) => matches!(**error, zbus::fdo::Error::AccessDenied(_)),
        _ => false,
    }
}

/// Whether the player an error came from is not there at all: its name
/// left the bus after it was listed, or it has no MPRIS object. As opposed
/// to one that is there and did not answer, which might yet be playing.
fn gone(error: &zbus::Error) -> bool {
    use zbus::fdo::Error as Fdo;
    match error {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
        ),
        zbus::Error::FDO(error) => matches!(
            **error,
            Fdo::ServiceUnknown(_)
                | Fdo::NameHasNoOwner(_)
                | Fdo::UnknownObject(_)
                | Fdo::UnknownInterface(_)
        ),
        _ => false,
    }
}

/// Set once the AppArmor refusal below has been logged.
static WARNED_DENIED: AtomicBool = AtomicBool::new(false);

/// Log a player that could not be read or controlled.
///
/// At debug, since players come and go mid-call all the time — except the
/// first AccessDenied, at warn. That one means AppArmor: a snap-confined
/// player such as Firefox refuses messages from a daemon that is confined
/// too, as one started from a snap-packaged editor's terminal is. Nothing
/// else would explain a blank card and a dead knob, and nothing the daemon
/// does fixes it.
fn skipped(what: &str, error: &zbus::Error) {
    if denied(error) && !WARNED_DENIED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "{what}: {error}. The player is probably confined by AppArmor and \
             refuses confined senders; run the daemon unconfined (as \
             galdeck.service is) to show and control it"
        );
    } else {
        log::debug!("{what}: {error}");
    }
}

fn string(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.to_string()),
        Value::Value(inner) => string(inner),
        _ => None,
    }
}

fn microseconds(n: i64) -> Option<Duration> {
    u64::try_from(n).ok().map(Duration::from_micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(position: Option<u64>, length: Option<u64>) -> Media {
        Media {
            player: "spotify".into(),
            status: Status::Playing,
            title: None,
            artist: None,
            album: None,
            position: position.map(Duration::from_secs),
            length: length.map(Duration::from_secs),
            art: None,
        }
    }

    /// Players as the bus lists them: full names, in no particular order.
    fn bus(players: &[(&str, Status)]) -> Vec<(String, Status)> {
        players
            .iter()
            .map(|(name, status)| (format!("{PREFIX}{name}"), *status))
            .collect()
    }

    fn full(name: &str) -> Option<String> {
        Some(format!("{PREFIX}{name}"))
    }

    #[test]
    fn timestamps_read_like_a_player() {
        assert_eq!(timestamp(Duration::from_secs(31)), "0:31");
        assert_eq!(timestamp(Duration::from_secs(125)), "2:05");
        assert_eq!(timestamp(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn progress_needs_both_ends_and_stays_in_range() {
        assert_eq!(media(Some(30), Some(120)).progress(), Some(0.25));
        assert_eq!(media(Some(30), None).progress(), None);
        assert_eq!(media(Some(30), Some(0)).progress(), None);
        // A position past the end — some players report one briefly.
        assert_eq!(media(Some(200), Some(120)).progress(), Some(1.0));
    }

    #[test]
    fn covers_compare_by_url() {
        let a = Art {
            url: "x".into(),
            image: Arc::new(image::RgbaImage::new(1, 1)),
        };
        let b = Art {
            url: "x".into(),
            image: Arc::new(image::RgbaImage::new(2, 2)),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn playing_beats_paused_beats_stopped() {
        let players = bus(&[
            ("amarok", Status::Stopped),
            ("spotify", Status::Playing),
            ("firefox.instance_1_85", Status::Paused),
        ]);
        assert_eq!(pick(&players, None, None), full("spotify"));
        let players = bus(&[("amarok", Status::Stopped), ("vlc", Status::Paused)]);
        assert_eq!(pick(&players, None, None), full("vlc"));
        assert_eq!(pick(&[], None, None), None);
    }

    #[test]
    fn the_sticky_player_wins_a_tie_and_only_a_tie() {
        // Spotify was just paused by the knob, with a browser tab paused
        // since yesterday: the next press must resume Spotify.
        let players = bus(&[
            ("firefox.instance_1_85", Status::Paused),
            ("spotify", Status::Paused),
        ]);
        let spotify = full("spotify");
        assert_eq!(pick(&players, None, spotify.as_deref()), spotify);
        // With nothing sticky, the first by name, every time.
        assert_eq!(pick(&players, None, None), full("firefox.instance_1_85"));
        // A sticky player that has gone is no reason to prefer anything.
        let gone = full("rhythmbox");
        assert_eq!(
            pick(&players, None, gone.as_deref()),
            full("firefox.instance_1_85")
        );

        // Something actually playing beats the sticky player paused.
        let players = bus(&[("spotify", Status::Paused), ("vlc", Status::Playing)]);
        assert_eq!(pick(&players, None, spotify.as_deref()), full("vlc"));
    }

    #[test]
    fn a_source_narrows_the_choice_by_substring_ignoring_case() {
        let players = bus(&[
            ("firefox.instance_1_85", Status::Playing),
            ("spotify", Status::Paused),
        ]);
        assert_eq!(pick(&players, Some("Spotify"), None), full("spotify"));
        assert_eq!(
            pick(&players, Some("firefox"), None),
            full("firefox.instance_1_85")
        );
        assert_eq!(pick(&players, Some("vlc"), None), None);
        // The prefix every player shares is not what a source matches.
        assert_eq!(pick(&players, Some("mpris"), None), None);
        // A sticky player outside the source is not picked through it.
        let sticky = full("spotify");
        assert_eq!(
            pick(&players, Some("firefox"), sticky.as_deref()),
            full("firefox.instance_1_85")
        );
    }

    #[test]
    fn seeks_convert_to_microseconds_without_overflowing() {
        assert_eq!(MediaCommand::seek_seconds(5), MediaCommand::Seek(5_000_000));
        assert_eq!(
            MediaCommand::seek_seconds(-15),
            MediaCommand::Seek(-15_000_000)
        );
        assert_eq!(MediaCommand::seek_seconds(0), MediaCommand::Seek(0));
        assert_eq!(
            MediaCommand::seek_seconds(i64::MAX),
            MediaCommand::Seek(i64::MAX)
        );
        assert_eq!(
            MediaCommand::seek_seconds(i64::MIN),
            MediaCommand::Seek(i64::MIN)
        );
    }

    #[test]
    fn a_seek_goes_at_most_ten_minutes_either_way() {
        assert_eq!(seek_offset(5_000_000), 5_000_000);
        assert_eq!(seek_offset(-5_000_000), -5_000_000);
        assert_eq!(seek_offset(600_000_000), 600_000_000);
        assert_eq!(seek_offset(600_000_001), 600_000_000);
        assert_eq!(seek_offset(-700_000_000), -600_000_000);
        assert_eq!(seek_offset(i64::MAX), 600_000_000);
        assert_eq!(seek_offset(i64::MIN), -600_000_000);
        let MediaCommand::Seek(huge) = MediaCommand::seek_seconds(1_000_000) else {
            unreachable!()
        };
        assert_eq!(seek_offset(huge), 600_000_000);
    }

    #[test]
    fn each_command_maps_to_its_method_capability_and_argument() {
        use MediaCommand::*;
        assert_eq!(PlayPause.capability(), None);
        assert_eq!(Next.capability().map(|c| c.0), Some("CanGoNext"));
        assert_eq!(Previous.capability().map(|c| c.0), Some("CanGoPrevious"));
        assert_eq!(Seek(-1).capability().map(|c| c.0), Some("CanSeek"));
        let methods = [PlayPause, Next, Previous, Seek(1)].map(MediaCommand::method);
        assert_eq!(methods, ["PlayPause", "Next", "Previous", "Seek"]);
        let arguments =
            [PlayPause, Next, Previous, Seek(-5), Seek(i64::MAX)].map(MediaCommand::argument);
        assert_eq!(arguments, [None, None, None, Some(-5), Some(600_000_000)]);
    }

    /// What [`perform`] did with a fake player.
    #[derive(Debug, PartialEq)]
    struct Performed {
        answer: Result<Option<&'static str>, String>,
        /// The properties it read, in order.
        read: Vec<&'static str>,
        /// The methods it called, with their argument.
        sent: Vec<(&'static str, Option<i64>)>,
    }

    /// [`perform`] on a player whose boolean properties are `properties`,
    /// any other being unknown to it.
    fn perform_on(command: MediaCommand, properties: &[(&str, bool)]) -> Performed {
        let (mut read, mut sent) = (Vec::new(), Vec::new());
        let answer = perform(
            command,
            |property| {
                read.push(property);
                let value = properties.iter().find(|(name, _)| *name == property);
                value
                    .map(|(_, value)| *value)
                    .ok_or_else(|| zbus::fdo::Error::UnknownProperty(property.into()).into())
            },
            |method, argument| {
                sent.push((method, argument));
                Ok(())
            },
        );
        Performed {
            answer: answer.map_err(|e| e.to_string()),
            read,
            sent,
        }
    }

    #[test]
    fn a_command_is_sent_only_when_the_player_says_it_would_do_something() {
        use MediaCommand::*;
        // A player taking no commands is not asked about the command, and
        // the reason given is the player.
        assert_eq!(
            perform_on(Next, &[("CanControl", false), ("CanGoNext", true)]),
            Performed {
                answer: Ok(Some("the player takes no commands")),
                read: vec!["CanControl"],
                sent: vec![],
            }
        );
        // A YouTube tab: happy to be told Next, and nothing would happen.
        assert_eq!(
            perform_on(Next, &[("CanControl", true), ("CanGoNext", false)]),
            Performed {
                answer: Ok(Some("no next track")),
                read: vec!["CanControl", "CanGoNext"],
                sent: vec![],
            }
        );
        assert_eq!(
            perform_on(Previous, &[("CanControl", true), ("CanGoPrevious", true)]),
            Performed {
                answer: Ok(None),
                read: vec!["CanControl", "CanGoPrevious"],
                sent: vec![("Previous", None)],
            }
        );
        // Nothing but CanControl stands in the way of play-pause.
        assert_eq!(
            perform_on(PlayPause, &[("CanControl", true)]),
            Performed {
                answer: Ok(None),
                read: vec!["CanControl"],
                sent: vec![("PlayPause", None)],
            }
        );

        // What is sent is clamped, whatever was asked.
        let seek = MediaCommand::seek_seconds(-3600);
        let performed = perform_on(seek, &[("CanControl", true), ("CanSeek", true)]);
        assert_eq!(performed.answer, Ok(None));
        assert_eq!(performed.sent, [("Seek", Some(-600_000_000))]);
        let performed = perform_on(seek, &[("CanControl", true), ("CanSeek", false)]);
        assert_eq!(performed.answer, Ok(Some("cannot seek")));
        assert_eq!(performed.sent, []);

        // A property that cannot be read is an error, not a guess either way.
        let performed = perform_on(Seek(1), &[("CanControl", true)]);
        assert!(performed.answer.is_err());
        assert_eq!(performed.sent, []);
    }

    /// The error a peer or the bus replies with when it replies `name`.
    fn replied(name: &str, detail: &str) -> zbus::Error {
        let call = zbus::Message::method_call(PATH, "PlayPause")
            .unwrap()
            .build(&())
            .unwrap();
        let reply = zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&(detail,))
            .unwrap();
        zbus::Error::from(reply)
    }

    fn timed_out() -> zbus::Error {
        std::io::Error::from(std::io::ErrorKind::TimedOut).into()
    }

    fn refused() -> zbus::Error {
        zbus::fdo::Error::AccessDenied("AppArmor".into()).into()
    }

    /// [`choose`] as a test can compare: the player chosen, or the one that
    /// kept it from choosing.
    fn choose_among(
        players: Vec<(&str, zbus::Result<Status>)>,
        source: Option<&str>,
        sticky: Option<&str>,
    ) -> Result<Option<String>, String> {
        let players = players
            .into_iter()
            .map(|(name, status)| (format!("{PREFIX}{name}"), status))
            .collect();
        let sticky = sticky.map(|name| format!("{PREFIX}{name}"));
        match choose(players, source, sticky.as_deref()) {
            Choice::Player(name) => Ok(Some(name)),
            Choice::Nobody => Ok(None),
            Choice::Unsure(name, _) => Err(name),
        }
    }

    #[test]
    fn a_player_that_did_not_answer_counts_as_playing() {
        let spotify = || full("spotify").unwrap();
        // A hung Spotify, playing as far as anyone knows, and a paused tab:
        // play-pause must not start the tab.
        let players = || {
            vec![
                ("firefox.instance_1_85", Ok(Status::Paused)),
                ("spotify", Err(timed_out())),
            ]
        };
        assert_eq!(choose_among(players(), None, None), Err(spotify()));
        assert_eq!(
            choose_among(players(), None, Some("spotify")),
            Err(spotify())
        );
        // Unless the source rules it out.
        assert_eq!(
            choose_among(players(), Some("firefox"), None),
            Ok(full("firefox.instance_1_85"))
        );

        // A player that answered and would win even against a playing one
        // is chosen: playing and sticky, or playing and first by name.
        let players = || vec![("spotify", Ok(Status::Playing)), ("vlc", Err(refused()))];
        assert_eq!(choose_among(players(), None, None), Ok(full("spotify")));
        assert_eq!(
            choose_among(players(), None, Some("vlc")),
            Err(full("vlc").unwrap())
        );
        let players = || vec![("amarok", Err(refused())), ("spotify", Ok(Status::Playing))];
        assert_eq!(
            choose_among(players(), None, Some("spotify")),
            Ok(full("spotify"))
        );
        assert_eq!(
            choose_among(players(), None, None),
            Err(full("amarok").unwrap())
        );

        // Every player refusing, as they do a confined daemon, is not "no
        // player": it is the refusal.
        let players = vec![("firefox.instance_1_85", Err(refused()))];
        assert_eq!(
            choose_among(players, None, None),
            Err(full("firefox.instance_1_85").unwrap())
        );
        assert_eq!(choose_among(vec![], None, None), Ok(None));
    }

    #[test]
    fn a_player_that_left_the_bus_is_not_waited_for() {
        let left = || replied("org.freedesktop.DBus.Error.ServiceUnknown", "gone");
        assert!(gone(&left()));
        assert!(gone(
            &zbus::fdo::Error::NameHasNoOwner("gone".into()).into()
        ));
        assert!(gone(&zbus::fdo::Error::UnknownObject("none".into()).into()));
        assert!(!gone(&timed_out()));
        assert!(!gone(&refused()));
        assert!(!gone(&zbus::fdo::Error::Failed("no".into()).into()));

        let players = vec![
            ("firefox.instance_1_85", Ok(Status::Paused)),
            ("spotify", Err(left())),
        ];
        assert_eq!(
            choose_among(players, None, Some("spotify")),
            Ok(full("firefox.instance_1_85"))
        );
        assert_eq!(
            choose_among(vec![("spotify", Err(left()))], None, None),
            Ok(None)
        );
    }

    #[test]
    fn the_sticky_player_is_what_was_last_remembered() {
        // Names no other test uses: the store is shared by the whole process.
        let first = format!("{PREFIX}test.first_{}", std::process::id());
        let second = format!("{PREFIX}test.second_{}", std::process::id());
        remember(&first);
        assert_eq!(sticky(), Some(first.clone()));
        remember(&second);
        remember(&second);
        assert_eq!(sticky(), Some(second));
    }

    #[test]
    fn access_denied_is_told_apart_from_other_failures() {
        assert!(denied(&refused()));
        let failed: zbus::Error = zbus::fdo::Error::Failed("no".into()).into();
        assert!(!denied(&failed));
        assert!(!denied(&zbus::Error::InvalidReply));
        assert!(!denied(&timed_out()));

        // The same refusal as a method call's reply, which is how it arrives.
        assert!(denied(&replied(
            "org.freedesktop.DBus.Error.AccessDenied",
            "An AppArmor policy prevents this sender"
        )));
    }

    #[test]
    fn a_read_stops_one_byte_past_the_limit() {
        let read = |bytes: &[u8]| read_at_most(std::io::Cursor::new(bytes.to_vec()), 4);
        assert_eq!(read(b"abc"), Some(b"abc".to_vec()));
        assert_eq!(read(b"abcd"), Some(b"abcd".to_vec()));
        assert_eq!(read(b"abcde"), None);
        // An endless reader, like /dev/zero, ends too.
        assert_eq!(read_at_most(std::io::repeat(0), 4), None);
    }

    #[test]
    fn a_cover_on_disk_must_be_a_regular_file_of_cover_size() {
        let dir = std::env::temp_dir().join(format!("galdeck-media-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cover = dir.join("cover.png");
        std::fs::write(&cover, b"not really a png").unwrap();
        assert_eq!(
            read_art_file(cover.to_str().unwrap()),
            Some(b"not really a png".to_vec())
        );

        // Sparse, so this costs no disk.
        let huge = dir.join("huge.png");
        File::create(&huge)
            .unwrap()
            .set_len(MAX_ART_BYTES + 1)
            .unwrap();
        assert_eq!(read_art_file(huge.to_str().unwrap()), None);

        assert_eq!(read_art_file(dir.to_str().unwrap()), None);
        assert_eq!(read_art_file("/dev/zero"), None);
        assert_eq!(read_art_file(dir.join("missing").to_str().unwrap()), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cover_that_is_a_fifo_is_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let dir = std::env::temp_dir().join(format!("galdeck-media-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("cover.png");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid C string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let path = fifo.to_str().unwrap().to_string();

        // Opened straight away, as it would be if swapped in after the path
        // was looked at. With no writer a blocking open never returns, so
        // it runs on a thread of its own and is given a few seconds.
        let (tx, rx) = std::sync::mpsc::channel();
        let open = path.clone();
        std::thread::spawn(move || tx.send(open_cover(&open).is_some()));
        let opened = rx.recv_timeout(Duration::from_secs(5));
        let read = read_art_file(&path);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(opened, Ok(false));
        assert_eq!(read, None);
    }
}
