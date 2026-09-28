//! Logos of popular apps, compiled in, for keys that start them.
//!
//! Each is an icon name, looked up like any other: `logo-spotify` draws the
//! logo in white on a tile of the brand's colour, the way a launcher shows
//! the app, and `logo-spotify-symbolic` draws the logo alone in the key's
//! label colour, like the desktop's own symbolic icons.
//!
//! The logos are from Simple Icons, which publishes them under CC0. Only
//! logos it lists with no licence of their own are here: the rest are GPL,
//! CC-BY-SA and the like, which a tile made from them would carry. They are
//! still their owners' trademarks; see `logos/README.md`.
//!
//! Icons are looked up and drawn from files, with every check that makes a
//! file safe to draw, so the logos are written once to a directory of their
//! own in the cache directory, which [`crate::icons::IconThemes::discover`]
//! searches after the icon themes. The directory is named for what is in it,
//! so a daemon with other logos writes its own rather than fighting over one.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What every logo's icon name starts with.
pub const PREFIX: &str = "logo-";

/// How round a tile's corners are, in the logos' 24-unit square.
const TILE_RADIUS: &str = "5";
/// Where the logo sits on its tile: centred, at 60% of it each way, which
/// leaves margin enough to read as a tile and keeps the logo large on a key.
const TILE_LOGO: &str = "translate(4.8 4.8) scale(0.6)";
/// Brand colours brighter than this, as relative luminance, get a black logo
/// on their tile rather than a white one: Linux yellow, Hugging Face
/// yellow. Well above the halfway point for contrast, since a white logo on
/// a mid colour is how most of these apps show themselves.
const LIGHT_TILE: f64 = 0.5;

/// Written last into the directory, so one without it is not used.
const COMPLETE: &str = ".complete";

/// One logo.
#[derive(Debug)]
pub struct Logo {
    /// What follows [`PREFIX`] in its icon name.
    pub name: &'static str,
    /// The app's name, as the picker shows it.
    pub title: &'static str,
    /// The brand's colour, as six hex digits.
    pub hex: &'static str,
    /// The heading the picker puts it under.
    pub category: &'static str,
    /// The file as Simple Icons has it: one path on a 24-unit square.
    svg: &'static str,
}

macro_rules! logo {
    ($name:literal, $title:literal, $hex:literal, $category:expr) => {
        Logo {
            name: $name,
            title: $title,
            hex: $hex,
            category: $category,
            svg: include_str!(concat!("../logos/", $name, ".svg")),
        }
    };
}

impl Logo {
    /// Its icon name, drawn on its tile.
    pub fn icon_name(&self) -> String {
        format!("{PREFIX}{}", self.name)
    }

    /// Its icon name, drawn alone in the label's colour.
    pub fn symbolic_name(&self) -> String {
        format!("{PREFIX}{}-symbolic", self.name)
    }

    /// The logo alone, in black, for the key to recolour.
    pub fn symbolic_svg(&self) -> &'static str {
        self.svg
    }

    /// The logo in white, or black on a light colour, on a rounded tile of
    /// the brand's colour.
    pub fn tile_svg(&self) -> String {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\">\
             <rect width=\"24\" height=\"24\" rx=\"{TILE_RADIUS}\" fill=\"#{hex}\"/>\
             <path transform=\"{TILE_LOGO}\" fill=\"{fill}\" d=\"{d}\"/></svg>",
            hex = self.hex,
            fill = self.tile_logo_colour(),
            d = self.path_data(),
        )
    }

    /// The logo's one path, as drawing commands.
    fn path_data(&self) -> &'static str {
        let start = self.svg.find(" d=\"").map(|at| at + 4).unwrap_or(0);
        let len = self.svg[start..].find('"').unwrap_or(0);
        &self.svg[start..start + len]
    }

    fn tile_logo_colour(&self) -> &'static str {
        let channel = |at: usize| {
            let value = u8::from_str_radix(&self.hex[at..at + 2], 16).unwrap_or(0);
            let c = f64::from(value) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4);
        if luminance > LIGHT_TILE {
            "#000000"
        } else {
            "#FFFFFF"
        }
    }
}

/// The logo called `name`, without [`PREFIX`] or `-symbolic`.
pub fn find(name: &str) -> Option<&'static Logo> {
    LOGOS.iter().find(|logo| logo.name == name)
}

