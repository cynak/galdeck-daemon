//! The UI is served from bytes compiled into the binary, so nothing at build
//! time checks that its references line up. These do.

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");

#[test]
fn the_page_references_the_assets_that_are_served() {
    assert!(INDEX.contains("/app.js"));
    assert!(INDEX.contains("/app.css"));
    assert!(!APP_JS.is_empty());
    assert!(!APP_CSS.is_empty());
}

#[test]
fn every_api_path_the_ui_fetches_is_one_the_router_answers() {
    // A typo here is a 404 the user sees and nothing else notices.
    let known = [
        "/api/call",
        "/api/events",
        "/api/preview",
        "/api/preview/key/",
        "/api/preview/lcd.jpg",
    ];
    for (index, _) in APP_JS.match_indices("/api/") {
        let tail: String = APP_JS[index..]
            .chars()
            .take_while(|c| !"\"'`?$ \n".contains(*c))
            .collect();
        assert!(
            known.iter().any(|k| tail.starts_with(k)),
            "app.js fetches {tail:?}, which the router does not answer"
        );
    }
}

#[test]
fn every_command_the_ui_sends_is_one_the_protocol_has() {
    // The daemon's Request enum is internally tagged on `cmd`, so a wrong
    // name is a deserialize failure at runtime rather than a compile error.
    let known = [
        "ping",
        "status",
        "set_brightness",
        "switch_page",
        "switch_profile",
        "reload",
        "get_config",
        "get_layout",
        "validate_config",
        "apply_config",
    ];
    for (index, _) in APP_JS.match_indices("cmd: \"") {
        let name: String = APP_JS[index + 6..]
            .chars()
            .take_while(|c| *c != '"')
            .collect();
        assert!(
            known.contains(&name.as_str()),
            "app.js sends unknown cmd {name:?}"
        );
    }
}

#[test]
fn every_element_the_script_looks_up_exists_in_the_page() {
    // `el("typo")` returns null and the failure surfaces somewhere unrelated.
    // Ids built into the inspector's own markup are excluded: they are created
    // by the script itself.
    let built_by_script = [
        "f-label",
        "f-icon",
        "f-action",
        "f-exec",
        "f-page",
        "f-profile",
        "f-bg",
        "f-bg-clear",
        "f-press",
        "f-cw",
        "f-ccw",
        "f-ring",
        "save",
        "add",
        "remove",
    ];
    for (index, _) in APP_JS.match_indices("el(\"") {
        let id: String = APP_JS[index + 4..]
            .chars()
            .take_while(|c| *c != '"')
            .collect();
        if built_by_script.contains(&id.as_str()) {
            continue;
        }
        assert!(
            INDEX.contains(&format!("id=\"{id}\"")),
            "app.js looks up #{id}, which index.html does not define"
        );
    }
}

#[test]
fn the_assets_are_plain_ascii_text_with_unix_line_endings() {
    // A stray carriage return in a served file breaks nothing visibly and is
    // a nuisance to track down later.
    for (name, text) in [
        ("index.html", INDEX),
        ("app.js", APP_JS),
        ("app.css", APP_CSS),
    ] {
        assert!(!text.contains('\r'), "{name} has CRLF line endings");
        assert!(
            !text.starts_with('\u{feff}'),
            "{name} starts with a byte-order mark"
        );
    }
}

#[test]
fn the_token_is_taken_out_of_the_address_bar() {
    // It arrives in the URL because that is the only way to hand it to a
    // browser, but leaving it there puts it in history and in every referrer.
    assert!(APP_JS.contains("searchParams.delete(\"token\")"));
    assert!(APP_JS.contains("history.replaceState"));
    assert!(INDEX.contains(r#"name="referrer" content="no-referrer""#));
}
