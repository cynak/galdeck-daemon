//! Icons: names looked up in the desktop's icon theme, and icon files drawn
//! to fit a key.
//!
//! A key's icon is either a file or a name such as
//! `network-wireless-symbolic`. A name is found much as GTK finds one: in the
//! icon theme and the themes it inherits from, then in Adwaita, then in
//! hicolor, which every theme falls back to. The toggles people want on a key
//! exist in Adwaita and Yaru only as `-symbolic.svg`, so SVG is drawn here
//! with resvg; a symbolic icon is then recoloured with the key's label colour,
//! the way the desktop recolours it for its own text colour.
//!
//! Nothing here is trusted to be small or well-behaved. A theme can be a
//! user's own, an SVG's drawing time has nothing to do with its size, and a
//! PNG can claim any dimensions in its header, so every read is capped and
//! checked before it is opened. Decoding costs time on the engine thread, so
//! it happens once per icon and size: [`IconCache`] keeps what was drawn and
//! [`IconThemes`] keeps what each name resolved to.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use galdeck::Rgb;
use image::RgbaImage;
use resvg::{tiny_skia, usvg};

/// How many drawn icons [`IconCache`] keeps.
///
/// A page has a dozen keys, and a key with states has an icon per state; this
/// is a few pages' worth, at about 70 KB each.
pub const CACHE_CAPACITY: usize = 128;
/// Most names [`IconThemes::symbolic_names`] lists. Adwaita alone has about
/// 650; this is room for a large theme on top without an answer growing
/// without bound.
pub const MAX_ICON_NAMES: usize = 5000;

/// Largest SVG read. The largest icon in the installed themes is 56 KB, and
/// parsing time grows with size: a 2.9 MB file of paths takes 117 ms.
const MAX_SVG_BYTES: u64 = 256 * 1024;
/// Largest PNG, JPEG or GIF read.
const MAX_RASTER_BYTES: u64 = 4 * 1024 * 1024;
/// Widest and tallest picture decoded, to be shrunk to fit the key: a
/// photo or an app's 1200 px logo, as the editor's Upload button takes.
/// What bounds a decode is [`MAX_RASTER_ALLOC`]; this only refuses a side
/// no picture meant for a key has.
const MAX_RASTER_SIDE: u32 = 4096;
/// Most memory one decode may take: a 4096x4096 picture with alpha, and
/// no more. A header can claim any size, and a few bytes claiming
/// 20000x20000 would otherwise decode into 1.6 GB.
const MAX_RASTER_ALLOC: u64 = 64 * 1024 * 1024;
/// Widest and tallest box an icon is drawn into: larger than any key.
const MAX_BOX_SIDE: u32 = 1024;
/// Largest index.theme read. hicolor's, which lists 649 directories, is 55 KB.
const MAX_INDEX_BYTES: u64 = 256 * 1024;
/// Most directories one theme may list.
const MAX_DIRECTORIES: usize = 1024;
/// Most themes one theme may inherit from directly.
const MAX_PARENTS: usize = 16;
/// How deep inheritance is followed, counting the chosen theme as 0.
const MAX_INHERIT_DEPTH: usize = 8;
/// Most themes searched, whatever the inheritance says.
const MAX_THEMES: usize = 32;
/// Most names remembered between reloads before the memory is dropped.
const MAX_REMEMBERED: usize = 1024;
/// Most directory entries looked at to list names.
const MAX_LISTED_FILES: usize = 200_000;
/// Most files for one name tried and refused before the rest are passed
/// over untried. Yaru has two copies of some icons, one refused; a theme
/// with a copy in every directory it lists would otherwise have them all
/// read and parsed.
const MAX_TRIED: usize = 4;
/// Most filter steps an SVG may run, counting a filter once for each element
/// it is applied to. Forty chained blurs took 632 ms to draw a 112 px icon;
/// only 16 of the 1,592 symbolic icons installed use a filter at all.
const MAX_FILTER_STEPS: usize = 8;
/// Most elements an SVG may come to once what it refers to is counted
/// wherever it is used. The busiest installed icon, Adwaita's
/// user-trash-full, has 693; a 45 KB file of a thousand-element mask on a
/// thousand elements came to a million, half a gigabyte and a second and a
/// half, and 256 KB of it would come to gigabytes.
const MAX_SVG_ELEMENTS: usize = 10_000;
/// Most layers an SVG may draw, each counted by its size in boxes: groups
/// drawn apart to be blended back, and each clip path and mask applied. A
/// layer can be up to [`LARGEST_LAYER`] boxes, and a thousand nested masks
/// over a large rectangle took 4.7 s to draw.
const MAX_LAYERS: usize = 512;
/// The largest a layer can be, in boxes: resvg clips one to twice the
/// pixmap's size past each edge, five times it each way.
const LARGEST_LAYER: usize = 25;
/// Deepest an SVG's elements and references may nest. The deepest
/// installed icon nests 7. usvg allows 1024, but parsing and drawing recurse
/// once a level, and a thousand nested masks took 4.7 s to draw.
const MAX_SVG_DEPTH: usize = 64;
/// Most path segments an SVG may draw: a path's once each time it is
/// filled or stroked, wherever it is drawn, and each dash of a dashed stroke
/// as one. A line dashed 950,000 times took 374 ms to draw, and a hundred of
/// them 36 s, in 6 KB.
const MAX_SEGMENTS: f64 = 200_000.0;
/// Most the segments of each path drawn may come to, squared and summed.
/// Filling a path sorts its edges past each other along every row of
/// pixels, which for edges that keep crossing takes time growing with the
/// square of how many there are: a zigzag of 10,000 segments took 180 ms
/// to stroke, and one of 40,000 took 2.9 s.
const MAX_SEGMENTS_SQUARED: f64 = 50_000_000.0;
/// Most gradient work an SVG may do, in boxes filled times the stops the
/// gradient has, plus [`GRADIENT_SETUP`]: every pixel a gradient fills looks
/// through its stops. A 4,000-stop gradient filling a thousand rectangles
/// took 9.3 s.
const MAX_GRADIENT_WORK: f64 = 40_000.0;
/// What a gradient costs for each pixel it fills besides its stops, in
/// stops: a box filled with a two-stop gradient took as long as one with
/// fifty more.
const GRADIENT_SETUP: f64 = 50.0;
/// Farthest from the corner it is drawn from, in pixels, that a path may
/// be filled. tiny-skia works in f32: curves filled from 8.5x10^7 pixels
/// out came out past the edge of the pixmap and panicked, stopping the
/// daemon.
const MAX_FILL_REACH: f32 = 1_048_576.0;
/// Farthest a path may be stroked, in pixels from the corner it is drawn
/// from and from its own origin at the scale it is drawn, where its outline
/// is worked out, more finely and less precisely the further out: one curve
/// stroked 10^7 pixels across took 44 ms, and 10^8 across 0.6 s and 100 MB;
/// 2,000 small ones reaching 250,000 pixels took 1.2 s. The farthest an
/// installed icon reaches is 8,138.
const MAX_STROKE_REACH: f32 = 32_768.0;
/// Most edge an SVG may draw, in pixels along what it fills and down both
/// sides of what it strokes: filling walks every edge down each row of
/// pixels it crosses. 990 copies of a path of 200 curves across the icon,
/// 23 KB, took 3.4 s to stroke. The busiest installed icons come to under
/// 100,000.
const MAX_EDGE_PIXELS: f64 = 1_000_000.0;
/// Most an SVG may fill and stroke, in boxes, each drawing counted by the
/// part of a layer it can cover: 9,500 squares over the whole icon, filled
/// and stroked, took 350 ms, and 480 filling a pattern's tile ten times
/// 800 ms, each in under 40 KB. No installed icon comes to 500.
const MAX_FILLED: f64 = 5_000.0;
/// Most path data an SVG may come to, in bytes, once what it refers to is
/// counted wherever it is used: no more than the largest file read holds.
/// usvg strokes every path it builds to find its bounds, before anything
/// here could count it: a thousand copies of a 6,000-curve path, 184 KB,
/// took 12.5 s to parse. The most an installed icon comes to is under 96 KiB.
const MAX_SVG_DATA: usize = 256 * 1024;
/// A drawing slower than this is worth a line in the log.
const SLOW_DRAWING: Duration = Duration::from_millis(50);
/// How long the settings portal is given to say which theme is in use.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(1);

/// Searched after the chosen theme and everything it inherits from.
const FALLBACK_THEME: &str = "Adwaita";
/// Searched last of all. Every theme inherits from it, so a theme that names
/// it early would otherwise have it searched before its other parents.
const LAST_THEME: &str = "hicolor";

/// Where to look for icon themes, from the environment, in the order they
/// are searched.
///
/// ~/.icons, then `$XDG_DATA_HOME/icons`, then each `$XDG_DATA_DIRS` entry's
/// `icons`, then /usr/share/pixmaps: the freedesktop.org search path.
pub fn search_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    roots(
        home.as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
    )
}

/// [`search_roots`] from the given values rather than the environment.
///
/// Relative entries are skipped, as the XDG base directory spec says they
/// must be: an empty
/// one means the current directory, and a daemon should not take icons from
/// wherever it happened to be started.
fn roots(
    home: Option<&Path>,
    data_home: Option<&OsStr>,
    data_dirs: Option<&OsStr>,
) -> Vec<PathBuf> {
    let home = home.filter(|home| home.is_absolute());
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push(home.join(".icons"));
    }
    let data_home = data_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home.map(|home| home.join(".local/share")));
    if let Some(data_home) = data_home {
        roots.push(data_home.join("icons"));
    }
    let data_dirs = data_dirs
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or(OsStr::new("/usr/local/share:/usr/share"));
    roots.extend(
        std::env::split_paths(data_dirs)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("icons")),
    );
    roots.push(PathBuf::from("/usr/share/pixmaps"));

    // The same directory twice only doubles the looking.
    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(root.clone()));
    roots
}

/// The icon theme the desktop uses, as the settings portal reports it.
///
/// Asked over D-Bus rather than by running `gsettings`, which would be a
/// process per reload and, started from a snap's environment, reads the
/// snap's schemas instead of the desktop's. `None` when there is no portal
/// or it does not answer within a second, or when the answer is not a
/// usable theme name.
pub fn desktop_theme() -> Option<String> {
    desktop_theme_on(None, PORTAL_TIMEOUT)
}

/// [`desktop_theme`] from the bus at `address`, or the session bus, given
/// `timeout` in all.
///
/// Asked on a thread of its own: zbus's timeout covers the call but not
/// connecting, and a bus that takes the connection and then says nothing
/// would otherwise hold up a start or a reload for good. While an earlier
/// ask is still stuck, this gives up at once rather than leave another
/// thread waiting beside it.
fn desktop_theme_on(address: Option<String>, timeout: Duration) -> Option<String> {
    static ASKING: AtomicBool = AtomicBool::new(false);
    if ASKING.swap(true, Ordering::AcqRel) {
        log::debug!("icon theme: the settings portal has not answered an earlier ask");
        return None;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("galdeck-icon-theme".into())
        .spawn(move || {
            let name = ask_portal(address.as_deref(), timeout);
            ASKING.store(false, Ordering::Release);
            let _ = tx.send(name);
        });
    if let Err(e) = spawned {
        ASKING.store(false, Ordering::Release);
        log::debug!("icon theme: cannot ask the settings portal: {e}");
        return None;
    }
    rx.recv_timeout(timeout)
        .map_err(|_| {
            log::warn!(
                "icon theme: the settings portal did not answer within {} ms",
                timeout.as_millis()
            )
        })
        .ok()
        .flatten()
}

/// Ask the settings portal on the bus at `address`, or the session bus.
fn ask_portal(address: Option<&str>, timeout: Duration) -> Option<String> {
    let builder = match address {
        Some(address) => zbus::blocking::connection::Builder::address(address),
        None => zbus::blocking::connection::Builder::session(),
    };
    let connection = builder
        .and_then(|builder| builder.method_timeout(timeout).build())
        .map_err(|e| log::debug!("icon theme: no session bus: {e}"))
        .ok()?;
    let reply = connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            "ReadOne",
            &("org.gnome.desktop.interface", "icon-theme"),
        )
        .map_err(|e| log::debug!("icon theme: the settings portal did not say: {e}"))
        .ok()?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    let name = string(&value)?;
    usable_theme(&name, "the desktop's").then_some(name)
}

/// Whether `name` can be looked up, warning when it cannot. `whose` says
/// where the name came from.
fn usable_theme(name: &str, whose: &str) -> bool {
    let valid = valid_theme_name(name);
    if !valid {
        log::warn!("icon theme: {whose} theme {name:?} is not a name that can be looked up");
    }
    valid
}

/// The string in a D-Bus value, however many variants it is wrapped in.
fn string(value: &zbus::zvariant::Value<'_>) -> Option<String> {
    match value {
        zbus::zvariant::Value::Str(s) => Some(s.to_string()),
        zbus::zvariant::Value::Value(inner) => string(inner),
        _ => None,
    }
}

/// An icon file as the config names it, with a leading `~` taken to mean the
/// daemon's user's home: `~/Pictures/mute.png`. Anything else is as written.
pub fn expand_home(path: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    expand_home_in(path, home.as_deref())
}

/// [`expand_home`] with `home` given rather than taken from the
/// environment. Only `~` itself and `~/`: `~alice/` is another user's home,
/// which is not the daemon's to go looking in. A home that is not absolute
/// is no home.
fn expand_home_in(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home.filter(|home| home.is_absolute()) else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Whether `name` can name an icon theme: one path component, so looking it
/// up can never leave the directories themes are searched in.
pub fn valid_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
}

/// Whether `name` can name an icon: letters, digits and `_.+-`, not starting
/// with a dot. Checked again here although the config's own check refuses
/// anything else, because a config with errors still loads, and a name is
/// joined onto directories.
fn valid_icon_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// Whether the icon at `path` is a symbolic one, drawn in one colour for the
/// desktop to recolour. By name, as GTK decides it: whatever colour the file
/// happens to use -- #2e3436, #808080, "gray" -- is a placeholder.
pub fn is_symbolic(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with("-symbolic"))
}

/// One kind of directory in an icon theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Drawn for one size.
    Fixed,
    /// Vector drawings good between a minimum and a maximum size.
    Scalable,
    /// Drawn for one size, and used for sizes near it.
    Threshold,
}

/// A directory an index.theme lists, and what it says the icons in it are.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Dir {
    /// Relative to the theme's directory.
    path: String,
    size: u32,
    scale: u32,
    kind: Kind,
    min_size: u32,
    max_size: u32,
}

impl Dir {
    fn parse(path: &str, keys: &HashMap<&str, &str>) -> Option<Dir> {
        // A listed directory stays inside the theme: no `..`, nothing
        // absolute.
        let inside = Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
        if path.is_empty() || !inside {
            return None;
        }
        let number = |key: &str| keys.get(key).and_then(|v| v.parse::<u32>().ok());
        // The one key the icon theme spec requires; a directory without it
        // is broken.
        let size = number("Size")?;
        let kind = match keys.get("Type").map(|t| t.to_ascii_lowercase()).as_deref() {
            Some("fixed") => Kind::Fixed,
            Some("scalable") => Kind::Scalable,
            _ => Kind::Threshold,
        };
        Some(Dir {
            path: path.to_string(),
            size,
            scale: number("Scale").unwrap_or(1).max(1),
            kind,
            min_size: number("MinSize").unwrap_or(size),
            max_size: number("MaxSize").unwrap_or(size),
        })
    }

    /// Pixels across an icon here is drawn for.
    fn pixels(&self) -> u32 {
        self.size.saturating_mul(self.scale)
    }

    /// Whether a scalable icon here is meant to be drawn `target` pixels
    /// across.
    fn covers(&self, target: u32) -> bool {
        let scale = self.scale;
        (self.min_size.saturating_mul(scale)..=self.max_size.saturating_mul(scale))
            .contains(&target)
    }
}

/// What an index.theme says.
#[derive(Debug, Default, PartialEq, Eq)]
struct Index {
    parents: Vec<String>,
    dirs: Vec<Dir>,
}