/// Every logo's symbolic name, for the names an icon field suggests along
/// with the themes' symbolic ones. The tiles are picked from the editor's
/// picker, which shows them.
pub fn symbolic_names() -> impl Iterator<Item = String> {
    LOGOS.iter().map(Logo::symbolic_name)
}

/// The logos for the editor's picker, as JSON: each one's names, title,
/// colour and category, and both drawings, so what the picker shows is
/// what the key will.
pub fn catalogue_json() -> &'static str {
    static JSON: OnceLock<String> = OnceLock::new();
    JSON.get_or_init(|| {
        let logos: Vec<_> = LOGOS
            .iter()
            .map(|logo| {
                serde_json::json!({
                    "name": logo.icon_name(),
                    "symbolic": logo.symbolic_name(),
                    "title": logo.title,
                    "hex": format!("#{}", logo.hex),
                    "category": logo.category,
                    "tile_svg": logo.tile_svg(),
                    "symbolic_svg": logo.symbolic_svg(),
                })
            })
            .collect();
        serde_json::json!({ "logos": logos }).to_string()
    })
}

/// The directory the logos are drawn from, written first if it is not
/// there yet. `None`, said in the log, when there is no cache directory or
/// it cannot be written: the logos are then names no theme has.
pub fn dir() -> Option<PathBuf> {
    let Some(cache) = cache_home() else {
        log::warn!("app logos: no cache directory (neither $XDG_CACHE_HOME nor $HOME is set)");
        return None;
    };
    write_under(&cache.join("galdeck"))
        .map_err(|e| log::warn!("app logos: {e}"))
        .ok()
}

/// `$XDG_CACHE_HOME`, else ~/.cache, as long as it is absolute.
fn cache_home() -> Option<PathBuf> {
    let absolute = |dir: PathBuf| dir.is_absolute().then_some(dir);
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .and_then(absolute)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .and_then(absolute)
                .map(|home| home.join(".cache"))
        })
}

