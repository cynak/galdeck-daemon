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

#[test]
fn everything_the_page_loads_by_itself_is_served_without_a_token() {
    // The browser fetches a `<script src>` and a `<link href>` with no way to
    // attach a header or a query string, so anything referenced that way has
    // to be public or the page loads with no script and no styling -- which is
    // exactly what happened, and looked like the daemon being unreachable
    // rather than like a 401.
    let public = ["/", "/index.html", "/app.js", "/app.css"];
    let source = include_str!("../src/http.rs");

    let mut referenced = Vec::new();
    for (index, _) in INDEX
        .match_indices("src=\"/")
        .chain(INDEX.match_indices("href=\"/"))
    {
        let start = INDEX[index..].find('"').unwrap() + index + 1;
        let path: String = INDEX[start..].chars().take_while(|c| *c != '"').collect();
        referenced.push(path);
    }
    assert!(
        !referenced.is_empty(),
        "the page should reference its assets"
    );

    for path in &referenced {
        assert!(
            public.contains(&path.as_str()),
            "index.html loads {path} with no token, so it must be public"
        );
        // And the allowlist in the router has to actually contain it.
        assert!(
            source.contains(&format!("\"{path}\"")),
            "{path} is not in the router's public list"
        );
    }
}

#[test]
fn the_ui_clears_a_token_the_daemon_rejected() {
    // Otherwise a stored token that will never work again sits there
    // outliving every reload, and the only way out is to know to clear site
    // data — which is not something anyone should have to work out.
    assert!(APP_JS.contains("sessionStorage.removeItem(\"galdeck-token\")"));
    assert!(APP_JS.contains("class StaleToken"));
}

#[test]
fn a_rejected_token_says_what_to_do_about_it() {
    // "401: a valid token is required" is true and useless.
    assert!(APP_JS.contains("galdeck ui"));
    assert!(APP_JS.contains("restarted"));
}