/// Read an index.theme: the themes it inherits from and its directories.
///
/// Only what lookup needs. Only the directories listed under `Directories`
/// and `ScaledDirectories` count, as the icon theme spec says; a theme's
/// other directories are not searched.
fn parse_index(text: &str) -> Index {
    let mut sections: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    let mut section = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(name.trim());
            continue;
        }
        if let (Some(section), Some((key, value))) = (section, line.split_once('=')) {
            // The first of a repeated key wins, so a later copy cannot
            // quietly change what was already read.
            sections
                .entry(section)
                .or_default()
                .entry(key.trim())
                .or_insert(value.trim());
        }
    }

    let head = sections.get("Icon Theme");
    let list = |key: &str| {
        head.and_then(|keys| keys.get(key))
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|item| !item.is_empty())
    };
    let parents = list("Inherits")
        .filter(|name| valid_theme_name(name))
        .take(MAX_PARENTS)
        .map(String::from)
        .collect();
    let mut listed = HashSet::new();
    let dirs = list("Directories")
        .chain(list("ScaledDirectories"))
        .filter(|path| listed.insert(*path))
        .take(MAX_DIRECTORIES)
        .filter_map(|path| Dir::parse(path, sections.get(path)?))
        .collect();
    Index { parents, dirs }
}

/// An installed icon theme.
#[derive(Clone, Debug)]
struct Theme {
    name: String,
    /// The theme's directory under each root that has one, in root order.
    /// One theme can be spread over several: an app installs its icon in
    /// ~/.local/share/icons/hicolor, and the rest of hicolor is in
    /// /usr/share/icons.
    homes: Vec<PathBuf>,
    parents: Vec<String>,
    dirs: Vec<Dir>,
}

impl Theme {
    /// The theme called `name`, if one is installed: a directory of that
    /// name under some root with an index.theme in it. The first index.theme
    /// found describes the theme, as the icon theme spec says.
    fn find(roots: &[PathBuf], name: &str) -> Option<Theme> {
        let homes: Vec<PathBuf> = roots
            .iter()
            .map(|root| root.join(name))
            .filter(|home| home.is_dir())
            .collect();
        let index = homes.iter().find_map(|home| {
            let path = home.join("index.theme");
            if !is_file(&path) {
                return None;
            }
            read_file(&path, MAX_INDEX_BYTES)
                .map_err(|e| log::warn!("icon theme {}: {e}", path.display()))
                .ok()
        })?;
        let index = parse_index(&String::from_utf8_lossy(&index));
        Some(Theme {
            name: name.to_string(),
            homes,
            parents: index.parents,
            dirs: index.dirs,
        })
    }

    /// The file for icon `name` in this theme closest to `target` pixels
    /// that is `usable`.
    ///
    /// A scalable SVG first, since it draws sharp at any size. Otherwise the
    /// smallest drawn at `target` or larger, so it is only ever shrunk, and
    /// failing that the largest there is. One that is not usable is passed
    /// over for the next best: Yaru has resources-symbolic in a directory of
    /// icons drawn with filters, and again in one without.
    fn find_icon(
        &self,
        name: &str,
        target: u32,
        usable: &mut dyn FnMut(&Path) -> bool,
    ) -> Option<PathBuf> {
        let svg = format!("{name}.svg");
        let png = format!("{name}.png");
        let mut scalable = Vec::new();
        // Ranked by: too small, then how far from the target, then PNG after
        // SVG. Lowest first.
        let mut sized: Vec<((bool, u32, bool), PathBuf)> = Vec::new();
        for dir in &self.dirs {
            for home in &self.homes {
                let base = home.join(&dir.path);
                for (file, is_png) in [(&svg, false), (&png, true)] {
                    let path = base.join(file);
                    if !is_file(&path) {
                        continue;
                    }
                    if dir.kind == Kind::Scalable && !is_png {
                        if !dir.covers(target) {
                            scalable.push(path);
                        } else if usable(&path) {
                            return Some(path);
                        }
                        continue;
                    }
                    let pixels = dir.pixels();
                    sized.push(((pixels < target, pixels.abs_diff(target), is_png), path));
                }
            }
        }
        // Stable, so of two ranked alike the one found first comes first.
        sized.sort_by_key(|(rank, _)| *rank);
        scalable
            .into_iter()
            .chain(sized.into_iter().map(|(_, path)| path))
            .find(|path| usable(path))
    }
}

/// The themes to search in order, from the chosen one: it and what it
/// inherits, depth first, then Adwaita and what it inherits, then hicolor.
///
/// Themes that are not installed are skipped, and so is anything already
/// visited, so a theme inheriting from itself through others ends. hicolor
/// is held back to the end: Yaru names it, so a plain depth-first walk from
/// Yaru-prussiangreen-dark would search hicolor, the last resort, before
/// Yaru-dark, the dark accent's next parent.
fn chain(roots: &[PathBuf], first: Option<&str>) -> Vec<Theme> {
    /// Add `name` and what it inherits, until `chain` is `limit` long.
    fn walk(
        roots: &[PathBuf],
        name: &str,
        depth: usize,
        limit: usize,
        seen: &mut HashSet<String>,
        chain: &mut Vec<Theme>,
    ) {
        if depth > MAX_INHERIT_DEPTH
            || chain.len() >= limit
            || name == LAST_THEME
            || !seen.insert(name.to_string())
        {
            return;
        }
        let Some(theme) = Theme::find(roots, name) else {
            log::debug!("icon theme {name} is not installed; skipped");
            return;
        };
        let parents = theme.parents.clone();
        chain.push(theme);
        for parent in &parents {
            walk(roots, parent, depth + 1, limit, seen, chain);
        }
    }

    let mut themes = Vec::new();
    let mut seen = HashSet::new();
    if let Some(first) = first {
        walk(roots, first, 0, MAX_THEMES, &mut seen, &mut themes);
    }
    // Room of its own, so a long chain cannot crowd out the fallback.
    let limit = themes.len() + MAX_THEMES;
    walk(roots, FALLBACK_THEME, 0, limit, &mut seen, &mut themes);
    themes.extend(Theme::find(roots, LAST_THEME));
    themes
}

/// Names already looked up, for a box's width and height, and which have
/// been warned about.
#[derive(Debug, Default)]
struct Found {
    paths: HashMap<(String, u32, u32), Result<PathBuf, Missing>>,
    warned: HashSet<String>,
}

/// Why an icon name has no file that can be drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    /// No theme has a file by that name, with `-symbolic` or without.
    NotFound,
    /// Every file by that name is refused: the first found, and why.
    Refused { path: PathBuf, reason: String },
}

/// The icon themes a name is looked up in, fixed until the next reload.
///
/// Built once per reload: finding the themes reads every index.theme in the
/// chain, and hicolor's alone lists 649 directories. Lookups are remembered
/// too, since a name can mean thousands of `stat`s before it is found or
/// given up on. Shareable between threads, so the list of names can be made
/// off the engine thread.
#[derive(Debug)]
pub struct IconThemes {
    roots: Vec<PathBuf>,
    themes: Vec<Theme>,
    found: Mutex<Found>,
    /// Whether the app logos are among the roots, so their symbolic names
    /// are suggested along with the themes'.
    logos: bool,
}

impl IconThemes {
    /// The themes under `roots`, starting from `theme`; Adwaita then hicolor
    /// when that is `None`, not installed, or not a usable name.
    pub fn load(roots: Vec<PathBuf>, theme: Option<&str>) -> IconThemes {
        let theme = theme.filter(|name| usable_theme(name, "the chosen"));
        let themes = chain(&roots, theme);
        IconThemes {
            roots,
            themes,
            found: Mutex::default(),
            logos: false,
        }
    }

    /// The themes for this desktop: `configured` if the config names one,
    /// else whatever the desktop uses, searched for where the environment
    /// says. The app logos compiled in are searched last of all, as icons
    /// in no theme; see [`crate::logos`].
    ///
    /// Asks the settings portal when nothing usable is configured, which can
    /// take a second, so this belongs at start and reload rather than
    /// anywhere a key is waiting to be drawn.
    pub fn discover(configured: Option<&str>) -> IconThemes {
        let configured = configured.filter(|name| usable_theme(name, "the configured"));
        let desktop = match configured {
            Some(_) => None,
            None => desktop_theme(),
        };
        let mut roots = search_roots();
        let logos = crate::logos::dir();
        roots.extend(logos.clone());
        let mut themes = IconThemes::load(roots, configured.or(desktop.as_deref()));
        themes.logos = logos.is_some();
        log::debug!(
            "icon themes, in order: {}",
            themes.theme_names().join(" > ")
        );
        themes
    }

    /// The installed themes that are searched, in order.
    pub fn theme_names(&self) -> Vec<&str> {
        self.themes
            .iter()
            .map(|theme| theme.name.as_str())
            .collect()
    }

    /// The file for icon `name`, to be drawn in a `width` x `height` box.
    ///
    /// Searched for in each theme in turn, and if it is in none of them,
    /// searched for again with `-symbolic` added or taken away: many icons
    /// now ship only in their symbolic form -- network-wireless is in
    /// neither Yaru nor Adwaita, network-wireless-symbolic is in both -- and
    /// a name typed from memory often has the suffix wrong. A file that
    /// could not be drawn in the box is passed over for the next one found.
    /// Remembered until the next reload, found or not; a name with nothing
    /// to draw is warned about once.
    pub fn resolve(&self, name: &str, size: (u32, u32)) -> Option<PathBuf> {
        self.lookup(name, size).ok()
    }

    /// [`IconThemes::resolve`], saying why there is nothing to draw.
    pub fn lookup(&self, name: &str, (width, height): (u32, u32)) -> Result<PathBuf, Missing> {
        if !valid_icon_name(name) {
            return Err(Missing::NotFound);
        }
        let key = (name.to_string(), width, height);
        if let Some(found) = self.lock().paths.get(&key) {
            return found.clone();
        }
        let mut refused = None;
        let mut path = self.find(name, (width, height), &mut refused);
        // Only a name found nowhere is tried under its other name. One found
        // and refused is what was asked for, and the other is another icon.
        if path.is_none() && refused.is_none() {
            let other = match name.strip_suffix("-symbolic") {
                Some("") => None,
                Some(plain) => Some(plain.to_string()),
                None => Some(format!("{name}-symbolic")),
            };
            if let Some(other) = other {
                path = self.find(&other, (width, height), &mut refused);
            }
        }
        let result = match (path, refused) {
            (Some(path), _) => Ok(path),
            (None, Some((path, reason))) => Err(Missing::Refused { path, reason }),
            (None, None) => Err(Missing::NotFound),
        };

        let mut found = self.lock();
        if let Err(missing) = &result {
            if found.warned.insert(name.to_string()) {
                match missing {
                    Missing::NotFound => log::warn!(
                        "no icon called {name} in the icon themes {}",
                        self.theme_names().join(", ")
                    ),
                    Missing::Refused { path, reason } => {
                        log::warn!(
                            "icon {name} is {}, which cannot be drawn: {reason}",
                            path.display()
                        )
                    }
                }
            }
        }
        // Bounded: a config being edited can name a new icon on every save.
        // A name forgotten here is warned about again, once.
        if found.paths.len() >= MAX_REMEMBERED {
            found.paths.clear();
        }
        if found.warned.len() >= MAX_REMEMBERED {
            found.warned.clear();
        }
        found.paths.insert(key, result.clone());
        result
    }

    /// The first file for `name` that can be drawn in a box of `size`, from
    /// the themes in turn and then the roots. The first file that cannot be
    /// drawn is left in `refused`, with why, unless one already is.
    fn find(
        &self,
        name: &str,
        size: (u32, u32),
        refused: &mut Option<(PathBuf, String)>,
    ) -> Option<PathBuf> {
        let mut tried = 0;
        let mut usable = |path: &Path| {
            if tried >= MAX_TRIED {
                return false;
            }
            tried += 1;
            match check(path, size) {
                Ok(()) => true,
                Err(reason) => {
                    log::debug!("icon {name}: {} is passed over: {reason}", path.display());
                    refused.get_or_insert_with(|| (path.to_path_buf(), reason));
                    false
                }
            }
        };
        let target = size.0.min(size.1);
        self.themes
            .iter()
            .find_map(|theme| theme.find_icon(name, target, &mut usable))
            .or_else(|| self.find_unthemed(name, &mut usable))
    }

    /// An icon not in any theme: a file of that name straight in one of the
    /// roots, which is where the icon theme spec says an app with no themed
    /// icon may put one -- /usr/share/pixmaps, mostly.
    fn find_unthemed(&self, name: &str, usable: &mut dyn FnMut(&Path) -> bool) -> Option<PathBuf> {
        self.roots
            .iter()
            .flat_map(|root| {
                [
                    root.join(format!("{name}.svg")),
                    root.join(format!("{name}.png")),
                ]
            })
            .find(|path| is_file(path) && usable(path))
    }

    /// Every symbolic icon's name in the themes, and the app logos', sorted:
    /// what an icon field suggests. Reads every listed directory, so
    /// this is for a thread of its own, once per reload.
    pub fn symbolic_names(&self) -> Vec<String> {
        self.symbolic_names_up_to(MAX_ICON_NAMES)
    }

    fn symbolic_names_up_to(&self, limit: usize) -> Vec<String> {
        let mut names = BTreeSet::new();
        if self.logos {
            names.extend(crate::logos::symbolic_names());
        }
        let mut looked_at = 0;
        'themes: for theme in &self.themes {
            for dir in &theme.dirs {
                for home in &theme.homes {
                    let Ok(entries) = std::fs::read_dir(home.join(&dir.path)) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        looked_at += 1;
                        if looked_at > MAX_LISTED_FILES {
                            break 'themes;
                        }
                        let file = entry.file_name();
                        let Some(file) = file.to_str() else {
                            continue;
                        };
                        let stem = file
                            .strip_suffix(".svg")
                            .or_else(|| file.strip_suffix(".png"));
                        if let Some(stem) = stem {
                            if stem.ends_with("-symbolic") && valid_icon_name(stem) {
                                names.insert(stem.to_string());
                            }
                        }
                    }
                }
            }
        }
        names.into_iter().take(limit).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Found> {
        self.found.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where an icon file came from, which decides whether a small picture is
/// blown up to fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// A file the config names. Fitted inside the box but never enlarged:
    /// a 16x16 picture blown up to fill a key looks worse than left small.
    File,
    /// Found by name in an icon theme. Fitted to the box either way: the
    /// theme was asked for the size nearest the box, and the icon should be
    /// the same size on every key whichever size the theme had.
    Theme,
}

/// The icon file at `path`, drawn to fit a `width` x `height` box and
/// recoloured with `tint` if one is given.
///
/// An SVG is drawn at the size of the box. A PNG, JPEG or GIF is decoded and
/// scaled; see [`Origin`]. Either keeps its own aspect ratio, so one side
/// can come out shorter than the box.
pub fn load(
    path: &Path,
    (width, height): (u32, u32),
    origin: Origin,
    tint: Option<Rgb>,
) -> Result<RgbaImage, String> {
    check_box(width, height)?;
    let mut image = if is_svg(path) {
        load_svg(path, width, height)?
    } else {
        load_raster(path, width, height, origin)?
    };
    if let Some(color) = tint {
        self::tint(&mut image, color);
    }
    Ok(image)
}

/// Refuse what [`load`] would refuse for the icon file at `path` in a
/// `width` x `height` box, short of drawing or decoding it: an SVG is
/// parsed and its cost counted, a picture's header read.
fn check(path: &Path, (width, height): (u32, u32)) -> Result<(), String> {
    check_box(width, height)?;
    if is_svg(path) {
        prepare_svg(path, width, height).map(drop)
    } else {
        check_raster(path)
    }
}

/// Refuse a box with no room in it, or one larger than any key.
fn check_box(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("there is no room for it".into());
    }
    if width > MAX_BOX_SIDE || height > MAX_BOX_SIDE {
        return Err(format!("{width}x{height} is larger than any key"));
    }
    Ok(())
}

/// Whether `path` is drawn as an SVG, by its name.
fn is_svg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
}

/// Recolour a symbolic icon: keep how much of each pixel it covers, and
/// replace whatever colour it was drawn in with `color`.
///
/// Rather than restyling the SVG the way GTK 3 does: in resvg, a stylesheet
/// setting `fill` misses strokes, so the strike through night-light-disabled
/// stayed dark grey and vanished on a dark key, and an inline `style=` on
/// the element beats the sheet anyway. Coverage is right whatever the file
/// did, opacity included. Parts GTK would colour as warnings come out in the
/// one colour.
pub fn tint(image: &mut RgbaImage, color: Rgb) {
    for pixel in image.pixels_mut() {
        let alpha = pixel.0[3];
        pixel.0 = [color.r, color.g, color.b, alpha];
    }
}

