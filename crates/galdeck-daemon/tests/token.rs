//! The UI token outlives the daemon that minted it, until it is replaced.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::Arc;

use galdeck_core::wake_channel;
use galdeck_daemon::engine::ControlSender;
use galdeck_daemon::http::{remove_legacy_token, token_path_for, HttpServer};
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
    let first = server(&file).logins().token();
    let second = server(&file).logins().token();
    assert_eq!(first, second);

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

    let token = server(&file).logins().token();
    assert!(
        !token.contains("aaaaaaaaaaaa"),
        "a world-readable token must not be reused: {token}"
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

    let token = server(&file).logins().token();
    assert_eq!(token.len(), 48);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    let _ = std::fs::remove_file(&file);
}

#[test]
fn rotating_replaces_the_token_in_the_file_and_in_the_server() {
    // For `galdeck ui --new-token`: the old one must stop working now and
    // must not come back at the next start.
    let file = scratch("rotate");
    let running = server(&file);
    let logins = running.logins();
    let before = logins.token();
    logins.rotate_token().unwrap();
    let after = logins.token();
    assert_ne!(before, after);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), after);
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "token file mode was {mode:o}");
    drop(running);
    assert_eq!(server(&file).logins().token(), after);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn the_token_lives_under_its_new_name_beside_the_socket() {
    // A new name, so the first start after the upgrade mints a new token:
    // the old one was printed to the journal at every start.
    let socket = std::path::Path::new("/run/user/1000/galdeck.sock");
    assert_eq!(
        token_path_for(socket),
        std::path::Path::new("/run/user/1000/galdeck-ui.token")
    );
}

#[test]
fn the_old_token_file_is_removed_at_start() {
    let dir = std::env::temp_dir().join(format!("galdeck-token-{}-legacy", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("galdeck-ui-token");
    std::fs::write(&old, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let socket = dir.join("galdeck.sock");

    remove_legacy_token(&socket);
    assert!(!old.exists());
    // And quietly nothing when there is nothing to remove.
    remove_legacy_token(&socket);

    let _ = std::fs::remove_dir_all(&dir);
}