/// The logos' directory in `parent`, written unless a complete one is
/// already there. [`dir`] is this in the cache directory.
///
/// Written under a name of this process's and renamed into place, so a
/// daemon starting beside this one never finds it half written; if that
/// daemon renames its copy in first, this one's is dropped.
pub fn write_under(parent: &Path) -> Result<PathBuf, String> {
    let name = format!("logos-{:016x}", fingerprint());
    let dir = parent.join(&name);
    if dir.join(COMPLETE).is_file() {
        return Ok(dir);
    }
    std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    let staging = parent.join(format!(".{name}.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let written = write_files(&staging);
    let placed = written.and_then(|()| match std::fs::rename(&staging, &dir) {
        Ok(()) => Ok(()),
        Err(_) if dir.join(COMPLETE).is_file() => Ok(()),
        // Left from something other than a daemon writing it, since those
        // rename a finished one into place. Replaced.
        Err(_) => std::fs::remove_dir_all(&dir)
            .and_then(|()| std::fs::rename(&staging, &dir))
            .map_err(|e| format!("putting the logos in {}: {e}", dir.display())),
    });
    let _ = std::fs::remove_dir_all(&staging);
    placed.map(|()| dir)
}

fn write_files(dir: &Path) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("writing the logos to {}: {e}", dir.display());
    std::fs::create_dir(dir).map_err(failed)?;
    for (file, contents) in files() {
        std::fs::write(dir.join(file), contents).map_err(failed)?;
    }
    std::fs::File::create(dir.join(COMPLETE))
        .map(drop)
        .map_err(failed)
}

/// Every file the directory holds, by name.
fn files() -> impl Iterator<Item = (String, String)> {
    LOGOS.iter().flat_map(|logo| {
        [
            (format!("{}.svg", logo.icon_name()), logo.tile_svg()),
            (
                format!("{}.svg", logo.symbolic_name()),
                logo.symbolic_svg().to_string(),
            ),
        ]
    })
}

/// A hash of every file, FNV-1a: the same from one build to the next,
/// which the standard library's hasher does not promise.
fn fingerprint() -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (file, contents) in files() {
        for byte in file.bytes().chain([0]).chain(contents.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// The picker's headings, in its order.
const BROWSERS: &str = "Browsers";
const CHAT: &str = "Chat";
const MEDIA: &str = "Music & video";
const GAMES: &str = "Games";
const CREATIVE: &str = "Streaming & creative";
const DEVELOPMENT: &str = "Development";
const AI: &str = "AI";
const TERMINAL: &str = "Terminal";
const OFFICE: &str = "Office & notes";
const SYSTEM: &str = "System";

/// Every logo, in the order the picker shows them: each heading's in one
/// run.
pub const LOGOS: &[Logo] = &[
    logo!("firefox", "Firefox", "FF7139", BROWSERS),
    logo!("chrome", "Google Chrome", "4285F4", BROWSERS),
    logo!("brave", "Brave", "FB542B", BROWSERS),
    logo!("opera", "Opera", "FF1B2D", BROWSERS),
    logo!("tor-browser", "Tor Browser", "7D4698", BROWSERS),
    logo!("librewolf", "LibreWolf", "00ACFF", BROWSERS),
    logo!("zen-browser", "Zen Browser", "F76F53", BROWSERS),
    logo!("duckduckgo", "DuckDuckGo", "DE5833", BROWSERS),
    logo!("discord", "Discord", "5865F2", CHAT),
    logo!("telegram", "Telegram", "26A5E4", CHAT),
    logo!("signal", "Signal", "3B45FD", CHAT),
    logo!("whatsapp", "WhatsApp", "25D366", CHAT),
    logo!("messenger", "Messenger", "0866FF", CHAT),
    logo!("zoom", "Zoom", "0B5CFF", CHAT),
    logo!("google-meet", "Google Meet", "00897B", CHAT),
    logo!("element", "Element", "0DBD8B", CHAT),
    logo!("matrix", "Matrix", "000000", CHAT),
    logo!("teamspeak", "TeamSpeak", "4B69B6", CHAT),
    logo!("mumble", "Mumble", "000000", CHAT),
    logo!("thunderbird", "Thunderbird", "0A84FF", CHAT),
    logo!("gmail", "Gmail", "EA4335", CHAT),
    logo!("proton-mail", "Proton Mail", "6D4AFF", CHAT),
    logo!("mastodon", "Mastodon", "6364FF", CHAT),
    logo!("bluesky", "Bluesky", "1185FE", CHAT),
    logo!("reddit", "Reddit", "FF4500", CHAT),
    logo!("x", "X", "000000", CHAT),
    logo!("spotify", "Spotify", "1ED760", MEDIA),
    logo!("youtube", "YouTube", "FF0000", MEDIA),
    logo!("youtube-music", "YouTube Music", "FF0000", MEDIA),
    logo!("twitch", "Twitch", "9146FF", MEDIA),
    logo!("netflix", "Netflix", "E50914", MEDIA),
    logo!("plex", "Plex", "EBAF00", MEDIA),
    logo!("jellyfin", "Jellyfin", "00A4DC", MEDIA),
    logo!("kodi", "Kodi", "17B2E7", MEDIA),
    logo!("vlc", "VLC", "FF8800", MEDIA),
    logo!("mpv", "mpv", "691F69", MEDIA),
    logo!("apple-music", "Apple Music", "FA243C", MEDIA),
    logo!("soundcloud", "SoundCloud", "FF5500", MEDIA),
    logo!("tidal", "TIDAL", "000000", MEDIA),
    logo!("deezer", "Deezer", "A238FF", MEDIA),
    logo!("crunchyroll", "Crunchyroll", "FF5E00", MEDIA),
    logo!("stremio", "Stremio", "685CEE", MEDIA),
    logo!("lastfm", "Last.fm", "D51007", MEDIA),
    logo!("pocket-casts", "Pocket Casts", "F43E37", MEDIA),
    logo!("audacity", "Audacity", "0000CC", MEDIA),
    logo!("steam", "Steam", "000000", GAMES),
    logo!("epic-games", "Epic Games", "313131", GAMES),
    logo!("gog", "GOG", "86328A", GAMES),
    logo!("heroic", "Heroic", "4B93FF", GAMES),
    logo!("lutris", "Lutris", "FF9900", GAMES),
    logo!("battle-net", "Battle.net", "4381C3", GAMES),
    logo!("ea", "EA", "000000", GAMES),
    logo!("ubisoft", "Ubisoft", "000000", GAMES),
    logo!("playstation", "PlayStation", "0070D1", GAMES),
    logo!("retroarch", "RetroArch", "000000", GAMES),
    logo!("itch-io", "itch.io", "FA5C5C", GAMES),
    logo!("protondb", "ProtonDB", "F50057", GAMES),
    logo!("lichess", "Lichess", "000000", GAMES),
    logo!("chess-com", "Chess.com", "81B64C", GAMES),
    logo!("obs", "OBS Studio", "302E31", CREATIVE),
    logo!("streamlabs", "Streamlabs", "80F5D2", CREATIVE),
    logo!("elgato", "Elgato", "101010", CREATIVE),
    logo!("krita", "Krita", "3BABFF", CREATIVE),
    logo!("blender", "Blender", "E87D0D", CREATIVE),
    logo!("kdenlive", "Kdenlive", "527EB2", CREATIVE),
    logo!("davinci-resolve", "DaVinci Resolve", "233A51", CREATIVE),
    logo!("figma", "Figma", "F24E1E", CREATIVE),
    logo!("penpot", "Penpot", "000000", CREATIVE),
    logo!("excalidraw", "Excalidraw", "6965DB", CREATIVE),
    logo!("ardour", "Ardour", "C61C3E", CREATIVE),
    logo!("freecad", "FreeCAD", "418FDE", CREATIVE),
    logo!("openscad", "OpenSCAD", "F9D72C", CREATIVE),
    logo!("vscodium", "VSCodium", "2F80ED", DEVELOPMENT),
    logo!("zed", "Zed", "084CCF", DEVELOPMENT),
    logo!("cursor", "Cursor", "000000", DEVELOPMENT),
    logo!("sublime-text", "Sublime Text", "FF9800", DEVELOPMENT),
    logo!("vim", "Vim", "019733", DEVELOPMENT),
    logo!("jetbrains", "JetBrains", "000000", DEVELOPMENT),
    logo!("intellij-idea", "IntelliJ IDEA", "000000", DEVELOPMENT),
    logo!("pycharm", "PyCharm", "000000", DEVELOPMENT),
    logo!("webstorm", "WebStorm", "000000", DEVELOPMENT),
    logo!("clion", "CLion", "000000", DEVELOPMENT),
    logo!("rider", "Rider", "000000", DEVELOPMENT),
    logo!("goland", "GoLand", "000000", DEVELOPMENT),
    logo!("github", "GitHub", "181717", DEVELOPMENT),
    logo!("gitlab", "GitLab", "FC6D26", DEVELOPMENT),
    logo!("gitea", "Gitea", "609926", DEVELOPMENT),
    logo!("docker", "Docker", "2496ED", DEVELOPMENT),
    logo!("podman", "Podman", "892CA0", DEVELOPMENT),
    logo!("kubernetes", "Kubernetes", "326CE5", DEVELOPMENT),
    logo!("postman", "Postman", "FF6C37", DEVELOPMENT),
    logo!("insomnia", "Insomnia", "4000BF", DEVELOPMENT),
    logo!("dbeaver", "DBeaver", "382923", DEVELOPMENT),
    logo!("jupyter", "Jupyter", "F37626", DEVELOPMENT),
    logo!("python", "Python", "3776AB", DEVELOPMENT),
    logo!("nodejs", "Node.js", "5FA04E", DEVELOPMENT),
    logo!("arduino", "Arduino", "00878F", DEVELOPMENT),
    logo!("claude", "Claude", "D97757", AI),
    logo!("ollama", "Ollama", "000000", AI),
    logo!("gemini", "Gemini", "8E75B2", AI),
    logo!("perplexity", "Perplexity", "1FB8CD", AI),
    logo!("deepseek", "DeepSeek", "5786FE", AI),
    logo!("mistral", "Mistral", "FA520F", AI),
    logo!("hugging-face", "Hugging Face", "FFD21E", AI),
    logo!("gnome-terminal", "GNOME Terminal", "241F31", TERMINAL),
    logo!("wezterm", "WezTerm", "4E49EE", TERMINAL),
    logo!("ghostty", "Ghostty", "3551F3", TERMINAL),
    logo!("warp", "Warp", "01A4FF", TERMINAL),
    logo!("iterm2", "iTerm2", "000000", TERMINAL),
    logo!("tmux", "tmux", "1BB91F", TERMINAL),
    logo!("htop", "htop", "009020", TERMINAL),
    logo!("libreoffice", "LibreOffice", "18A303", OFFICE),
    logo!("onlyoffice", "ONLYOFFICE", "444444", OFFICE),
    logo!("obsidian", "Obsidian", "7C3AED", OFFICE),
    logo!("notion", "Notion", "000000", OFFICE),
    logo!("joplin", "Joplin", "1071D3", OFFICE),
    logo!("logseq", "Logseq", "85C8C8", OFFICE),
    logo!("evernote", "Evernote", "00A82D", OFFICE),
    logo!("todoist", "Todoist", "E44332", OFFICE),
    logo!("trello", "Trello", "0052CC", OFFICE),
    logo!("zotero", "Zotero", "CC2936", OFFICE),
    logo!("overleaf", "Overleaf", "47A141", OFFICE),
    logo!("google-calendar", "Google Calendar", "4285F4", OFFICE),
    logo!("google-drive", "Google Drive", "4285F4", OFFICE),
    logo!("google-keep", "Google Keep", "FFBB00", OFFICE),
    logo!("google-maps", "Google Maps", "4285F4", OFFICE),
    logo!("google-photos", "Google Photos", "4285F4", OFFICE),
    logo!("dropbox", "Dropbox", "0061FF", OFFICE),
    logo!("nextcloud", "Nextcloud", "0082C9", OFFICE),
    logo!("proton-drive", "Proton Drive", "EB508D", OFFICE),
    logo!("1password", "1Password", "145FE4", OFFICE),
    logo!("bitwarden", "Bitwarden", "175DDC", OFFICE),
    logo!("keepassxc", "KeePassXC", "6CAC4D", OFFICE),
    logo!("linux", "Linux", "FCC624", SYSTEM),
    logo!("ubuntu", "Ubuntu", "E95420", SYSTEM),
    logo!("arch-linux", "Arch Linux", "1793D1", SYSTEM),
    logo!("linux-mint", "Linux Mint", "86BE43", SYSTEM),
    logo!("pop-os", "Pop!_OS", "48B9C7", SYSTEM),
    logo!("opensuse", "openSUSE", "73BA25", SYSTEM),
    logo!("manjaro", "Manjaro", "35BFA4", SYSTEM),
    logo!("endeavouros", "EndeavourOS", "7F7FFF", SYSTEM),
    logo!("gnome", "GNOME", "4A86CF", SYSTEM),
    logo!("kde", "KDE", "1D99F3", SYSTEM),
    logo!("xfce", "XFCE", "2284F2", SYSTEM),
    logo!("hyprland", "Hyprland", "58E1FF", SYSTEM),
    logo!("wine", "Wine", "800000", SYSTEM),
    logo!("virtualbox", "VirtualBox", "2F61B4", SYSTEM),
    logo!("vmware", "VMware", "607078", SYSTEM),
    logo!("qemu", "QEMU", "FF6600", SYSTEM),
    logo!("proxmox", "Proxmox", "E57000", SYSTEM),
    logo!("wireshark", "Wireshark", "1679A7", SYSTEM),
    logo!("tailscale", "Tailscale", "242424", SYSTEM),
    logo!("wireguard", "WireGuard", "88171A", SYSTEM),
    logo!("proton-vpn", "Proton VPN", "66DEB1", SYSTEM),
    logo!("mullvad", "Mullvad", "294D73", SYSTEM),
    logo!("syncthing", "Syncthing", "0891D1", SYSTEM),
    logo!("qbittorrent", "qBittorrent", "2F67BA", SYSTEM),
    logo!("transmission", "Transmission", "D70008", SYSTEM),
    logo!("home-assistant", "Home Assistant", "18BCF2", SYSTEM),
    logo!("raspberry-pi", "Raspberry Pi", "A22846", SYSTEM),
    logo!("octoprint", "OctoPrint", "13C100", SYSTEM),
    logo!("bambu-lab", "Bambu Lab", "00AE42", SYSTEM),
    logo!("qgis", "QGIS", "589632", SYSTEM),
    logo!("nvidia", "NVIDIA", "76B900", SYSTEM),
    logo!("amd", "AMD", "ED1C24", SYSTEM),
    logo!("corsair", "Corsair", "231F20", SYSTEM),
];