/// Draw an SVG to fit the box.
fn load_svg(path: &Path, width: u32, height: u32) -> Result<RgbaImage, String> {
    let started = Instant::now();
    let svg = prepare_svg(path, width, height)?;
    let (out_width, out_height) = (svg.width, svg.height);
    let mut pixmap = tiny_skia::Pixmap::new(out_width, out_height)
        .ok_or_else(|| format!("cannot draw at {out_width}x{out_height}"))?;
    resvg::render(&svg.tree, svg.transform, &mut pixmap.as_mut());

    let took = started.elapsed();
    if took > SLOW_DRAWING {
        log::warn!(
            "icon {} took {} ms to draw; a simpler icon would keep page changes quick",
            path.display(),
            took.as_millis()
        );
    }
    RgbaImage::from_raw(out_width, out_height, pixmap.take_demultiplied())
        .ok_or_else(|| "the drawing came out the wrong size".into())
}

/// An SVG parsed and found fit to draw in a box.
struct Prepared {
    tree: usvg::Tree,
    /// The size of the drawing: the box, less what keeping the SVG's
    /// aspect ratio leaves out.
    width: u32,
    height: u32,
    /// What fits the SVG into that.
    transform: tiny_skia::Transform,
}

/// Read and parse the SVG at `path`, and refuse it if drawing it to fit a
/// `width` x `height` box would take more than an icon may; everything short
/// of drawing it.
fn prepare_svg(path: &Path, width: u32, height: u32) -> Result<Prepared, String> {
    let bytes = read_file(path, MAX_SVG_BYTES)?;
    check_svg(&bytes)?;
    // Parsed here rather than by usvg, so it can be sized before usvg
    // builds anything from it.
    let text = std::str::from_utf8(&bytes).map_err(|_| "it is not UTF-8 text".to_string())?;
    let document = usvg::roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    match expanded_size(&document) {
        Some(size) if size.elements > MAX_SVG_ELEMENTS => {
            return Err(format!(
                "it comes to {} elements once what it refers to is counted, and an icon may \
                 come to at most {MAX_SVG_ELEMENTS}",
                size.elements
            ))
        }
        Some(size) if size.data > MAX_SVG_DATA => {
            return Err(format!(
                "it comes to {} bytes of path data once what it refers to is counted, and an \
                 icon may come to at most {MAX_SVG_DATA}",
                size.data
            ))
        }
        Some(_) => {}
        None => return Err(format!("it nests deeper than {MAX_SVG_DEPTH} levels")),
    }
    let tree = usvg::Tree::from_xmltree(&document, &svg_options()).map_err(|e| e.to_string())?;

    // The pixmap is the box, never the size the file declares: an SVG saying
    // width="100000" is drawn small, not allocated large.
    let size = tree.size();
    let scale = (width as f32 / size.width()).min(height as f32 / size.height());
    // A size so small that fitting it overflows leaves nothing to draw.
    if !scale.is_finite() {
        return Err("it is too small to draw".into());
    }
    let fitted = |side: f32, most: u32| ((side * scale).round() as u32).clamp(1, most);
    let (out_width, out_height) = (fitted(size.width(), width), fitted(size.height(), height));
    // Centred, for the part of a pixel lost to rounding.
    let transform = tiny_skia::Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        (out_width as f32 - size.width() * scale) / 2.0,
        (out_height as f32 - size.height() * scale) / 2.0,
    );

    let mut cost = Cost {
        scale,
        pixels: out_width as f32 * out_height as f32,
        across: (LARGEST_LAYER as f32).sqrt() * (out_width + out_height) as f32,
        ..Cost::default()
    };
    cost.add(tree.root(), transform, true)?;
    if cost.filter_steps > MAX_FILTER_STEPS {
        return Err(format!(
            "it runs {} filter steps, and an icon may run at most {MAX_FILTER_STEPS}",
            cost.filter_steps
        ));
    }
    if cost.layers > MAX_LAYERS {
        return Err(format!(
            "it draws {} layers, and an icon may draw at most {MAX_LAYERS}",
            cost.layers
        ));
    }
    if cost.edge_pixels.is_nan() || cost.edge_pixels > MAX_EDGE_PIXELS {
        return Err(format!(
            "its paths come to {:.0} pixels of edge to draw, and an icon's may come to at most \
             {MAX_EDGE_PIXELS}",
            cost.edge_pixels
        ));
    }
    if cost.filled.is_nan() || cost.filled > MAX_FILLED {
        return Err(format!(
            "it fills {:.0} times the icon's size, and an icon may fill at most {MAX_FILLED}",
            cost.filled
        ));
    }
    Ok(Prepared {
        tree,
        width: out_width,
        height: out_height,
        transform,
    })
}

/// Refuse what the parser would otherwise take on trust.
///
/// Gzip is refused outright rather than decompressed, since a few KB can
/// inflate to gigabytes. A DOCTYPE is how an SVG declares entities, which
/// the parser expands; no installed theme needs one but four of Yaru's snap
/// icons, which carry the standard one without using it.
fn check_svg(bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Err("it is compressed (an .svgz); only plain SVG is read".into());
    }
    let declares = |what: &[u8]| {
        bytes
            .windows(what.len())
            .any(|window| window.eq_ignore_ascii_case(what))
    };
    if declares(b"<!doctype") || declares(b"<!entity") {
        return Err("it has a DOCTYPE or ENTITY declaration, which icons may not".into());
    }
    Ok(())
}

/// How an SVG may be parsed.
///
/// An `<image>` naming a file is not followed: the default would read any
/// path at all, whole and uncapped, so `href="/dev/zero"` would eat memory
/// until the daemon died and a FIFO would stop the engine thread for good.
/// Pictures embedded as `data:` are kept, since Adwaita's legacy icons have
/// them -- though with resvg's picture decoders left out they are not drawn.
/// An SVG embedded that way is not kept: it would escape every check made on
/// this one.
fn svg_options() -> usvg::Options<'static> {
    usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, data, _| match image::guess_format(&data) {
                Ok(image::ImageFormat::Png) => Some(usvg::ImageKind::PNG(data)),
                Ok(image::ImageFormat::Jpeg) => Some(usvg::ImageKind::JPEG(data)),
                Ok(image::ImageFormat::Gif) => Some(usvg::ImageKind::GIF(data)),
                _ => None,
            }),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    }
}

/// What drawing a parsed SVG takes, counted before it is drawn: its drawing
/// time grows with these, not with the file's size.
#[derive(Debug, Default)]
struct Cost {
    /// How much the drawing is scaled to fit the box.
    scale: f32,
    /// The box's area in pixels.
    pixels: f32,
    /// Filter primitives run, weighted by how much work each is.
    filter_steps: usize,
    /// Layers drawn, in boxes; see [`MAX_LAYERS`].
    layers: usize,
    /// Path segments drawn; see [`MAX_SEGMENTS`].
    segments: f64,
    /// The segments of each path drawn, squared; see
    /// [`MAX_SEGMENTS_SQUARED`].
    segments_squared: f64,
    /// The most segments one path drawn has.
    longest: f64,
    /// Gradient work, in boxes filled times stops; see
    /// [`MAX_GRADIENT_WORK`].
    gradient_work: f64,
    /// How far across the largest layer is, in pixels: no edge is walked
    /// further than that, however long it is.
    across: f32,
    /// Edge drawn, in pixels; see [`MAX_EDGE_PIXELS`].
    edge_pixels: f64,
    /// What fills and strokes cover, in boxes; see [`MAX_FILLED`].
    filled: f64,
}

impl Cost {
    /// Add what drawing `group` takes: once for every element a filter, clip
    /// path, mask, pattern or gradient is applied to, including those inside
    /// masks, clip paths and patterns, since each use is drawn again. One
    /// blur applied to a thousand elements is a thousand blurs.
    ///
    /// `transform` is what resvg draws `group` with, before the group's own:
    /// how large a pattern's tile is drawn depends on it. `placed` is whether
    /// usvg knows where `group` lands on the pixmap. It does not inside a
    /// mask, clip path or pattern, which are drawn wherever they are used, so
    /// a layer there counts as large as one can be.
    fn add(
        &mut self,
        group: &usvg::Group,
        transform: tiny_skia::Transform,
        placed: bool,
    ) -> Result<(), String> {
        // As resvg draws it: what follows is transformed the way resvg
        // transforms it, so the sizes it allocates are the sizes counted.
        let transform = transform.pre_concat(group.transform());
        let boxes = if placed {
            let bounds = group.abs_layer_bounding_box();
            let pixels = bounds.width() * self.scale * bounds.height() * self.scale;
            let boxes = (pixels / self.pixels).ceil();
            if boxes.is_nan() {
                LARGEST_LAYER
            } else {
                boxes.clamp(1.0, LARGEST_LAYER as f32) as usize
            }
        } else {
            LARGEST_LAYER
        };
        if group.should_isolate() {
            self.layers = self.layers.saturating_add(boxes);
        }
        for filter in group.filters() {
            for primitive in filter.primitives() {
                let steps = filter_steps(primitive.kind())?;
                self.filter_steps = self.filter_steps.saturating_add(steps);
                if let usvg::filter::Kind::Image(image) = primitive.kind() {
                    let (sx, sy) = transform.get_scale();
                    let scaled = tiny_skia::Transform::from_scale(sx, sy);
                    self.add(image.root(), scaled, false)?;
                }
            }
        }
        // A clip path or mask is drawn into a pixmap of the group's layer.
        // It can have its own, and so on: each one in the chain is drawn,
        // though usvg's `subroots` stops after the second.
        let mut clip = group.clip_path();
        while let Some(clip_path) = clip {
            self.layers = self.layers.saturating_add(boxes);
            let clipping = transform.pre_concat(clip_path.transform());
            self.add(clip_path.root(), clipping, false)?;
            clip = clip_path.clip_path();
        }
        let mut mask = group.mask();
        while let Some(this) = mask {
            self.layers = self.layers.saturating_add(boxes);
            self.add(this.root(), transform, false)?;
            mask = this.mask();
        }
        for node in group.children() {
            match node {
                usvg::Node::Group(child) => self.add(child, transform, placed)?,
                usvg::Node::Path(path) => self.add_path(path, transform)?,
                // A nested SVG image is never kept, and text is not built.
                _ => {
                    let mut added = Ok(());
                    node.subroots(|root| {
                        if added.is_ok() {
                            added = self.add(root, transform, false);
                        }
                    });
                    added?;
                }
            }
        }
        Ok(())
    }

    /// Add what filling and stroking `path` takes, drawn with `transform`.
    fn add_path(
        &mut self,
        path: &usvg::Path,
        transform: tiny_skia::Transform,
    ) -> Result<(), String> {
        if !path.is_visible() {
            return Ok(());
        }
        let segments = path.data().verbs().len() as f64;
        if let Some(fill) = path.fill() {
            self.add_segments(segments)?;
            self.add_drawn(path.data(), path.bounding_box(), transform, None)?;
            self.add_paint(fill.paint(), path.bounding_box(), transform)?;
        }
        if let Some(stroke) = path.stroke() {
            // The dashes are the path's, drawn with it. Counting them walks
            // the path, which is not done once there is too much to draw.
            let dashes = match stroke.dasharray() {
                Some(dashes) if self.segments + segments <= MAX_SEGMENTS => {
                    dash_count(path.data(), dashes)
                }
                _ => 0.0,
            };
            self.add_segments(segments + dashes)?;
            // The outline turns at the end of every segment and dash.
            let ends = segments + 2.0 * dashes;
            let bounds = path.stroke_bounding_box();
            self.add_drawn(path.data(), bounds, transform, Some((stroke, ends)))?;
            self.add_paint(stroke.paint(), bounds, transform)?;
        }
        Ok(())
    }

    /// Add one drawing of `path`, a fill or, given its stroke and how many
    /// ends its outline turns at, a stroke, covering `bounds` drawn with
    /// `transform`: the edges tiny-skia walks and the pixels it covers. One
    /// reaching further out than tiny-skia draws reliably is refused.
    fn add_drawn(
        &mut self,
        path: &tiny_skia::Path,
        bounds: usvg::Rect,
        transform: tiny_skia::Transform,
        stroke: Option<(&usvg::Stroke, f64)>,
    ) -> Result<(), String> {
        let (sx, sy) = transform.get_scale();
        let resolution = sx.max(sy);
        // A stroke's outline is worked out before it is transformed, to a
        // precision set by how much it is scaled, which f32 does not have
        // far from where the path is drawn from.
        let (outlined, most) = match stroke {
            Some(_) => (farthest(bounds) * resolution, MAX_STROKE_REACH),
            None => (0.0, MAX_FILL_REACH),
        };
        let drawn = bounds.transform(transform);
        let reach = drawn.map_or(f32::INFINITY, |drawn| farthest(drawn).max(outlined));
        if reach.is_nan() || reach > most {
            let how = if stroke.is_some() { "stroke" } else { "fill" };
            return Err(format!(
                "it draws a {how} reaching {reach:.0} pixels out, and an icon's may reach at \
                 most {most}"
            ));
        }

        let mut edges = drawn_length(path, transform, self.across);
        if let Some((stroke, ends)) = stroke {
            // Down both sides, and across the stroke at every end.
            let width = (stroke.width().get() * resolution).min(self.across);
            edges = 2.0 * edges + ends * f64::from(width);
        }
        self.edge_pixels += edges;
        // Covered no further than a layer reaches, however large the shape.
        let boxes = drawn.map_or(0.0, |drawn| drawn.width() * drawn.height() / self.pixels);
        self.filled += f64::from(boxes).min(LARGEST_LAYER as f64);
        Ok(())
    }

    /// Add one drawing, a fill or a stroke, of a path of `segments`,
    /// refusing once there is more than an icon may draw.
    fn add_segments(&mut self, segments: f64) -> Result<(), String> {
        self.segments += segments;
        self.segments_squared += segments * segments;
        self.longest = self.longest.max(segments);
        if self.segments.is_nan() || self.segments > MAX_SEGMENTS {
            return Err(format!(
                "it draws more than {MAX_SEGMENTS} path segments, counting each dash as one"
            ));
        }
        if self.segments_squared.is_nan() || self.segments_squared > MAX_SEGMENTS_SQUARED {
            return Err(format!(
                "its paths are too long to draw quickly: the longest has {:.0} segments, \
                 counting each dash as one",
                self.longest
            ));
        }
        Ok(())
    }

    /// Add what filling `bounds` with `paint`, drawn with `transform`,
    /// takes beyond a plain colour: a gradient looks through its stops for
    /// every pixel it fills, and a pattern draws its tile first.
    fn add_paint(
        &mut self,
        paint: &usvg::Paint,
        bounds: usvg::Rect,
        transform: tiny_skia::Transform,
    ) -> Result<(), String> {
        let stops = match paint {
            usvg::Paint::Color(_) => return Ok(()),
            usvg::Paint::Pattern(pattern) => return self.add_pattern(pattern, transform),
            usvg::Paint::LinearGradient(gradient) => gradient.stops().len(),
            usvg::Paint::RadialGradient(gradient) => gradient.stops().len(),
        };
        // Filled no further than a layer reaches, however large the shape.
        let boxes = bounds
            .transform(transform)
            .map_or(0.0, |drawn| drawn.width() * drawn.height() / self.pixels);
        let boxes = f64::from(boxes).min(LARGEST_LAYER as f64);
        self.gradient_work += boxes * (stops as f64 + GRADIENT_SETUP);
        if self.gradient_work.is_nan() || self.gradient_work > MAX_GRADIENT_WORK {
            return Err(format!(
                "its gradients come to {:.0} stops times the boxes they fill, and an icon's \
                 may come to at most {MAX_GRADIENT_WORK}",
                self.gradient_work
            ));
        }
        Ok(())
    }

    /// Add what filling with `pattern`, drawn with `transform`, takes: its
    /// tile is drawn first, into a pixmap of its own, every time the pattern
    /// is used, at the size resvg works out here.
    fn add_pattern(
        &mut self,
        pattern: &usvg::Pattern,
        transform: tiny_skia::Transform,
    ) -> Result<(), String> {
        let (sx, sy) = transform.pre_concat(pattern.transform()).get_scale();
        let rect = pattern.rect();
        let (width, height) = ((rect.width() * sx).round(), (rect.height() * sy).round());
        // Too small to be a pixel: resvg draws nothing.
        if width < 1.0 || height < 1.0 {
            return Ok(());
        }
        let boxes = width * height / self.pixels;
        // One tile larger than a layer can be, or a size that is not one.
        if boxes.is_nan() || boxes > LARGEST_LAYER as f32 {
            return Err(format!(
                "it fills with a pattern drawn {width:.0}x{height:.0} pixels at a time, larger \
                 than an icon may draw one"
            ));
        }
        self.layers = self.layers.saturating_add(boxes.ceil() as usize);
        // resvg draws the tile's content scaled, and nothing more.
        let scaled = tiny_skia::Transform::from_scale(sx, sy);
        self.add(pattern.root(), scaled, false)
    }
}

