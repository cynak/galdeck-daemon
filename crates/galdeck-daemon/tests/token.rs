//! The UI token outlives the daemon that minted it.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::Arc;

use galdeck_core::wake_channel;
use galdeck_daemon::engine::ControlSender;
use galdeck_daemon::http::HttpServer;
use galdeck_daemon::preview::Preview;

fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("galdeck-token-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn server(token_file: &std::path::Path) -> HttpServer {
    let (waker, _wake_rx) = wake_channel();
    let (tx, _rx) = channel();
    HttpServer::bind_with_token(
        0,
        ControlSender::new(tx, waker),
        Preview::new(),
        Arc::new(AtomicBool::new(false)),
        token_file.to_path_buf(),
    )
    .expect("binding to an ephemeral loopback port")
}

#[test]
fn a_restart_keeps_the_same_token() {
    // A fresh token every run means every open tab is holding a dead one the
    // moment the daemon restarts, and the token is stripped from the address
    // bar on load, so a refresh cannot recover it either.
    let file = scratch("reuse");
    let first = server(&file).url();
    let second = server(&file).url();

    let token_of = |url: &str| url.split("token=").nth(1).unwrap().to_string();
    assert_eq!(token_of(&first), token_of(&second));

    let _ = std::fs::remove_file(&file);
}

#[test]
fn the_token_file_is_not_readable_by_anyone_else() {
    let file = scratch("mode");
    let _ = server(&file);
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "token file mode was {mode:o}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn a_token_file_anyone_can_read_is_replaced_rather_than_trusted() {
    let file = scratch("loose");
    std::fs::write(&file, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    let url = server(&file).url();
    assert!(
        !url.contains("aaaaaaaaaaaa"),
        "a world-readable token must not be reused: {url}"
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let _ = std::fs::remove_file(&file);
}

#[test]
fn a_file_that_is_not_a_token_is_replaced() {
    let file = scratch("junk");
    std::fs::write(&file, "not a token at all").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let url = server(&file).url();
    let token = url.split("token=").nth(1).unwrap();
    assert_eq!(token.len(), 48);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    let _ = std::fs::remove_file(&file);
}
