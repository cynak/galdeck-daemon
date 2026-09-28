//! Pictures kept in the config directory's `assets/`, for icons, backgrounds
//! and widgets: uploaded from the editor, or downloaded from a link pasted
//! into it.
//!
//! Either way the config ends up naming a file on disk, never the link. A key
//! reads its icon every time it is drawn, and that must not wait on someone
//! else's server, or break the day the link does.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Largest picture a link may bring. Uploads from the editor stop at 3 MB,
/// and nothing drawn on a key needs anything like this much.
pub const MAX_DOWNLOAD: u64 = 4 * 1024 * 1024;
/// How long a download may take, start to finish. Under the control socket's
/// ten seconds, so a slow server is reported as slow rather than as a daemon
/// that stopped answering.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = concat!("galdeck-daemon/", env!("CARGO_PKG_VERSION"));

/// Where pictures are kept, as an absolute path: a key's `icon` without a `/`
/// in it would be read as an icon theme name rather than a file.
pub fn dir(config_dir: &Path) -> PathBuf {
    let dir = config_dir.join("assets");
    std::path::absolute(&dir).unwrap_or(dir)
}

/// Keep a picture in `dir` under a plain version of `name`, and say where.
///
/// Only PNG, JPEG and GIF, which is what a key can draw. Never overwrites: a
/// picture another page uses would change under it. The same bytes again
/// reuse the file already there.
pub fn store(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let Some(format) = picture_format(bytes) else {
        return Err("only PNG, JPEG and GIF pictures can be used".into());
    };
    let extension = format.extensions_str()[0];
    // A plain name: letters, digits, dashes and underscores, whatever was
    // sent. Nothing that could climb out of the directory or hide.
    let stem: String = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("picture")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "picture" } else { stem };
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    for n in 0..1000 {
        let file = if n == 0 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem}-{n}.{extension}")
        };
        let path = dir.join(file);
        match std::fs::read(&path) {
            Ok(existing) if existing == bytes => return Ok(path),
            Ok(_) => continue,
            Err(_) => {}
        }
        // `create_new`, because a download finishes on a thread of its own
        // and could otherwise land on a name an upload took a moment ago.
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        let mut file = match created {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("writing {}: {e}", path.display())),
        };
        if let Err(e) = file.write_all(bytes) {
            // Half a picture would be reused as if it were the whole one.
            let _ = std::fs::remove_file(&path);
            return Err(format!("writing {}: {e}", path.display()));
        }
        return Ok(path);
    }
    Err(format!("too many pictures called {stem}"))
}