/// How many dashes stroking `path` with `dashes` comes to, the way
/// tiny-skia works it out: the path's length times the dashes in the array,
/// over what the array's dashes and gaps add up to. The length is taken
/// along the control points, which is never less than along the curve.
fn dash_count(path: &tiny_skia::Path, dashes: &[f32]) -> f64 {
    let interval: f64 = dashes.iter().map(|&dash| f64::from(dash)).sum();
    let mut length = 0.0;
    legs(path, |from, to| length += f64::from(from.distance(to)));
    // Pairs of dash and gap; usvg has made the array even. An interval of
    // nothing at all would never end, and comes out as more than any cap.
    length * (dashes.len() / 2) as f64 / interval
}

/// How long `path` is drawn with `transform`, in pixels along its control
/// points, counting no leg as longer than `longest`: an edge is walked only
/// where it crosses the layer it is drawn in.
fn drawn_length(path: &tiny_skia::Path, transform: tiny_skia::Transform, longest: f32) -> f64 {
    let mut length = 0.0;
    legs(path, |mut from, mut to| {
        transform.map_point(&mut from);
        transform.map_point(&mut to);
        length += f64::from(from.distance(to).min(longest));
    });
    length
}

/// Call `leg` with each leg of `path` along its control points, from one
/// point to the next, closing each contour that is closed.
fn legs(path: &tiny_skia::Path, mut leg: impl FnMut(tiny_skia::Point, tiny_skia::Point)) {
    use tiny_skia::PathSegment;
    let (mut start, mut at) = (tiny_skia::Point::zero(), tiny_skia::Point::zero());
    for segment in path.segments() {
        let (points, count) = match segment {
            PathSegment::MoveTo(to) => {
                (start, at) = (to, to);
                continue;
            }
            PathSegment::LineTo(to) => ([to; 3], 1),
            PathSegment::QuadTo(control, to) => ([control, to, to], 2),
            PathSegment::CubicTo(first, second, to) => ([first, second, to], 3),
            PathSegment::Close => ([start; 3], 1),
        };
        for &to in &points[..count] {
            leg(at, to);
            at = to;
        }
    }
}

/// The farthest `rect` reaches from the origin, along either axis.
fn farthest(rect: usvg::Rect) -> f32 {
    [rect.left(), rect.top(), rect.right(), rect.bottom()]
        .into_iter()
        .map(f32::abs)
        .fold(0.0, f32::max)
}

/// How many steps one filter primitive counts as. Most make one pass over
/// the filter's region; three do more, by an amount the file chooses.
fn filter_steps(kind: &usvg::filter::Kind) -> Result<usize, String> {
    use usvg::filter::Kind;
    Ok(match kind {
        // Its work grows with the square of its radius in drawn pixels, which
        // depends on transforms usvg does not report inside masks and
        // patterns, so it cannot be bounded beforehand: a radius of 100 took
        // longer than 30 s to draw. No installed icon uses it.
        Kind::Morphology(_) => {
            return Err("it uses feMorphology, which is too slow to draw on a key".into())
        }
        // A pass per octave: a thousand octaves took 19 s.
        Kind::Turbulence(turbulence) => turbulence.num_octaves().max(1) as usize,
        // Work per pixel for every cell of the matrix, so 3x3 is one step: a
        // 60x60 matrix took 4.4 s.
        Kind::ConvolveMatrix(convolve) => {
            let matrix = convolve.matrix();
            (matrix.columns() as usize)
                .saturating_mul(matrix.rows() as usize)
                .div_ceil(9)
                .max(1)
        }
        _ => 1,
    })
}

/// How many elements drawing `document` comes to, and how much path data,
/// counting what each one refers to again wherever it is drawn. `None` when
/// elements and references nest deeper than [`MAX_SVG_DEPTH`].
///
/// usvg copies what a `<use>` names, and converts a mask, clip path,
/// pattern or filter again for each element that uses it and a marker again
/// for each vertex it sits on. That happens while it parses, before its tree
/// could be counted, so this counts the XML. A pattern or marker set on a
/// group reaches every shape or vertex inside it. Gradients are left out:
/// they have no elements to copy. What a stylesheet refers to could apply to
/// any element, so it is counted against every one.
fn expanded_size(document: &usvg::roxmltree::Document) -> Option<Expanded> {
    let mut ids: HashMap<&str, Vec<usvg::roxmltree::Node>> = HashMap::new();
    for node in document.descendants().filter(|node| node.is_element()) {
        if let Some(id) = node.attribute("id") {
            ids.entry(id).or_default().push(node);
        }
    }
    let mut sheet: Vec<&str> = document
        .descendants()
        .filter(|node| node.has_tag_name("style"))
        .flat_map(|style| style.descendants().filter_map(|text| text.text()))
        .flat_map(url_ids)
        .collect();
    sheet.sort_unstable();
    sheet.dedup();

    // What the stylesheet refers to is sized first, leaving the stylesheet
    // out, then added to every element.
    let mut expansion = Expansion {
        ids,
        sizes: HashMap::new(),
        sheet: Expanded::default(),
        sheet_markers: Expanded::default(),
    };
    let (mut sheet_size, mut markers) = (Expanded::default(), Expanded::default());
    for id in sheet {
        for target in expansion.targets(id) {
            let size = expansion.size(target, 0)?;
            if target.has_tag_name("marker") {
                markers = markers.plus(size);
            } else {
                sheet_size = sheet_size.plus(size);
            }
        }
    }
    expansion.sizes.clear();
    expansion.sheet = sheet_size;
    expansion.sheet_markers = markers;
    expansion.size(document.root_element(), 0)
}

/// What an element comes to: its elements, and among them the shapes a
/// pattern set on it could fill and the vertices a marker could sit on; and
/// the bytes of path data they have.
#[derive(Clone, Copy, Debug, Default)]
struct Expanded {
    elements: usize,
    shapes: usize,
    vertices: usize,
    data: usize,
}

impl Expanded {
    fn plus(self, other: Expanded) -> Expanded {
        Expanded {
            elements: self.elements.saturating_add(other.elements),
            shapes: self.shapes.saturating_add(other.shapes),
            vertices: self.vertices.saturating_add(other.vertices),
            data: self.data.saturating_add(other.data),
        }
    }
}

/// How far a reference reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// A `<use>`: a copy, drawn as if it were the element's own children.
    Copy,
    /// A clip path, mask or filter: drawn once, for the element itself.
    Once,
    /// A fill or stroke, which what is inside the element inherits: once
    /// for each shape.
    Shapes,
    /// A marker, also inherited: once for each vertex.
    Vertices,
}

/// The sizes [`expanded_size`] has worked out so far.
struct Expansion<'a, 'input> {
    /// Every element with each id: usvg might take any of them, so the
    /// largest is counted.
    ids: HashMap<&'a str, Vec<usvg::roxmltree::Node<'a, 'input>>>,
    /// `None` while an element is being sized, so one that refers back to
    /// itself counts nothing the second time, as usvg drops such a loop.
    sizes: HashMap<usvg::roxmltree::NodeId, Option<Expanded>>,
    /// What the stylesheet refers to, added to every element.
    sheet: Expanded,
    /// The markers the stylesheet refers to, added at every vertex.
    sheet_markers: Expanded,
}

impl<'a, 'input> Expansion<'a, 'input> {
    /// The elements `id` could name that are worth counting.
    fn targets(&self, id: &str) -> Vec<usvg::roxmltree::Node<'a, 'input>> {
        self.ids
            .get(id)
            .into_iter()
            .flatten()
            .filter(|node| {
                !matches!(
                    node.tag_name().name(),
                    "linearGradient" | "radialGradient" | "stop"
                )
            })
            .copied()
            .collect()
    }

    /// The largest of what `id` could name, in each respect.
    fn largest(&mut self, id: &str, depth: usize) -> Option<Expanded> {
        let mut largest = Expanded::default();
        for target in self.targets(id) {
            let size = self.size(target, depth)?;
            largest = Expanded {
                elements: largest.elements.max(size.elements),
                shapes: largest.shapes.max(size.shapes),
                vertices: largest.vertices.max(size.vertices),
                data: largest.data.max(size.data),
            };
        }
        Some(largest)
    }

    fn size(&mut self, node: usvg::roxmltree::Node<'a, 'input>, depth: usize) -> Option<Expanded> {
        if depth > MAX_SVG_DEPTH {
            return None;
        }
        match self.sizes.get(&node.id()) {
            Some(Some(size)) => return Some(*size),
            Some(None) => return Some(Expanded::default()),
            None => {}
        }
        self.sizes.insert(node.id(), None);

        let own_vertices = vertices(node);
        let mut total = Expanded {
            elements: 1usize
                .saturating_add(self.sheet.elements)
                .saturating_add(self.sheet_markers.elements.saturating_mul(own_vertices)),
            shapes: usize::from(is_shape(node)),
            vertices: own_vertices,
            data: path_data(node)
                .len()
                .saturating_add(self.sheet.data)
                .saturating_add(self.sheet_markers.data.saturating_mul(own_vertices)),
        };
        for child in node.children().filter(|child| child.is_element()) {
            total = total.plus(self.size(child, depth + 1)?);
        }
        let references = references(node);
        // A copy first, since a pattern or marker set on a <use> reaches
        // the shapes it copies.
        for (id, _) in references.iter().filter(|(_, reach)| *reach == Reach::Copy) {
            total = total.plus(self.largest(id, depth + 1)?);
        }
        let (shapes, vertices) = (total.shapes.max(1), total.vertices);
        for (id, reach) in references {
            let times = match reach {
                Reach::Copy => continue,
                Reach::Once => 1,
                Reach::Shapes => shapes,
                Reach::Vertices => vertices,
            };
            let size = self.largest(id, depth + 1)?;
            total.elements = total
                .elements
                .saturating_add(size.elements.saturating_mul(times));
            total.data = total.data.saturating_add(size.data.saturating_mul(times));
        }

        self.sizes.insert(node.id(), Some(total));
        Some(total)
    }
}

/// The ids an element refers to, and how far each reaches: by
/// `href="#id"`, or by `url(#id)` in an attribute or a `style` declaration.
fn references<'a>(node: usvg::roxmltree::Node<'a, '_>) -> Vec<(&'a str, Reach)> {
    let mut ids = Vec::new();
    for attribute in node.attributes() {
        let (name, value) = (attribute.name(), attribute.value());
        if name == "href" {
            if let Some(id) = value.trim().strip_prefix('#') {
                let reach = if node.has_tag_name("use") {
                    Reach::Copy
                } else {
                    Reach::Once
                };
                ids.push((id, reach));
            }
        } else if name == "style" {
            for declaration in value.split(';') {
                let (property, value) = declaration.split_once(':').unwrap_or(("", declaration));
                let reach = reach(property.trim());
                ids.extend(url_ids(value).map(|id| (id, reach)));
            }
        } else {
            let reach = reach(name);
            ids.extend(url_ids(value).map(|id| (id, reach)));
        }
    }
    ids
}

/// How far a reference in `property` reaches.
fn reach(property: &str) -> Reach {
    match property {
        "fill" | "stroke" => Reach::Shapes,
        "marker" | "marker-start" | "marker-mid" | "marker-end" => Reach::Vertices,
        _ => Reach::Once,
    }
}

/// Whether a fill or stroke set on `node` or inherited by it is drawn.
fn is_shape(node: usvg::roxmltree::Node) -> bool {
    matches!(
        node.tag_name().name(),
        "path"
            | "rect"
            | "circle"
            | "ellipse"
            | "line"
            | "polyline"
            | "polygon"
            | "text"
            | "tspan"
            | "textPath"
    )
}

/// The ids named by `url(#id)` in `text`.
fn url_ids(text: &str) -> impl Iterator<Item = &str> {
    text.split("url(").skip(1).filter_map(|rest| {
        let id = rest.trim_start().trim_start_matches(['"', '\'']);
        let id = id.strip_prefix('#')?;
        let end = id
            .find(|c: char| c == ')' || c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(id.len());
        Some(&id[..end])
    })
}

/// How many places along an element a marker could be drawn, from its path
/// data without parsing it: at most one per command letter and per number,
/// counting each `.` as a number, since `1.5.5` is two.
fn vertices(node: usvg::roxmltree::Node) -> usize {
    if node.has_tag_name("line") {
        return 2;
    }
    let mut count = 0usize;
    let mut in_number = false;
    for byte in path_data(node).bytes() {
        let digit = byte.is_ascii_digit();
        if byte.is_ascii_alphabetic() || byte == b'.' || (digit && !in_number) {
            count += 1;
        }
        in_number = digit || byte == b'.';
    }
    count
}

/// The path data of a path, polyline or polygon: nothing for any other
/// element.
fn path_data<'a>(node: usvg::roxmltree::Node<'a, '_>) -> &'a str {
    let data = match node.tag_name().name() {
        "path" => node.attribute("d"),
        "polyline" | "polygon" => node.attribute("points"),
        _ => None,
    };
    data.unwrap_or("")
}

/// A reader for the PNG, JPEG or GIF at `path`, held to the limits.
fn raster_reader(path: &Path) -> Result<image::ImageReader<std::io::Cursor<Vec<u8>>>, String> {
    let bytes = read_file(path, MAX_RASTER_BYTES)?;
    // By what the file is, not what it is called; the formats the daemon
    // is built to decode, and no others.
    let format = match image::guess_format(&bytes) {
        Ok(
            format @ (image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Gif),
        ) => format,
        _ => return Err("it is not a PNG, JPEG or GIF picture".into()),
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    reader.limits(raster_limits());
    Ok(reader)
}

/// What decoding a picture may take. The image crate checks the sides
/// against the header, then reserves the whole decoded picture, width x
/// height x bytes per pixel, against `max_alloc` before it decodes: a
/// 4096x4096 picture with alpha is exactly [`MAX_RASTER_ALLOC`], and fits.
fn raster_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_RASTER_SIDE);
    limits.max_image_height = Some(MAX_RASTER_SIDE);
    limits.max_alloc = Some(MAX_RASTER_ALLOC);
    limits
}

/// Refuse a picture [`load_raster`] would refuse before decoding it, from
/// its header alone.
fn check_raster(path: &Path) -> Result<(), String> {
    use image::ImageDecoder;
    let decoder = raster_reader(path)?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    raster_limits()
        .reserve(decoder.total_bytes())
        .map_err(|e| e.to_string())
}

/// Decode a PNG, JPEG or GIF and scale it to fit the box.
fn load_raster(path: &Path, width: u32, height: u32, origin: Origin) -> Result<RgbaImage, String> {
    let image = raster_reader(path)?.decode().map_err(|e| e.to_string())?;

    let fits = image.width() <= width && image.height() <= height;
    let fills = image.width() == width || image.height() == height;
    let keep = match origin {
        Origin::File => fits,
        Origin::Theme => fits && fills,
    };
    let image = if keep {
        image
    } else {
        image.resize(width, height, image::imageops::FilterType::Triangle)
    };
    Ok(image.to_rgba8())
}

/// All of the regular file at `path`, refused past `limit` bytes.
///
/// Looked at before it is opened, since opening some devices does something
/// even when nothing is then read; and checked again once open, since the
/// path can be swapped for a FIFO in between. The open does not block, which
/// a FIFO with no writer otherwise would for ever, and the read is capped in
/// case the file grows.
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let too_big = || format!("it is larger than {} KB", limit / 1024);
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("it is not a regular file".into());
    }
    if metadata.len() > limit {
        return Err(too_big());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|e| e.to_string())?;
    if !file.metadata().is_ok_and(|opened| opened.is_file()) {
        return Err("it is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(too_big());
    }
    Ok(bytes)
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// What one drawn icon is kept under. The file's modification time and
/// length are part of it, so an icon edited on disk is drawn again rather
/// than served stale.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    width: u32,
    height: u32,
    tint: Option<[u8; 3]>,
    origin: Origin,
}

#[derive(Debug)]
struct Cached {
    /// Why an icon could not be drawn, kept so that it is not tried and
    /// warned about on every paint, and can be said in the layout.
    image: Result<Arc<RgbaImage>, String>,
    used: u64,
}

/// Icons already drawn, the most recently used kept.
///
/// Filled when a page is applied, so an animation's frames and a key's
/// repaints reuse one drawing instead of decoding the file again each time.
#[derive(Debug)]
pub struct IconCache {
    entries: HashMap<CacheKey, Cached>,
    capacity: usize,
    /// Counts uses, to tell which entry was used longest ago.
    clock: u64,
}

impl Default for IconCache {
    fn default() -> Self {
        IconCache::new(CACHE_CAPACITY)
    }
}

impl IconCache {
    pub fn new(capacity: usize) -> Self {
        IconCache {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            clock: 0,
        }
    }

