//! The control surface a configuration UI will sit on: read the files, try an
//! edit, apply it. The interesting part is what it refuses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::time::Duration;

use galdeck_core::{wake_channel, Clock, DeadlineCell};
use galdeck_daemon::clock::SystemClock;
use galdeck_daemon::engine::{ControlMsg, ControlSender, Engine};
use galdeck_daemon::io::IoThread;
use galdeck_device::FakeDeck;
use galdeck_ipc::{Patch, Request, Response, Severity, Value};
use galdeck_model::Workspace;

struct Harness {
    control: ControlSender,
    dir: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start() -> Self {
        // A private copy of the shipped example, so a test that saves does not
        // edit the repository.
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/v2");
        let dir = std::env::temp_dir().join(format!(
            "galdeck-config-api-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&source, &dir);

        let (workspace, diagnostics) = Workspace::load(&dir);
        let workspace = workspace.expect("the example must load");
        assert!(
            !diagnostics.iter().any(|d| d.severity == Severity::Error),
            "{diagnostics:#?}"
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let deadline = Arc::new(DeadlineCell::new());
        let (waker, wake_rx) = wake_channel();
        let (paint_tx, paint_rx) = sync_channel(64);
        let (device_tx, device_rx) = sync_channel(256);
        let (control_tx, control_rx) = channel();
        let control = ControlSender::new(control_tx, waker.clone());

        let (deck, _handle) = FakeDeck::new();
        let io = IoThread::with_deck(
            Box::new(deck),
            paint_rx,
            device_tx,
            Arc::clone(&deadline),
            Arc::clone(&clock),
            waker,
            Arc::clone(&shutdown),
        );
        let io_thread = std::thread::spawn(move || io.run());

        let mut engine = Engine::new(
            dir.clone(),
            workspace,
            control_rx,
            device_rx,
            paint_tx,
            deadline,
            clock,
            wake_rx,
            Arc::clone(&shutdown),
        )
        .expect("engine builds");
        let core = std::thread::spawn(move || engine.run());

        Self {
            control,
            dir,
            shutdown,
            threads: vec![io_thread, core],
        }
    }

    fn request(&self, request: Request) -> Response {
        let (reply, rx) = channel();
        self.control
            .send(ControlMsg { request, reply })
            .expect("engine listening");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("engine answers")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

#[test]
fn get_config_returns_every_file_as_written() {
    let harness = Harness::start();
    let Response::Config(snapshot) = harness.request(Request::GetConfig) else {
        panic!("expected a config snapshot");
    };
    let names: Vec<&str> = snapshot.files.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"galdeck.toml"));
    assert!(names.contains(&"profiles/work.toml"));
    assert!(names.contains(&"themes/nord.toml"));

    // The text is what is on disk, comments included.
    let global = snapshot
        .files
        .iter()
        .find(|f| f.name == "galdeck.toml")
        .unwrap();
    assert!(global
        .text
        .contains("# galdeck — the small file at the root."));
    assert!(snapshot
        .diagnostics
        .iter()
        .all(|d| d.severity != Severity::Error));
}

#[test]
fn a_clean_edit_validates_with_nothing_to_say() {
    let harness = Harness::start();
    let response = harness.request(Request::ValidateConfig {
        file: "galdeck.toml".into(),
        patches: vec![Patch::Set {
            path: "brightness".into(),
            value: Value::Integer(80),
        }],
        generation: None,
    });
    let Response::Diagnostics { diagnostics } = response else {
        panic!("expected diagnostics, got {response:?}");
    };
    assert!(
        diagnostics.iter().all(|d| d.severity != Severity::Error),
        "{diagnostics:#?}"
    );
}

#[test]
fn validation_sees_the_whole_workspace_not_just_the_edited_file() {
    // Deleting a palette entry is fine as TOML and fine for the theme file on
    // its own. It is not fine for the keys that reference it, and that is the
    // thing a user needs told before saving.
    let harness = Harness::start();
    let response = harness.request(Request::ValidateConfig {
        file: "themes/nord.toml".into(),
        patches: vec![Patch::Remove {
            path: "palette.storm".into(),
        }],
        generation: None,
    });
    let Response::Diagnostics { diagnostics } = response else {
        panic!("expected diagnostics, got {response:?}");
    };
    let unknown = diagnostics
        .iter()
        .find(|d| d.code == "E0117")
        .expect("the dangling @storm reference should be reported");
    assert!(unknown.message.contains("storm"));
}

#[test]
fn validating_changes_nothing_on_disk() {
    let harness = Harness::start();
    let before = std::fs::read_to_string(harness.dir.join("galdeck.toml")).unwrap();
    harness.request(Request::ValidateConfig {
        file: "galdeck.toml".into(),
        patches: vec![Patch::Set {
            path: "brightness".into(),
            value: Value::Integer(11),
        }],
        generation: None,
    });
    let after = std::fs::read_to_string(harness.dir.join("galdeck.toml")).unwrap();
    assert_eq!(before, after);
}

#[test]
fn applying_an_edit_saves_it_and_keeps_the_comments() {
    let harness = Harness::start();
    let response = harness.request(Request::ApplyConfig {
        file: "galdeck.toml".into(),
        patches: vec![Patch::Set {
            path: "brightness".into(),
            value: Value::Integer(35),
        }],
        generation: None,
    });
    assert!(matches!(response, Response::Ok), "got {response:?}");

    let written = std::fs::read_to_string(harness.dir.join("galdeck.toml")).unwrap();
    assert!(written.contains("brightness = 35"));
    assert!(written.contains("# Panel brightness, 0-100."));

    // And the running daemon picked it up.
    let Response::Status(status) = harness.request(Request::Status) else {
        panic!("expected status");
    };
    assert_eq!(status.brightness, 35);
}

#[test]
fn an_edit_that_would_break_the_config_is_refused_rather_than_written() {
    let harness = Harness::start();
    let before = std::fs::read_to_string(harness.dir.join("galdeck.toml")).unwrap();

    let response = harness.request(Request::ApplyConfig {
        file: "galdeck.toml".into(),
        patches: vec![Patch::Set {
            path: "brightness".into(),
            value: Value::Integer(240),
        }],
        generation: None,
    });
    let Response::Diagnostics { diagnostics } = response else {
        panic!("expected a refusal, got {response:?}");
    };
    assert!(diagnostics.iter().any(|d| d.code == "E0101"));

    let after = std::fs::read_to_string(harness.dir.join("galdeck.toml")).unwrap();
    assert_eq!(before, after, "nothing should have been written");
}

#[test]
fn an_edit_to_a_file_that_does_not_exist_is_an_error() {
    let harness = Harness::start();
    let response = harness.request(Request::ApplyConfig {
        file: "themes/nope.toml".into(),
        patches: vec![],
        generation: None,
    });
    let Response::Error { message } = response else {
        panic!("expected an error, got {response:?}");
    };
    assert!(message.contains("nope.toml"));
}

#[test]
fn status_reports_the_profile_as_well_as_the_page() {
    let harness = Harness::start();
    let Response::Status(status) = harness.request(Request::Status) else {
        panic!("expected status");
    };
    assert_eq!(status.profile, "work");
    assert!(status.profiles.contains(&"play".to_string()));
    assert_eq!(status.page, "main");
}