/// Fetch a picture from a link and keep it in `dir`.
///
/// Blocking, for up to [`DOWNLOAD_TIMEOUT`], so never on the deck's thread.
pub fn download(url: &str, dir: &Path) -> Result<PathBuf, String> {
    let url = url.trim();
    let web = ["http://", "https://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    });
    if !web {
        return Err("a link has to start with http:// or https://".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let bytes = agent
        .get(url)
        .call()
        .map_err(failed)?
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(failed)?;
    if picture_format(&bytes).is_none() {
        return Err(not_a_picture(&bytes).into());
    }
    store(dir, name_from_url(url), &bytes)
}

fn picture_format(bytes: &[u8]) -> Option<image::ImageFormat> {
    use image::ImageFormat::{Gif, Jpeg, Png};
    match image::guess_format(bytes) {
        Ok(format @ (Png | Jpeg | Gif)) => Some(format),
        _ => None,
    }
}

/// Why a download is refused, in terms of what was probably pasted.
fn not_a_picture(bytes: &[u8]) -> &'static str {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_ascii_lowercase();
    let head = head.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("<!doctype html") || head.starts_with("<html") {
        "that link is a web page, not a picture: use the picture's own address \
         (right-click it and copy the image address)"
    } else if head.contains("<svg") {
        "that link is an SVG, and only PNG, JPEG and GIF pictures can be used"
    } else {
        "that link is not a PNG, JPEG or GIF picture"
    }
}

/// A name for a picture from where it came from: the link's last path
/// segment, which is usually the file's own name. `store` makes it safe.
fn name_from_url(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    rest.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("picture")
}

/// Word a failed download for whoever pasted the link. The link itself is
/// left out of the log, since links sometimes carry a key.
fn failed(e: ureq::Error) -> String {
    log::warn!("downloading a picture: {e}");
    match e {
        ureq::Error::StatusCode(code) => format!("the link answered {code}"),
        ureq::Error::BodyExceedsLimit(_) => {
            format!("that picture is over {} MB", MAX_DOWNLOAD / 1024 / 1024)
        }
        e => format!("downloading it failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest of a picture";

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("galdeck-assets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_link_is_named_after_its_last_segment() {
        assert_eq!(
            name_from_url("https://a.example/icons/play.png"),
            "play.png"
        );
        assert_eq!(
            name_from_url("https://a.example/play.png?size=64#x"),
            "play.png"
        );
        assert_eq!(name_from_url("https://a.example/icons/"), "icons");
        assert_eq!(name_from_url("https://a.example"), "a.example");
        assert_eq!(name_from_url("https://a.example/?q=1"), "a.example");
    }

    #[test]
    fn a_stored_name_cannot_leave_the_directory() {
        let dir = scratch("climb");
        let path = store(&dir, "../../etc/passwd.png", PNG).unwrap();
        assert_eq!(path, dir.join("passwd.png"));
        let path = store(&dir, "my icon (1).png", b"\x89PNG\r\n\x1a\nanother").unwrap();
        assert_eq!(path, dir.join("my-icon--1.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_picture_again_reuses_its_file_and_a_different_one_does_not() {
        let dir = scratch("reuse");
        let first = store(&dir, "play.png", PNG).unwrap();
        assert_eq!(store(&dir, "play.png", PNG).unwrap(), first);
        let other = store(&dir, "play.png", b"\x89PNG\r\n\x1a\nnot the same").unwrap();
        assert_eq!(other, dir.join("play-1.png"));
        assert_eq!(std::fs::read(&first).unwrap(), PNG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_pictures_a_key_can_draw_are_kept() {
        let dir = scratch("formats");
        assert!(store(&dir, "page.html", b"<!doctype html><p>hi").is_err());
        assert!(!dir.join("page.html").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_download_says_what_it_probably_was() {
        assert!(not_a_picture(b"\n  <!DOCTYPE html><html>").contains("web page"));
        assert!(not_a_picture(b"<?xml version=\"1.0\"?><svg xmlns=").contains("SVG"));
        assert!(not_a_picture(b"GIF").contains("not a PNG"));
    }

    #[test]
    fn the_directory_is_absolute_so_a_path_is_never_taken_for_an_icon_name() {
        assert!(dir(Path::new("relative/config")).is_absolute());
        assert!(dir(Path::new("relative/config")).ends_with("relative/config/assets"));
    }

    /// Answer one request on loopback with `status` and `body`, and give the
    /// link to ask it at.
    fn serve_once(path: &str, status: &str, body: Vec<u8>) -> String {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        });
        format!("http://127.0.0.1:{port}{path}")
    }

    #[test]
    fn a_picture_at_a_link_is_kept_under_the_link_s_name() {
        let dir = scratch("download");
        let link = serve_once("/icons/play.png?v=2", "200 OK", PNG.to_vec());
        let path = download(&link, &dir).unwrap();
        assert_eq!(path, dir.join("play.png"));
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_to_a_page_or_a_missing_picture_keeps_nothing() {
        let dir = scratch("refused");
        let page = serve_once("/gallery", "200 OK", b"<!DOCTYPE html><p>".to_vec());
        assert!(download(&page, &dir).unwrap_err().contains("web page"));
        let missing = serve_once("/gone.png", "404 Not Found", Vec::new());
        assert!(download(&missing, &dir).unwrap_err().contains("404"));
        assert!(!dir.exists());
    }

    #[test]
    fn a_download_stops_at_the_limit() {
        let dir = scratch("huge");
        let mut huge = PNG.to_vec();
        huge.resize(MAX_DOWNLOAD as usize + 1, 0);
        let link = serve_once("/huge.png", "200 OK", huge);
        assert!(download(&link, &dir).unwrap_err().contains("over 4 MB"));
        assert!(!dir.exists());
    }

    #[test]
    fn only_web_links_are_fetched() {
        let dir = scratch("scheme");
        for link in ["file:///etc/passwd", "ftp://a.example/x.png", "/tmp/x.png"] {
            let refused = download(link, &dir).unwrap_err();
            assert!(refused.contains("http"), "{link}: {refused}");
        }
        assert!(!dir.exists());
    }
}