    /// The icon at `path` drawn for a `width` x `height` box, from the cache
    /// if it has been drawn before.
    ///
    /// Tinted with `label_color` when the icon is symbolic, since it is
    /// drawn to sit with the label; any other icon keeps its own colours.
    /// `None` when it cannot be drawn, which is logged the first time.
    pub fn get(
        &mut self,
        path: &Path,
        origin: Origin,
        size: (u32, u32),
        label_color: Rgb,
    ) -> Option<Arc<RgbaImage>> {
        self.draw(path, origin, size, label_color).ok()
    }

    /// [`IconCache::get`], saying why an icon cannot be drawn.
    pub fn draw(
        &mut self,
        path: &Path,
        origin: Origin,
        (width, height): (u32, u32),
        label_color: Rgb,
    ) -> Result<Arc<RgbaImage>, String> {
        let tint = is_symbolic(path).then_some(label_color);
        let metadata = std::fs::metadata(path).ok();
        let key = CacheKey {
            path: path.to_path_buf(),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            len: metadata.as_ref().map_or(0, |m| m.len()),
            width,
            height,
            tint: tint.map(|color| [color.r, color.g, color.b]),
            origin,
        };
        self.clock += 1;
        if let Some(cached) = self.entries.get_mut(&key) {
            cached.used = self.clock;
            return cached.image.clone();
        }

        let image = load(path, (width, height), origin, tint)
            .inspect_err(|e| log::warn!("icon {}: {e}", path.display()))
            .map(Arc::new);
        if self.entries.len() >= self.capacity {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            Cached {
                image: image.clone(),
                used: self.clock,
            },
        );
        image
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A directory of its own for one test, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("galdeck-icons-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        /// A search root inside the scratch directory.
        fn root(&self, name: &str) -> PathBuf {
            let root = self.0.join(name);
            fs::create_dir_all(&root).unwrap();
            root
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SCALABLE: (&str, &str) = (
        "scalable/status",
        "Size=16\nMinSize=8\nMaxSize=512\nType=Scalable",
    );
    const SMALL: (&str, &str) = ("48x48/status", "Size=48\nType=Fixed");
    const LARGE: (&str, &str) = ("128x128/status", "Size=128\nType=Fixed");
    const HUGE: (&str, &str) = ("256x256/status", "Size=256\nType=Fixed");

    /// A symbolic-looking SVG: one colour, left half opaque, right half at
    /// half opacity.
    const SYMBOLIC_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
        <rect x="0" y="0" width="8" height="16" fill="#2e3436"/>
        <rect x="8" y="0" width="8" height="16" fill="#2e3436" opacity=".5"/>
    </svg>"##;
    const RED_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
        <rect width="16" height="16" fill="#ff0000"/>
    </svg>"##;

    /// Install theme `name` under `root`, inheriting from `parents`, with
    /// `dirs` as (directory, the keys of its section).
    fn install(root: &Path, name: &str, parents: &str, dirs: &[(&str, &str)]) {
        let home = root.join(name);
        fs::create_dir_all(&home).unwrap();
        let listed: Vec<&str> = dirs.iter().map(|(dir, _)| *dir).collect();
        let mut index = format!(
            "[Icon Theme]\nName={name}\nInherits={parents}\nDirectories={}\n",
            listed.join(",")
        );
        for (dir, keys) in dirs {
            index += &format!("\n[{dir}]\n{keys}\n");
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::write(home.join("index.theme"), index).unwrap();
    }

    /// Put an icon file in a theme's directory: an SVG or a square PNG of
    /// `pixels`, by the file's extension.
    fn put(root: &Path, theme: &str, dir: &str, file: &str, pixels: u32) -> PathBuf {
        let dir = root.join(theme).join(dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        if file.ends_with(".png") {
            RgbaImage::from_pixel(pixels, pixels, image::Rgba([0, 0, 255, 255]))
                .save(&path)
                .unwrap();
        } else {
            fs::write(&path, SYMBOLIC_SVG).unwrap();
        }
        path
    }

    fn names(themes: &IconThemes) -> Vec<&str> {
        themes.theme_names()
    }

    #[test]
    fn the_chosen_theme_comes_first_then_what_it_inherits_then_adwaita_then_hicolor() {
        let scratch = Scratch::new("chain");
        let root = scratch.root("icons");
        // hicolor is named early, as Yaru-dark names it, and still comes
        // last; Dark, named after it, is not pushed behind it.
        install(&root, "Mine", "Middle,hicolor,Dark", &[SCALABLE]);
        install(&root, "Middle", "Base", &[SCALABLE]);
        install(&root, "Base", "hicolor", &[SCALABLE]);
        install(&root, "Dark", "", &[SCALABLE]);
        install(&root, "Adwaita", "AdwaitaLegacy,hicolor", &[SCALABLE]);
        install(&root, "hicolor", "", &[SCALABLE]);

        let themes = IconThemes::load(vec![root], Some("Mine"));
        assert_eq!(
            names(&themes),
            ["Mine", "Middle", "Base", "Dark", "Adwaita", "hicolor"]
        );
    }

    #[test]
    fn a_theme_inheriting_from_itself_through_another_ends() {
        let scratch = Scratch::new("cycle");
        let root = scratch.root("icons");
        install(&root, "One", "Two", &[SCALABLE]);
        install(&root, "Two", "One,Two", &[SCALABLE]);
        install(&root, "hicolor", "", &[SCALABLE]);

        let themes = IconThemes::load(vec![root], Some("One"));
        assert_eq!(names(&themes), ["One", "Two", "hicolor"]);
    }

    #[test]
    fn themes_that_are_not_installed_are_skipped() {
        let scratch = Scratch::new("missing");
        let root = scratch.root("icons");
        // Humanity is not there at all; Empty is a directory with no
        // index.theme, which the spec says is not a theme. Adwaita is not
        // installed either.
        install(&root, "Mine", "Humanity,Empty,Dark", &[SCALABLE]);
        install(&root, "Dark", "", &[SCALABLE]);
        fs::create_dir_all(root.join("Empty/scalable/status")).unwrap();
        install(&root, "hicolor", "", &[SCALABLE]);

        let themes = IconThemes::load(vec![root.clone()], Some("Mine"));
        assert_eq!(names(&themes), ["Mine", "Dark", "hicolor"]);

        // Choosing a theme that is not installed leaves the fallbacks.
        let themes = IconThemes::load(vec![root], Some("Humanity"));
        assert_eq!(names(&themes), ["hicolor"]);
    }

    #[test]
    fn inheritance_is_followed_eight_deep() {
        let scratch = Scratch::new("deep");
        let root = scratch.root("icons");
        for level in 0..12 {
            install(
                &root,
                &format!("T{level}"),
                &format!("T{}", level + 1),
                &[SCALABLE],
            );
        }
        let themes = IconThemes::load(vec![root], Some("T0"));
        assert_eq!(
            names(&themes),
            ["T0", "T1", "T2", "T3", "T4", "T5", "T6", "T7", "T8"]
        );
    }

    #[test]
    fn a_theme_inheriting_from_dozens_still_leaves_adwaita_and_hicolor() {
        let scratch = Scratch::new("wide");
        let root = scratch.root("icons");
        // 1 + 16 + 32 themes, more than are ever searched.
        let parents: Vec<String> = (0..16).map(|n| format!("P{n}")).collect();
        install(&root, "Mine", &parents.join(","), &[SCALABLE]);
        for parent in &parents {
            install(&root, parent, &format!("{parent}a,{parent}b"), &[SCALABLE]);
            install(&root, &format!("{parent}a"), "", &[SCALABLE]);
            install(&root, &format!("{parent}b"), "", &[SCALABLE]);
        }
        install(&root, "Adwaita", "", &[SCALABLE]);
        install(&root, "hicolor", "", &[SCALABLE]);

        let themes = IconThemes::load(vec![root], Some("Mine"));
        let names = names(&themes);
        assert_eq!(names.len(), MAX_THEMES + 2);
        assert_eq!(names[..2], ["Mine", "P0"]);
        assert_eq!(names[MAX_THEMES..], ["Adwaita", "hicolor"]);
    }

    #[test]
    fn a_theme_name_must_be_one_path_component() {
        assert!(valid_theme_name("Yaru-prussiangreen-dark"));
        assert!(valid_theme_name("Adwaita"));
        for bad in [
            "",
            ".",
            "..",
            "../Adwaita",
            "a/b",
            "/usr/share/icons/Adwaita",
            "a\0b",
        ] {
            assert!(!valid_theme_name(bad), "{bad:?}");
        }

        let scratch = Scratch::new("theme-name");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[SCALABLE]);
        let themes = IconThemes::load(vec![root], Some("../Adwaita"));
        assert_eq!(names(&themes), ["Adwaita"]);
    }

    #[test]
    fn a_leading_tilde_is_the_daemon_s_own_home_and_nobody_else_s() {
        let home = Some(Path::new("/home/me"));
        let expand = |path: &str| expand_home_in(Path::new(path), home);
        assert_eq!(
            expand("~/Pictures/mute.png"),
            Path::new("/home/me/Pictures/mute.png")
        );
        assert_eq!(expand("~"), Path::new("/home/me"));
        assert_eq!(expand("~alice/mute.png"), Path::new("~alice/mute.png"));
        assert_eq!(
            expand("/usr/share/a~b.png"),
            Path::new("/usr/share/a~b.png")
        );
        assert_eq!(expand("pictures/~/x.png"), Path::new("pictures/~/x.png"));
        // No home, or one that is not a place, and it is left as written.
        assert_eq!(
            expand_home_in(Path::new("~/x.png"), None),
            Path::new("~/x.png")
        );
        assert_eq!(
            expand_home_in(Path::new("~/x.png"), Some(Path::new("relative"))),
            Path::new("~/x.png")
        );
    }

    #[test]
    fn a_name_is_found_in_the_first_theme_that_has_it() {
        let scratch = Scratch::new("first");
        let root = scratch.root("icons");
        install(&root, "Mine", "", &[SCALABLE]);
        install(&root, "Adwaita", "", &[SCALABLE]);
        install(&root, "hicolor", "", &[SCALABLE]);
        let mine = put(&root, "Mine", SCALABLE.0, "wifi-symbolic.svg", 0);
        put(&root, "Adwaita", SCALABLE.0, "wifi-symbolic.svg", 0);
        let adwaita_only = put(&root, "Adwaita", SCALABLE.0, "bt-symbolic.svg", 0);
        let hicolor_only = put(&root, "hicolor", SCALABLE.0, "app.svg", 0);

        let themes = IconThemes::load(vec![root], Some("Mine"));
        assert_eq!(themes.resolve("wifi-symbolic", (152, 116)), Some(mine));
        assert_eq!(
            themes.resolve("bt-symbolic", (152, 116)),
            Some(adwaita_only)
        );
        assert_eq!(themes.resolve("app", (152, 116)), Some(hicolor_only));
        assert_eq!(themes.resolve("nowhere", (152, 116)), None);
    }

    #[test]
    fn a_name_is_retried_with_symbolic_added_or_taken_away() {
        let scratch = Scratch::new("retry");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[SCALABLE]);
        let symbolic = put(
            &root,
            "Adwaita",
            SCALABLE.0,
            "network-wireless-symbolic.svg",
            0,
        );
        let plain = put(&root, "Adwaita", SCALABLE.0, "firefox.svg", 0);

        let themes = IconThemes::load(vec![root], None);
        assert_eq!(
            themes.resolve("network-wireless", (116, 116)),
            Some(symbolic)
        );
        assert_eq!(themes.resolve("firefox-symbolic", (116, 116)), Some(plain));
        // "-symbolic" alone has nothing to take the suffix from.
        assert_eq!(themes.resolve("-symbolic", (116, 116)), None);
    }

    /// An SVG no icon may be: it declares a DOCTYPE, as Yaru's snap icons
    /// do.
    const DOCTYPE_SVG: &str = r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect width="16" height="16"/></svg>"#;

    #[test]
    fn a_file_that_cannot_be_drawn_is_passed_over_for_the_next_one_found() {
        let scratch = Scratch::new("passed-over");
        let root = scratch.root("icons");
        // Listed first, as Yaru lists its icons drawn with filters.
        let generic = ("scalable/generic", SCALABLE.1);
        install(&root, "Mine", "", &[generic, SCALABLE, LARGE, HUGE]);
        install(&root, "Adwaita", "", &[SCALABLE]);
        let refuse = |theme: &str, dir: &str, file: &str| {
            let path = put(&root, theme, dir, file, 0);
            fs::write(&path, DOCTYPE_SVG).unwrap();
            path
        };

        // Another directory of the same theme.
        refuse("Mine", generic.0, "resources-symbolic.svg");
        let status = put(&root, "Mine", SCALABLE.0, "resources-symbolic.svg", 0);
        // The next theme.
        refuse("Mine", generic.0, "snap-symbolic.svg");
        let adwaita = put(&root, "Adwaita", SCALABLE.0, "snap-symbolic.svg", 0);
        // The next size: a PNG whose header claims more than may be decoded.
        let claims = put(&root, "Mine", LARGE.0, "bt.png", 128);
        fs::write(&claims, png_claiming(20_000, 20_000, 8)).unwrap();
        let larger = put(&root, "Mine", HUGE.0, "bt.png", 256);

        let themes = IconThemes::load(vec![root.clone()], Some("Mine"));
        assert_eq!(
            themes.resolve("resources-symbolic", (116, 116)),
            Some(status)
        );
        assert_eq!(themes.resolve("snap-symbolic", (116, 116)), Some(adwaita));
        assert_eq!(themes.resolve("bt", (116, 116)), Some(larger));

        // Refused wherever it is found: said why, and not taken for the
        // other name, which is another icon.
        let only = refuse("Mine", generic.0, "only-symbolic.svg");
        // Only the first few found are tried, the rest passed over.
        for dir in [generic.0, SCALABLE.0] {
            refuse("Mine", dir, "many.svg");
        }
        for (dir, pixels) in [(LARGE.0, 128), (HUGE.0, 256)] {
            let claims = put(&root, "Mine", dir, "many.png", pixels);
            fs::write(&claims, png_claiming(20_000, 20_000, 8)).unwrap();
        }
        put(&root, "Adwaita", SCALABLE.0, "many.svg", 0);
        put(&root, "Adwaita", SCALABLE.0, "only.svg", 0);
        let themes = IconThemes::load(vec![root], Some("Mine"));
        match themes.lookup("only-symbolic", (116, 116)) {
            Err(Missing::Refused { path, reason }) => {
                assert_eq!(path, only);
                assert!(reason.contains("DOCTYPE"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(themes.resolve("only-symbolic", (116, 116)), None);
        assert!(matches!(
            themes.lookup("many", (116, 116)),
            Err(Missing::Refused { .. })
        ));
        assert_eq!(themes.lookup("nowhere", (116, 116)), Err(Missing::NotFound));
    }

    #[test]
    fn a_scalable_svg_is_preferred_to_any_png() {
        let scratch = Scratch::new("scalable");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[LARGE, SCALABLE]);
        put(&root, "Adwaita", LARGE.0, "wifi.png", 128);
        let svg = put(&root, "Adwaita", SCALABLE.0, "wifi.svg", 0);

        let themes = IconThemes::load(vec![root], None);
        // Listed after the PNG's directory, and still chosen.
        assert_eq!(themes.resolve("wifi", (152, 116)), Some(svg));
    }

    #[test]
    fn the_png_chosen_is_the_smallest_at_least_the_box_else_the_largest() {
        let scratch = Scratch::new("sizes");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[SMALL, HUGE, LARGE]);
        let small = put(&root, "Adwaita", SMALL.0, "bt.png", 48);
        let large = put(&root, "Adwaita", LARGE.0, "bt.png", 128);
        let huge = put(&root, "Adwaita", HUGE.0, "bt.png", 256);

        let themes = IconThemes::load(vec![root.clone()], None);
        // The box's smaller side is what counts: 116 here.
        assert_eq!(themes.resolve("bt", (152, 116)), Some(large));
        assert_eq!(themes.resolve("bt", (300, 300)), Some(huge.clone()));
        assert_eq!(themes.resolve("bt", (40, 40)), Some(small));
        assert_eq!(themes.resolve("bt", (600, 600)), Some(huge));

        // A doubled directory counts its scale.
        let scratch = Scratch::new("scaled");
        let root = scratch.root("icons");
        install(
            &root,
            "Adwaita",
            "",
            &[LARGE, ("48x48@2/status", "Size=48\nScale=2\nType=Fixed")],
        );
        put(&root, "Adwaita", LARGE.0, "bt.png", 128);
        let doubled = put(&root, "Adwaita", "48x48@2/status", "bt.png", 96);
        let themes = IconThemes::load(vec![root], None);
        assert_eq!(themes.resolve("bt", (90, 90)), Some(doubled));
    }

    #[test]
    fn a_theme_spread_over_two_roots_is_searched_in_both() {
        let scratch = Scratch::new("spread");
        let user = scratch.root("user");
        let system = scratch.root("system");
        install(&system, "hicolor", "", &[SCALABLE]);
        // An app's own icon, in the user's hicolor, which has no index.theme.
        let app = put(&user, "hicolor", SCALABLE.0, "someapp.svg", 0);
        let other = put(&system, "hicolor", SCALABLE.0, "other.svg", 0);

        let themes = IconThemes::load(vec![user, system], None);
        assert_eq!(themes.resolve("someapp", (116, 116)), Some(app));
        assert_eq!(themes.resolve("other", (116, 116)), Some(other));
    }

    #[test]
    fn only_directories_the_index_lists_are_searched() {
        let scratch = Scratch::new("listed");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[SCALABLE]);
        put(&root, "Adwaita", "unlisted", "hidden.svg", 0);

        let themes = IconThemes::load(vec![root], None);
        assert_eq!(themes.resolve("hidden", (116, 116)), None);
    }

    #[test]
    fn an_icon_in_no_theme_is_found_straight_in_a_root() {
        let scratch = Scratch::new("unthemed");
        let pixmaps = scratch.root("pixmaps");
        let path = pixmaps.join("oldapp.png");
        RgbaImage::new(4, 4).save(&path).unwrap();

        let themes = IconThemes::load(vec![pixmaps], None);
        assert_eq!(themes.resolve("oldapp", (116, 116)), Some(path));
    }

    #[test]
    fn a_lookup_is_remembered_until_the_next_reload() {
        let scratch = Scratch::new("remembered");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[SCALABLE]);
        let path = put(&root, "Adwaita", SCALABLE.0, "gone.svg", 0);

        let themes = IconThemes::load(vec![root.clone()], None);
        assert_eq!(themes.resolve("gone", (116, 116)), Some(path.clone()));
        fs::remove_file(&path).unwrap();
        // No second walk: the answer from before.
        assert_eq!(themes.resolve("gone", (116, 116)), Some(path));
        // A reload looks again.
        let reloaded = IconThemes::load(vec![root], None);
        assert_eq!(reloaded.resolve("gone", (116, 116)), None);
    }

    #[test]
    fn what_is_remembered_about_names_stays_bounded() {
        let scratch = Scratch::new("bounded");
        let themes = IconThemes::load(vec![scratch.root("icons")], None);
        for n in 0..MAX_REMEMBERED + 10 {
            assert_eq!(themes.resolve(&format!("missing{n}"), (116, 116)), None);
        }
        let found = themes.lock();
        assert!(found.paths.len() <= MAX_REMEMBERED);
        assert!(found.warned.len() <= MAX_REMEMBERED);
    }

    #[test]
    fn names_and_directories_that_could_climb_out_are_not_used() {
        let scratch = Scratch::new("climb");
        let root = scratch.root("icons");
        install(&root, "Adwaita", "", &[SCALABLE]);
        put(&root, "Adwaita", "", "escape.svg", 0);

        let themes = IconThemes::load(vec![root.clone()], None);
        assert_eq!(themes.resolve("../escape", (116, 116)), None);
        assert_eq!(themes.resolve(".hidden", (116, 116)), None);
        assert_eq!(themes.resolve("", (116, 116)), None);

        let index = parse_index(
            "[Icon Theme]\nDirectories=../up,/abs,ok,a/../b\n\
             [../up]\nSize=16\n[/abs]\nSize=16\n[ok]\nSize=16\n[a/../b]\nSize=16\n",
        );
        let dirs: Vec<&str> = index.dirs.iter().map(|dir| dir.path.as_str()).collect();
        assert_eq!(dirs, ["ok"]);
    }

    #[test]
    fn an_index_theme_is_read_for_what_lookup_needs() {
        let index = parse_index(
            "# comment\n\
             [Icon Theme]\n\
             Name=Test\n\
             Name[de]=Prüfung\n\
             Inherits = Parent , ../bad,, Other\n\
             Directories=16x16/apps,scalable/apps,nosize,16x16/apps\n\
             ScaledDirectories=16x16@2/apps\n\
             \n\
             [16x16/apps]\n\
             Size=16\n\
             Type=Fixed\n\
             Size=99\n\
             [scalable/apps]\r\n\
             Size=16\r\n\
             MinSize=8\r\n\
             MaxSize=512\r\n\
             Type=Scalable\r\n\
             [nosize]\n\
             Type=Fixed\n\
             [16x16@2/apps]\n\
             Size=16\n\
             Scale=2\n",
        );
        assert_eq!(index.parents, ["Parent", "Other"]);
        assert_eq!(
            index.dirs,
            [
                Dir {
                    path: "16x16/apps".into(),
                    size: 16,
                    scale: 1,
                    kind: Kind::Fixed,
                    min_size: 16,
                    max_size: 16,
                },
                Dir {
                    path: "scalable/apps".into(),
                    size: 16,
                    scale: 1,
                    kind: Kind::Scalable,
                    min_size: 8,
                    max_size: 512,
                },
                Dir {
                    path: "16x16@2/apps".into(),
                    size: 16,
                    scale: 2,
                    kind: Kind::Threshold,
                    min_size: 16,
                    max_size: 16,
                },
            ]
        );
    }

    #[test]
    fn an_index_theme_lists_a_bounded_number_of_directories() {
        let listed: Vec<String> = (0..MAX_DIRECTORIES + 50).map(|n| format!("d{n}")).collect();
        let mut text = format!("[Icon Theme]\nDirectories={}\n", listed.join(","));
        for dir in &listed {
            text += &format!("[{dir}]\nSize=16\n");
        }
        assert_eq!(parse_index(&text).dirs.len(), MAX_DIRECTORIES);
    }

    #[test]
    fn roots_are_searched_in_the_spec_order_and_relative_ones_skipped() {
        let home = Path::new("/home/someone");
        assert_eq!(
            roots(Some(home), None, None),
            [
                "/home/someone/.icons",
                "/home/someone/.local/share/icons",
                "/usr/local/share/icons",
                "/usr/share/icons",
                "/usr/share/pixmaps",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            roots(
                Some(home),
                Some(OsStr::new("/data/home")),
                Some(OsStr::new(
                    "/usr/share/ubuntu::relative:/usr/share/:/usr/share"
                ))
            ),
            [
                "/home/someone/.icons",
                "/data/home/icons",
                "/usr/share/ubuntu/icons",
                "/usr/share/icons",
                "/usr/share/pixmaps",
            ]
            .map(PathBuf::from)
        );
        // A relative data home is ignored in favour of the default; no home
        // at all leaves the system directories.
        assert_eq!(
            roots(
                Some(home),
                Some(OsStr::new("snap/share")),
                Some(OsStr::new(""))
            )[1],
            PathBuf::from("/home/someone/.local/share/icons")
        );
        assert_eq!(
            roots(
                Some(Path::new("relative")),
                None,
                Some(OsStr::new("/opt/share"))
            ),
            ["/opt/share/icons", "/usr/share/pixmaps"].map(PathBuf::from)
        );
    }

    #[test]
    fn a_bus_that_never_answers_is_given_up_on_in_time() {
        let scratch = Scratch::new("silent-bus");
        let socket = scratch.0.join("bus");
        // Listening, so connecting succeeds, but nothing is ever said back.
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let address = format!("unix:path={}", socket.display());

        let started = Instant::now();
        let asked = desktop_theme_on(Some(address.clone()), Duration::from_millis(200));
        assert_eq!(asked, None);
        // Waited for an answer, rather than failing to connect at once.
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2));
        // The first ask is still waiting, so another does not wait beside it.
        let started = Instant::now();
        assert_eq!(
            desktop_theme_on(Some(address), Duration::from_secs(5)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(listener);
    }

    #[test]
    fn symbolic_names_are_listed_sorted_once_each_and_capped() {
        let scratch = Scratch::new("listing");
        let root = scratch.root("icons");
        install(&root, "Mine", "", &[SCALABLE]);
        install(&root, "Adwaita", "", &[SCALABLE, LARGE]);
        install(&root, "hicolor", "", &[SCALABLE]);
        put(&root, "Mine", SCALABLE.0, "wifi-symbolic.svg", 0);
        put(&root, "Adwaita", SCALABLE.0, "wifi-symbolic.svg", 0);
        put(
            &root,
            "Adwaita",
            SCALABLE.0,
            "airplane-mode-symbolic.svg",
            0,
        );
        put(&root, "Adwaita", SCALABLE.0, "full-colour.svg", 0);
        put(&root, "Adwaita", LARGE.0, "bt-symbolic.png", 4);
        put(
            &root,
            "hicolor",
            SCALABLE.0,
            "org.gnome.Settings-symbolic.svg",
            0,
        );
        put(&root, "Adwaita", "unlisted", "hidden-symbolic.svg", 0);
        fs::write(
            root.join("Adwaita")
                .join(SCALABLE.0)
                .join("notes-symbolic.txt"),
            "",
        )
        .unwrap();

        let themes = IconThemes::load(vec![root], Some("Mine"));
        assert_eq!(
            themes.symbolic_names(),
            [
                "airplane-mode-symbolic",
                "bt-symbolic",
                "org.gnome.Settings-symbolic",
                "wifi-symbolic",
            ]
        );
        assert_eq!(
            themes.symbolic_names_up_to(2),
            ["airplane-mode-symbolic", "bt-symbolic"]
        );
    }

    /// Write `contents` to `name` in the scratch directory.
    fn file(scratch: &Scratch, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = scratch.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    /// Run `load` on a thread and give it a few seconds, so a regression
    /// that blocks fails the test instead of hanging the run.
    fn load_within(path: PathBuf, size: (u32, u32)) -> Result<RgbaImage, String> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(load(&path, size, Origin::File, None)));
        rx.recv_timeout(Duration::from_secs(10))
            .expect("loading the icon did not finish")
    }

    fn opaque_pixels(image: &RgbaImage) -> usize {
        image.pixels().filter(|pixel| pixel.0[3] > 0).count()
    }

    #[test]
    fn an_svg_image_naming_a_file_is_not_followed() {
        let scratch = Scratch::new("href");
        // An endless file: the default resolver reads it whole.
        let zero = file(
            &scratch,
            "zero.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                <image href="/dev/zero" width="16" height="16"/>
            </svg>"#,
        );
        let drawn = load_within(zero, (64, 64)).unwrap();
        assert_eq!(opaque_pixels(&drawn), 0);

        // A readable SVG it points at is not pulled in either, by either
        // spelling of the attribute.
        let red = file(&scratch, "red.svg", RED_SVG);
        let pointing = file(
            &scratch,
            "pointing.svg",
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 16 16">
                    <image href="{0}" width="16" height="16"/>
                    <image xlink:href="{0}" width="16" height="16"/>
                </svg>"#,
                red.display()
            ),
        );
        let drawn = load_within(pointing, (64, 64)).unwrap();
        assert_eq!(opaque_pixels(&drawn), 0);

        // Nor is an SVG embedded as data, which would dodge the checks.
        let embedded = file(
            &scratch,
            "embedded.svg",
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                    <image href="data:image/svg+xml;base64,{}" width="16" height="16"/>
                </svg>"#,
                crate::base64::encode(RED_SVG.as_bytes())
            ),
        );
        let drawn = load_within(embedded, (64, 64)).unwrap();
        assert_eq!(opaque_pixels(&drawn), 0);
    }

    #[test]
    fn an_svg_declaring_an_enormous_size_is_drawn_at_the_box() {
        let scratch = Scratch::new("enormous");
        let view_box = file(
            &scratch,
            "view-box.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100000 100000">
                <rect width="100000" height="100000" fill="#ff0000"/>
            </svg>"##,
        );
        let started = Instant::now();
        let drawn = load_within(view_box, (152, 116)).unwrap();
        assert_eq!(drawn.dimensions(), (116, 116));
        assert_eq!(opaque_pixels(&drawn), 116 * 116);

        let declared = file(
            &scratch,
            "declared.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="50000">
                <rect width="100000" height="50000" fill="#ff0000"/>
            </svg>"##,
        );
        let drawn = load_within(declared, (152, 116)).unwrap();
        assert_eq!(drawn.dimensions(), (152, 76));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_compressed_svg_is_refused() {
        let scratch = Scratch::new("gzip");
        let mut bytes = vec![0x1f, 0x8b, 0x08, 0x00];
        bytes.extend_from_slice(&[0; 32]);
        let error = load_within(file(&scratch, "bomb.svg", &bytes), (64, 64)).unwrap_err();
        assert!(error.contains("compressed"), "{error}");
        let error = load_within(file(&scratch, "bomb.svgz", &bytes), (64, 64)).unwrap_err();
        assert!(error.contains("compressed"), "{error}");
    }

    #[test]
    fn an_svg_with_a_doctype_or_entity_is_refused() {
        let scratch = Scratch::new("doctype");
        let laughs = file(
            &scratch,
            "laughs.svg",
            r#"<?xml version="1.0"?>
            <!DOCTYPE svg [
              <!ENTITY a "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">
              <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
            ]>
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><title>&b;</title></svg>"#,
        );
        let error = load_within(laughs, (64, 64)).unwrap_err();
        assert!(error.contains("DOCTYPE"), "{error}");

        let lower = file(
            &scratch,
            "lower.svg",
            r#"<!doctype svg><svg xmlns="http://www.w3.org/2000/svg"/>"#,
        );
        assert!(load_within(lower, (64, 64)).is_err());
    }

    /// An SVG with one filter of `blurs` blurs, applied to `uses` elements.
    fn blurred(blurs: usize, uses: usize) -> String {
        let steps = "<feGaussianBlur stdDeviation=\"1\"/>".repeat(blurs);
        let rects = r##"<rect width="16" height="16" fill="#000" filter="url(#f)"/>"##.repeat(uses);
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                <defs><filter id="f">{steps}</filter></defs>{rects}
            </svg>"#
        )
    }

    #[test]
    fn an_svg_running_too_many_filter_steps_is_refused() {
        let scratch = Scratch::new("filters");
        assert!(load_within(file(&scratch, "eight.svg", blurred(8, 1)), (64, 64)).is_ok());
        let error = load_within(file(&scratch, "nine.svg", blurred(9, 1)), (64, 64)).unwrap_err();
        assert!(error.contains("9 filter steps"), "{error}");
        // One blur applied nine times is nine blurs.
        let error = load_within(file(&scratch, "reused.svg", blurred(1, 9)), (64, 64)).unwrap_err();
        assert!(error.contains("9 filter steps"), "{error}");
        // Inside a mask counts too.
        let masked = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                <defs>
                  <filter id="f">{}</filter>
                  <mask id="m"><rect width="16" height="16" fill="#fff" filter="url(#f)"/></mask>
                </defs>
                <rect width="16" height="16" fill="#000" mask="url(#m)"/>
            </svg>"##,
            "<feGaussianBlur stdDeviation=\"1\"/>".repeat(9)
        );
        assert!(load_within(file(&scratch, "masked.svg", masked), (64, 64)).is_err());
    }

    /// An SVG of one filter with `primitive` in it, over a large region.
    fn filtered(primitive: &str) -> String {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                <filter id="f" x="-100" y="-100" width="200" height="200">{primitive}</filter>
                <rect width="16" height="16" fill="#000" filter="url(#f)"/>
            </svg>"##
        )
    }

    #[test]
    fn filter_steps_whose_work_the_file_chooses_count_for_that_work() {
        let scratch = Scratch::new("heavy-filters");
        let refused = |name: &str, primitive: &str| {
            let started = Instant::now();
            let error = load_within(file(&scratch, name, filtered(primitive)), (116, 116))
                .expect_err(primitive);
            // Refused, not drawn: any of these would take seconds.
            assert!(started.elapsed() < Duration::from_secs(2), "{primitive}");
            error
        };

        // A radius of 100 took more than 30 s to draw.
        let error = refused("morphology.svg", r#"<feMorphology radius="100"/>"#);
        assert!(error.contains("feMorphology"), "{error}");
        let error = refused("small.svg", r#"<feMorphology radius="1"/>"#);
        assert!(error.contains("feMorphology"), "{error}");

        // An octave is a pass of its own: a thousand took 19 s.
        let error = refused(
            "turbulence.svg",
            r#"<feTurbulence baseFrequency="0.5" numOctaves="1000"/>"#,
        );
        assert!(error.contains("1000 filter steps"), "{error}");
        let two = r#"<feTurbulence baseFrequency="0.5" numOctaves="2"/>"#;
        assert!(load_within(file(&scratch, "two.svg", filtered(two)), (116, 116)).is_ok());

        // A 3x3 matrix is a step; a 60x60 one took 4.4 s.
        let order = |n: usize| {
            format!(
                r#"<feConvolveMatrix order="{n}" kernelMatrix="{}"/>"#,
                vec!["1"; n * n].join(" ")
            )
        };
        let error = refused("convolve.svg", &order(60));
        assert!(error.contains("400 filter steps"), "{error}");
        let three = file(&scratch, "three.svg", filtered(&order(3)));
        assert!(load_within(three, (116, 116)).is_ok());
    }

    #[test]
    fn an_svg_drawing_too_many_layers_is_refused() {
        let scratch = Scratch::new("layers");
        let big = r##"<rect x="-9999" y="-9999" width="99999" height="99999" fill="#000"/>"##;
        let refused = |name: &str, svg: String| {
            let started = Instant::now();
            let error = load_within(file(&scratch, name, svg), (116, 116)).unwrap_err();
            assert!(error.contains("layers"), "{error}");
            // A thousand masks over a large rectangle took 4.7 s to draw.
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        };
        let svg = |defs: &str, body: String| {
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                    <defs><mask id="m">{big}</mask><clipPath id="c">{big}</clipPath>{defs}</defs>
                    {body}
                </svg>"##
            )
        };
        let each = |open: &str, count: usize| format!("{open}{big}</g>").repeat(count);

        // A group with a mask is a layer, and so is drawing the mask.
        refused("masks.svg", svg("", each(r#"<g mask="url(#m)">"#, 300)));
        refused(
            "clips.svg",
            svg("", each(r#"<g clip-path="url(#c)">"#, 300)),
        );
        refused(
            "faded.svg",
            svg("", each(r#"<g opacity=".9">"#, MAX_LAYERS + 1)),
        );
        let nested = format!(
            "{}{big}{}",
            r#"<g opacity=".9">"#.repeat(20),
            "</g>".repeat(20)
        );
        let few = file(&scratch, "few.svg", svg("", nested));
        assert!(load_within(few, (116, 116)).is_ok());

        // Each clip path in a chain is drawn, though usvg lists only two.
        let chain: String = (0..40)
            .map(|n| {
                format!(
                    r#"<clipPath id="k{n}" clip-path="url(#k{})">{big}</clipPath>"#,
                    n + 1
                )
            })
            .collect();
        let chain = format!(r#"{chain}<clipPath id="k40">{big}</clipPath>"#);
        let clipped = r##"<rect width="16" height="16" clip-path="url(#k0)"/>"##.repeat(20);
        refused("chain.svg", svg(&chain, clipped));
    }

    /// An SVG whose pattern `p` is written `pattern`, and `body` after it.
    fn patterned(pattern: &str, body: &str) -> String {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                <defs>{pattern}</defs>{body}
            </svg>"##
        )
    }

    #[test]
    fn a_pattern_whose_tile_is_larger_than_a_layer_is_refused_before_it_is_drawn() {
        let scratch = Scratch::new("pattern-tile");
        let refused = |name: &str, svg: &str| {
            let started = Instant::now();
            let error = load_within(file(&scratch, name, svg), (152, 116)).unwrap_err();
            assert!(error.contains("pattern"), "{name}: {error}");
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        };

        // 257 bytes. resvg draws a pattern's tile into a pixmap of its own,
        // here 725000 px a side at the fit scale: the allocation of 2 TB
        // failed and aborted the daemon, which restarted into the same key.
        let huge = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
  <pattern id="p" patternUnits="userSpaceOnUse" width="100000" height="100000">
    <rect width="1" height="1" fill="#2e3436"/>
  </pattern>
  <rect width="16" height="16" fill="url(#p)"/>
</svg>
"##;
        assert_eq!(huge.len(), 257);
        refused("huge.svg", huge);

        let big = r#"<pattern id="p" patternUnits="userSpaceOnUse" width="16" height="16"><rect width="16" height="16"/></pattern>"#;
        let filled = r#"<rect width="16" height="16" fill="url(#p)"/>"#;
        // The tile's size is its rectangle as drawn: scaled by the pattern's
        // transform, by the groups the shape is in, and in the shape's own
        // units when the pattern is sized by it.
        let transformed = big.replace(
            "height=\"16\">",
            "height=\"16\" patternTransform=\"scale(150)\">",
        );
        refused("transformed.svg", &patterned(&transformed, filled));
        refused(
            "grouped.svg",
            &patterned(big, &format!(r#"<g transform="scale(100)">{filled}</g>"#)),
        );
        let by_shape =
            r#"<pattern id="p" width="1000" height="1000"><rect width="1" height="1"/></pattern>"#;
        refused("by-shape.svg", &patterned(by_shape, filled));
        // A pattern used inside another's tile is drawn at that tile's
        // scale, and one inside a mask at the masked shape's.
        let nested = format!(
            r#"<pattern id="p" patternUnits="userSpaceOnUse" width="16" height="16"><rect width="16" height="16" fill="url(#q)"/></pattern>{}"#,
            huge_pattern("q")
        );
        refused("nested.svg", &patterned(&nested, filled));
        let masked = format!(
            r#"{}<mask id="m"><rect width="16" height="16" fill="url(#q)"/></mask>"#,
            huge_pattern("q")
        );
        refused(
            "masked.svg",
            &patterned(
                &masked,
                r##"<rect width="16" height="16" fill="#000" mask="url(#m)"/>"##,
            ),
        );
    }

    /// A pattern `id` whose tile is 100000 units a side.
    fn huge_pattern(id: &str) -> String {
        format!(
            r#"<pattern id="{id}" patternUnits="userSpaceOnUse" width="100000" height="100000"><rect width="1" height="1"/></pattern>"#
        )
    }

    #[test]
    fn a_large_pattern_tile_is_drawn_and_counted_each_time_it_is_used() {
        let scratch = Scratch::new("pattern-uses");
        // 16 units scaled by four: 464 px a side at 116, sixteen boxes, and
        // within the 25 a layer can reach.
        let pattern = r##"<pattern id="p" patternUnits="userSpaceOnUse" width="16" height="16" patternTransform="scale(4)"><rect width="16" height="16" fill="#ff0000"/></pattern>"##;
        let uses = |count: usize| {
            patterned(
                pattern,
                &r#"<rect width="16" height="16" fill="url(#p)"/>"#.repeat(count),
            )
        };
        let drawn = load_within(file(&scratch, "one.svg", uses(1)), (116, 116)).unwrap();
        assert_eq!(drawn.dimensions(), (116, 116));
        assert_eq!(drawn.get_pixel(58, 58).0, [255, 0, 0, 255]);

        // The tile is drawn again for every shape it fills: 300 shapes
        // sharing a 2900 px tile took 2.7 s. 33 of these are 528 boxes.
        let started = Instant::now();
        let error = load_within(file(&scratch, "many.svg", uses(33)), (116, 116)).unwrap_err();
        assert!(error.contains("528 layers"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn dashes_long_paths_and_many_stop_gradients_are_counted_before_drawing() {
        let scratch = Scratch::new("path-work");
        let refused = |name: &str, body: &str, says: &str| {
            let svg = patterned("", body);
            let started = Instant::now();
            let error = load_within(file(&scratch, name, svg), (152, 116)).unwrap_err();
            assert!(error.contains(says), "{name}: {error}");
            // Refused, not drawn: each of these took seconds unchecked.
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        };

        // One line dashed 950,000 times took 374 ms to draw, and a hundred
        // of them, in 6 KB, took 36 s. That line reaches far past the icon;
        // one across it, dashed as finely, is as slow.
        let dashed = |length: usize| {
            format!(r##"<path d="M0 8 H{length}" stroke="#000" stroke-dasharray="1"/>"##)
        };
        refused("dashed.svg", &dashed(1_900_000), "reaching");
        refused(
            "fine.svg",
            r##"<path d="M0 8 H16" fill="none" stroke="#000" stroke-dasharray=".00001"/>"##,
            "segments",
        );
        // Short enough alone, too many together.
        refused(
            "dashes.svg",
            &dashed(400).repeat(1100),
            "200000 path segments",
        );
        // A path whose edges keep crossing takes time with the square of
        // their number: a zigzag of 40,000 segments took 2.9 s to stroke.
        let zigzag = |segments: usize| "l1 1 l-1 -1 ".repeat(segments / 2);
        let stroked = format!(
            r##"<path d="M8 8 {}" fill="none" stroke="#000"/>"##,
            zigzag(8000)
        );
        refused("zigzag.svg", &stroked, "8001 segments");
        // A path is drawn again wherever it is used.
        let copies = format!(
            r##"<defs><path id="z" d="M8 8 {}" fill="none" stroke="#000"/></defs>{}"##,
            zigzag(2000),
            r##"<use href="#z"/>"##.repeat(20)
        );
        refused("copies.svg", &copies, "segments");
        // Every pixel a gradient fills looks through every stop: 4,000
        // stops over a thousand rectangles took 9.3 s.
        let stops: String = (0..1000)
            .map(|n| format!(r#"<stop offset="{}"/>"#, n as f32 / 1000.0))
            .collect();
        let gradient = format!(
            r#"<linearGradient id="g">{stops}</linearGradient>{}"#,
            r#"<rect width="16" height="16" fill="url(#g)"/>"#.repeat(50)
        );
        refused("stops.svg", &gradient, "gradients");

        // Dashes and paths the way icons draw them.
        let ordinary = format!(
            r##"{}<path d="M1 1 {}" fill="none" stroke="#000" stroke-width=".1"/>"##,
            r##"<path d="M0 8 H16" stroke="#000" stroke-dasharray="1 .5"/>"##.repeat(20),
            "l.01 .01 l-.01 .005 ".repeat(1000)
        );
        let drawn = load_within(
            file(&scratch, "ordinary.svg", patterned("", &ordinary)),
            (152, 116),
        )
        .unwrap();
        assert!(opaque_pixels(&drawn) > 0);
    }

    /// Five curves whose stroke tiny-skia works out slowly when scaled up.
    const LOOPS: &str = "M8 8 C10.13 11.71 16.68 15.57 14.24 10.27 \
        C4.50 -1.35 12.07 12.56 -0.31 8.61 C-1.48 9.61 11.72 9.97 10.19 17.30 \
        C1.03 13.53 2.40 7.30 -0.66 15.98 C-1.73 1.92 11.54 8.09 -1.16 0.76 Z";

    #[test]
    fn a_path_drawn_far_past_the_icon_is_refused_before_it_is_drawn() {
        let scratch = Scratch::new("reach");
        let scaled = |scale: &str, path: &str| {
            patterned(
                "",
                &format!(r#"<g transform="scale({scale}) translate(-8 -8)">{path}</g>"#),
            )
        };
        let refused = |name: &str, svg: String| {
            let started = Instant::now();
            let error = load_within(file(&scratch, name, svg), (116, 116)).unwrap_err();
            assert!(error.contains("reaching"), "{name}: {error}");
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        };

        // Under 350 bytes each. tiny-skia works in f32: filled or stroked
        // 10^8 pixels across, a curve came out past the edge of the pixmap
        // and tiny-skia panicked in a release build, stopping the daemon.
        let wave = r##"<path d="M0 0 c5 10 10 -10 16 0" fill="none" stroke="#000"/>"##;
        refused("stroked.svg", scaled("1e8", wave));
        let filled = r##"<path d="M8 8 C11.14 -1.19 0.61 16.44 4.27 12.41 C-0.40 13.04 15.90 11.05 13.68 -1.48 C-0.67 10.28 11.85 0.19 0.63 15.71 Z"/>"##;
        refused("filled.svg", scaled("1.08e8", filled));
        // A stroke is worked out more finely the larger it is drawn: a
        // thousand of the wave's curves scaled 10^5 times took 27 s and ran
        // out of memory at 2.5 GB, and 2,000 of these scaled 2,000 times
        // took 1.2 s.
        let loops = |width: &str| {
            format!(
                r##"<path d="{}" fill="none" stroke="#000" stroke-width="{width}"/>"##,
                LOOPS.repeat(20)
            )
        };
        refused("thin.svg", scaled("2000", &loops(".00021")));
        refused("wave.svg", scaled("1e5", wave));

        // Drawn well within reach, and as far out as installed icons go: a
        // path a thousand units from where it is drawn from, moved back.
        let far = r##"<g transform="translate(-1000 -1000)"><path d="M1001 1001 c5 10 10 -10 14 14" fill="none" stroke="#000"/></g>"##;
        let background =
            r##"<rect x="-5000" y="-5000" width="10000" height="10000" fill="#fff"/>"##;
        for (name, svg) in [
            ("loops.svg", scaled("1", &loops(".5"))),
            ("far.svg", patterned("", far)),
            (
                "background.svg",
                patterned("", &format!("{background}{wave}")),
            ),
        ] {
            let drawn = load_within(file(&scratch, name, svg), (116, 116)).unwrap();
            assert!(opaque_pixels(&drawn) > 0, "{name}");
        }
    }

    #[test]
    fn edges_and_fills_are_counted_wherever_they_are_drawn() {
        let scratch = Scratch::new("edges");
        let load = |name: &str, svg: String| {
            let started = Instant::now();
            let loaded = load_within(file(&scratch, name, svg), (116, 116));
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
            loaded
        };

        // Curves across the icon, stroked: 990 copies of 200 of them, in
        // 23 KB, took 3.4 s. Every copy is drawn.
        let curves = "c5 10 10 -10 16 0 c-5 10 -10 -10 -16 0 ".repeat(100);
        let copies = |count: usize| {
            patterned(
                &format!(r##"<path id="w" d="M0 8 {curves}" fill="none" stroke="#000"/>"##),
                &r##"<use href="#w"/>"##.repeat(count),
            )
        };
        let error = load("curves.svg", copies(10)).unwrap_err();
        assert!(error.contains("pixels of edge"), "{error}");
        assert!(load("few.svg", copies(4)).is_ok());

        // Filled past the icon in a layer: 480 rectangles over a pattern's
        // tile drawn ten times took 800 ms.
        let layer = |count: usize| {
            let square = r##"<rect x="-32" y="-32" width="80" height="80" fill="#123"/>"##;
            patterned(
                "",
                &format!(r#"<g opacity=".5">{}</g>"#, square.repeat(count)),
            )
        };
        let error = load("filled.svg", layer(201)).unwrap_err();
        assert!(error.contains("times the icon's size"), "{error}");
        assert!(load("layer.svg", layer(20)).is_ok());
    }

    #[test]
    fn path_data_is_counted_wherever_it_is_used_before_it_is_parsed() {
        let scratch = Scratch::new("path-data");
        let refused = |name: &str, svg: String| {
            let started = Instant::now();
            let error = load_within(file(&scratch, name, svg), (116, 116)).unwrap_err();
            assert!(error.contains("bytes of path data"), "{name}: {error}");
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        };
        // usvg strokes every path it builds to find its bounds, copies and
        // all: a thousand copies of a 6,000-curve path took 12.5 s to parse.
        let curves = format!(
            r##"<path id="w" d="M0 8 {}" fill="none" stroke="#000"/>"##,
            "c5 10 10 -10 16 0 c-5 10 -10 -10 -16 0 ".repeat(100)
        );
        refused(
            "copies.svg",
            patterned(&curves, &r##"<use href="#w"/>"##.repeat(300)),
        );
        // Built again at every vertex a marker sits on.
        let marker = format!(r#"<marker id="m">{curves}</marker>"#);
        let vertices = format!(
            r##"<path d="M0 0 {}" stroke="#000" marker-mid="url(#m)"/>"##,
            "l.01 .01 ".repeat(300)
        );
        refused("markers.svg", patterned(&marker, &vertices));
    }

    #[test]
    fn an_svg_that_multiplies_what_it_refers_to_is_refused_before_it_is_built() {
        let scratch = Scratch::new("fan-out");
        let refused = |name: &str, svg: String| {
            assert!(svg.len() < 64 * 1024, "{name} is {} bytes", svg.len());
            let started = Instant::now();
            let error = load_within(file(&scratch, name, svg), (116, 116)).unwrap_err();
            assert!(error.contains("elements"), "{name}: {error}");
            // Counted on the XML: usvg never builds the copies.
            assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        };
        let svg = |body: String| {
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 16 16">{body}</svg>"##
            )
        };
        let rects = |n: usize| r##"<rect width=".1" height=".1" fill="#fff"/>"##.repeat(n);

        // Each level copies the one below ten times: 100,000 rectangles, and
        // 180 MB and 1.8 s, from 1 KB.
        let mut levels = r##"<g id="a0"><rect width="16" height="16"/></g>"##.to_string();
        for level in 1..=5 {
            let uses = format!(r##"<use xlink:href="#a{}"/>"##, level - 1).repeat(10);
            levels += &format!(r#"<g id="a{level}">{uses}</g>"#);
        }
        refused(
            "uses.svg",
            svg(format!(r##"<defs>{levels}</defs><use href="#a5"/>"##)),
        );

        // A mask is converted again for every element it is set on.
        let masked = r##"<rect width="16" height="16" mask="url(#m)"/>"##.repeat(150);
        refused(
            "masks.svg",
            svg(format!(
                r#"<defs><mask id="m">{}</mask></defs>{masked}"#,
                rects(150)
            )),
        );
        // So is one set from a stylesheet, which reaches every element.
        refused(
            "sheet.svg",
            svg(format!(
                r#"<style>rect {{ mask: url(#m) }}</style><defs><mask id="m">{}</mask></defs>{}"#,
                rects(150),
                r#"<rect width="16" height="16"/>"#.repeat(150)
            )),
        );
        // A marker is drawn at every vertex of a path.
        let path = format!("M0 0{}", " L1 1".repeat(300));
        refused(
            "markers.svg",
            svg(format!(
                r##"<defs><marker id="k">{}</marker></defs><path d="{path}" stroke="#000" marker-mid="url(#k)"/>"##,
                rects(60)
            )),
        );
        // A pattern set on a group fills every shape inside it, and one set
        // on a <use> every shape it copies.
        let pattern = format!(
            r#"<defs><pattern id="p" width="1" height="1" patternContentUnits="objectBoundingBox">{}</pattern><g id="shapes">{}</g></defs>"#,
            rects(60),
            r#"<rect width="16" height="16"/>"#.repeat(200)
        );
        refused(
            "group.svg",
            svg(format!(
                r##"{pattern}<g style="fill: url(#p)">{}</g>"##,
                r#"<rect width="16" height="16"/>"#.repeat(200)
            )),
        );
        refused(
            "copied.svg",
            svg(format!(
                r##"{pattern}<use href="#shapes" fill="url(#p)"/>"##
            )),
        );
        // Two elements with the same id: the larger is what counts.
        refused(
            "twins.svg",
            svg(format!(
                r#"<defs><mask id="m"/><mask id="m">{}</mask></defs>{masked}"#,
                rects(150)
            )),
        );
    }

    #[test]
    fn an_svg_using_gradients_and_masks_as_icons_do_is_drawn() {
        let scratch = Scratch::new("ordinary");
        // Gradients are left out of the count however often they are used,
        // as a stylesheet exported from a drawing program uses them.
        let gradients: String = (0..50)
            .map(|n| format!(r##"<linearGradient id="g{n}"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#00f"/></linearGradient>"##))
            .collect();
        let classes: String = (0..50)
            .map(|n| format!(".c{n} {{ fill: url(#g{n}) }}"))
            .collect();
        let shapes: String = (0..400)
            .map(|n| format!(r#"<rect class="c{}" width="16" height="16"/>"#, n % 50))
            .collect();
        let masked = r##"<rect width="16" height="16" fill="#000" mask="url(#m)"/>"##.repeat(20);
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
                <style>{classes}</style>
                <defs>{gradients}<mask id="m"><rect width="8" height="16" fill="#fff"/></mask></defs>
                {shapes}{masked}
            </svg>"##
        );
        let drawn = load_within(file(&scratch, "ordinary.svg", svg), (116, 116)).unwrap();
        assert!(opaque_pixels(&drawn) > 0);
    }

    #[test]
    fn an_svg_nested_deeper_than_any_icon_is_refused() {
        let scratch = Scratch::new("nested");
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">{}<rect width="16" height="16"/>{}</svg>"#,
            "<g>".repeat(MAX_SVG_DEPTH + 10),
            "</g>".repeat(MAX_SVG_DEPTH + 10)
        );
        let error = load_within(file(&scratch, "deep.svg", svg), (116, 116)).unwrap_err();
        assert!(error.contains("nests deeper"), "{error}");
    }

    #[test]
    fn an_svg_too_small_to_fit_is_refused_rather_than_drawn_at_infinity() {
        let scratch = Scratch::new("tiny");
        let tiny = file(
            &scratch,
            "tiny.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="1e-38" height="1e-38">
                <rect width="1" height="1" fill="#000"/>
            </svg>"##,
        );
        let error = load_within(tiny, (152, 116)).unwrap_err();
        assert!(error.contains("too small"), "{error}");
    }

    #[test]
    fn an_icon_file_past_its_size_cap_is_refused() {
        let scratch = Scratch::new("cap");
        let path = scratch.0.join("large.svg");
        // Sparse, so this costs no disk.
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SVG_BYTES + 1)
            .unwrap();
        let error = load_within(path, (64, 64)).unwrap_err();
        assert!(error.contains("larger than"), "{error}");

        let path = scratch.0.join("large.png");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_RASTER_BYTES + 1)
            .unwrap();
        assert!(load_within(path, (64, 64)).is_err());
    }

    #[test]
    fn an_icon_that_is_not_a_regular_file_is_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let scratch = Scratch::new("fifo");
        let fifo = scratch.0.join("icon.svg");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid C string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(load_within(fifo, (64, 64)).is_err());
        assert!(load_within(PathBuf::from("/dev/zero"), (64, 64)).is_err());
        assert!(load_within(scratch.0.clone(), (64, 64)).is_err());
    }

    /// The standard CRC-32 PNG chunks carry.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A one-pixel RGBA PNG whose header claims `width` x `height` at
    /// `depth` bits a channel.
    fn png_claiming(width: u32, height: u32, depth: u8) -> Vec<u8> {
        let mut png = Vec::new();
        RgbaImage::new(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        // The header chunk: its type and data from byte 12 to 29, its CRC
        // after. Checked first, so a wrong CRC cannot be what refuses it.
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(png[29..33], crc32(&png[12..29]).to_be_bytes());
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        png[24] = depth;
        let crc = crc32(&png[12..29]).to_be_bytes();
        png[29..33].copy_from_slice(&crc);
        png
    }

    #[test]
    fn a_png_claiming_20000_pixels_square_is_refused_before_decoding() {
        let png = png_claiming(20_000, 20_000, 8);
        let scratch = Scratch::new("claims");
        let started = Instant::now();
        let error = load_within(file(&scratch, "huge.png", &png), (116, 116)).unwrap_err();
        assert!(error.to_lowercase().contains("limit"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_picture_up_to_4096_pixels_a_side_is_decoded_and_shrunk_to_fit() {
        use image::ImageEncoder;
        let scratch = Scratch::new("large-pictures");
        // What the editor's Upload button and a pasted link store: an
        // app's 1200 px logo, a photo.
        let logo = scratch.0.join("logo.png");
        RgbaImage::from_pixel(1200, 1200, image::Rgba([200, 0, 0, 255]))
            .save(&logo)
            .unwrap();
        let drawn = load_within(logo, (152, 116)).unwrap();
        assert_eq!(drawn.dimensions(), (116, 116));
        assert_eq!(drawn.get_pixel(58, 58).0, [200, 0, 0, 255]);
        let photo = scratch.0.join("photo.jpg");
        image::RgbImage::from_pixel(1800, 1200, image::Rgb([0, 0, 200]))
            .save(&photo)
            .unwrap();
        assert_eq!(
            load_within(photo, (152, 116)).unwrap().dimensions(),
            (152, 101)
        );

        // The largest a side may be, with alpha: exactly the memory one
        // decode may take, which the image crate reserves before decoding.
        let largest = scratch.0.join("largest.png");
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Sub,
        )
        .write_image(
            RgbaImage::from_pixel(4096, 4096, image::Rgba([0, 200, 0, 255])).as_raw(),
            4096,
            4096,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        fs::write(&largest, png).unwrap();
        check_raster(&largest).unwrap();
        let drawn = load_within(largest, (116, 116)).unwrap();
        assert_eq!(drawn.get_pixel(58, 58).0, [0, 200, 0, 255]);

        // A pixel more a side, or more memory at that size, is refused from
        // the header.
        for (name, png) in [
            ("wide.png", png_claiming(4097, 16, 8)),
            ("deep.png", png_claiming(4096, 4096, 16)),
        ] {
            let path = file(&scratch, name, png);
            let error = check_raster(&path).unwrap_err();
            assert!(error.to_lowercase().contains("limit"), "{name}: {error}");
            let error = load_within(path, (116, 116)).unwrap_err();
            assert!(error.to_lowercase().contains("limit"), "{name}: {error}");
        }
    }

    #[test]
    fn only_png_jpeg_and_gif_pictures_are_decoded() {
        let scratch = Scratch::new("formats");
        let mut bmp = Vec::new();
        RgbaImage::new(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .ok();
        // Whether or not this build can write a BMP, the magic alone is
        // enough to be refused on.
        if bmp.is_empty() {
            bmp = b"BM\0\0\0\0".to_vec();
        }
        let error = load_within(file(&scratch, "icon.png", &bmp), (64, 64)).unwrap_err();
        assert!(error.contains("not a PNG"), "{error}");
        assert!(load_within(file(&scratch, "text.png", "hello"), (64, 64)).is_err());
    }

    #[test]
    fn a_picture_file_is_never_enlarged_but_a_theme_png_fills_the_box() {
        let scratch = Scratch::new("enlarge");
        let path = scratch.0.join("small.png");
        RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let file = load(&path, (152, 116), Origin::File, None).unwrap();
        assert_eq!(file.dimensions(), (16, 16));
        let theme = load(&path, (152, 116), Origin::Theme, None).unwrap();
        assert_eq!(theme.dimensions(), (116, 116));

        // Either way a larger one is shrunk to fit.
        let path = scratch.0.join("large.png");
        RgbaImage::new(400, 200).save(&path).unwrap();
        let file = load(&path, (152, 116), Origin::File, None).unwrap();
        assert_eq!(file.dimensions(), (152, 76));
    }

    #[test]
    fn tint_keeps_each_pixels_alpha_and_replaces_its_colour() {
        let mut image = RgbaImage::from_fn(3, 1, |x, _| match x {
            0 => image::Rgba([46, 52, 54, 255]),
            1 => image::Rgba([128, 128, 128, 128]),
            _ => image::Rgba([9, 9, 9, 0]),
        });
        tint(&mut image, Rgb::new(200, 100, 50));
        assert_eq!(image.get_pixel(0, 0).0, [200, 100, 50, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [200, 100, 50, 128]);
        assert_eq!(image.get_pixel(2, 0).0[3], 0);
    }

    #[test]
    fn a_symbolic_icon_is_drawn_in_the_label_colour_and_a_full_colour_one_is_not() {
        let scratch = Scratch::new("symbolic");
        let symbolic = file(&scratch, "wifi-symbolic.svg", SYMBOLIC_SVG);
        let full = file(&scratch, "wifi.svg", RED_SVG);
        assert!(is_symbolic(&symbolic));
        assert!(!is_symbolic(&full));
        assert!(!is_symbolic(Path::new("wifi-symbolic.symbolic.png")));

        let label = Rgb::new(200, 100, 50);
        let mut cache = IconCache::default();
        let drawn = cache
            .get(&symbolic, Origin::Theme, (32, 32), label)
            .unwrap();
        assert_eq!(drawn.dimensions(), (32, 32));
        // The opaque half in the label colour; the half-transparent half in
        // the label colour at about half coverage.
        assert_eq!(drawn.get_pixel(4, 16).0, [200, 100, 50, 255]);
        let faint = drawn.get_pixel(28, 16).0;
        assert_eq!(faint[..3], [200, 100, 50]);
        assert!((120..=136).contains(&faint[3]), "{faint:?}");

        let drawn = cache.get(&full, Origin::Theme, (32, 32), label).unwrap();
        assert_eq!(drawn.get_pixel(16, 16).0, [255, 0, 0, 255]);
    }

    #[test]
    fn the_cache_hands_back_the_same_drawing_until_something_changes() {
        let scratch = Scratch::new("cache");
        let path = file(&scratch, "wifi-symbolic.svg", SYMBOLIC_SVG);
        let white = Rgb::new(255, 255, 255);
        let mut cache = IconCache::default();

        let first = cache.get(&path, Origin::Theme, (64, 64), white).unwrap();
        let again = cache.get(&path, Origin::Theme, (64, 64), white).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(cache.len(), 1);

        // Another size, colour or origin is another drawing.
        let other = cache.get(&path, Origin::Theme, (32, 32), white).unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        let recoloured = cache
            .get(&path, Origin::Theme, (64, 64), Rgb::BLACK)
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &recoloured));
        assert_eq!(recoloured.get_pixel(4, 32).0, [0, 0, 0, 255]);
        let as_file = cache.get(&path, Origin::File, (64, 64), white).unwrap();
        assert!(!Arc::ptr_eq(&first, &as_file));

        // A full-colour icon ignores the label colour, so a new one does not
        // draw it again.
        let full = file(&scratch, "full.svg", RED_SVG);
        let red = cache.get(&full, Origin::Theme, (64, 64), white).unwrap();
        let still = cache
            .get(&full, Origin::Theme, (64, 64), Rgb::BLACK)
            .unwrap();
        assert!(Arc::ptr_eq(&red, &still));

        // An edited file is drawn again.
        fs::write(&path, RED_SVG.replace("#ff0000", "#000000")).unwrap();
        let edited = cache.get(&path, Origin::Theme, (64, 64), white).unwrap();
        assert!(!Arc::ptr_eq(&first, &edited));
    }

    #[test]
    fn the_cache_forgets_what_was_used_longest_ago() {
        let scratch = Scratch::new("lru");
        let one = file(&scratch, "one.svg", RED_SVG);
        let two = file(&scratch, "two.svg", RED_SVG);
        let three = file(&scratch, "three.svg", RED_SVG);
        let white = Rgb::new(255, 255, 255);
        let mut cache = IconCache::new(2);

        let first_one = cache.get(&one, Origin::Theme, (16, 16), white).unwrap();
        let first_two = cache.get(&two, Origin::Theme, (16, 16), white).unwrap();
        // One is used again, so two is now the older.
        cache.get(&one, Origin::Theme, (16, 16), white).unwrap();
        cache.get(&three, Origin::Theme, (16, 16), white).unwrap();
        assert_eq!(cache.len(), 2);

        let kept = cache.get(&one, Origin::Theme, (16, 16), white).unwrap();
        assert!(Arc::ptr_eq(&first_one, &kept));
        let redrawn = cache.get(&two, Origin::Theme, (16, 16), white).unwrap();
        assert!(!Arc::ptr_eq(&first_two, &redrawn));
    }

    #[test]
    fn an_icon_that_cannot_be_drawn_is_remembered_rather_than_retried() {
        let scratch = Scratch::new("broken");
        let broken = file(&scratch, "broken.svg", "not an svg");
        let mut cache = IconCache::default();
        assert!(cache
            .get(&broken, Origin::File, (16, 16), Rgb::BLACK)
            .is_none());
        assert!(cache
            .get(&broken, Origin::File, (16, 16), Rgb::BLACK)
            .is_none());
        assert_eq!(cache.len(), 1);

        // Fixed on disk, it is drawn.
        fs::write(&broken, RED_SVG).unwrap();
        assert!(cache
            .get(&broken, Origin::File, (16, 16), Rgb::BLACK)
            .is_some());
    }

    #[test]
    fn the_cache_remembers_why_an_icon_cannot_be_drawn() {
        let scratch = Scratch::new("why");
        let refused = file(&scratch, "doctype.svg", DOCTYPE_SVG);
        let mut cache = IconCache::default();
        for _ in 0..2 {
            let error = cache
                .draw(&refused, Origin::File, (16, 16), Rgb::BLACK)
                .unwrap_err();
            assert!(error.contains("DOCTYPE"), "{error}");
        }
        assert_eq!(cache.len(), 1);
    }

    /// The desktop's real themes, read only. Run by hand with
    /// `cargo test -p galdeck-daemon --lib icons -- --ignored`.
    #[test]
    #[ignore = "reads the icon themes installed on this machine"]
    fn a_real_symbolic_icon_resolves_and_draws() {
        let themes = IconThemes::load(search_roots(), None);
        let path = themes
            .resolve("network-wireless-symbolic", (152, 116))
            .expect("Adwaita has network-wireless-symbolic");
        let drawn = load(&path, (152, 116), Origin::Theme, Some(Rgb::WHITE)).unwrap();
        assert!(opaque_pixels(&drawn) > 0);
        assert!(themes.symbolic_names().len() > 100);
        eprintln!(
            "{} themes: {:?}",
            themes.theme_names().len(),
            themes.theme_names()
        );
    }
}
