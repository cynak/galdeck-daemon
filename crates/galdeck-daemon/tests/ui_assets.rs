//! The UI is served from bytes compiled into the binary, so nothing at build
//! time checks that its references line up. These do.

use galdeck_http::CONTENT_SECURITY_POLICY;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");

/// The sources one directive of the page's Content-Security-Policy allows.
fn allowed(directive: &str) -> Vec<&'static str> {
    CONTENT_SECURITY_POLICY
        .split(';')
        .find_map(|d| d.trim().strip_prefix(directive)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("the policy has no {directive}"))
        .split_whitespace()
        .collect()
}

/// Which line of `text` the byte offset `at` falls on, for failure messages.
fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

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
        "release_device",
        "resume_device",
        "get_calibration",
        "set_calibration",
        "reload_calibration",
        "test_pattern",
        "zone_pattern",
        "widget_sources",
        "render_widget",
        "save_asset",
        "catalog",
        "run_action",
        "geocode",
        "audio_targets",
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
    //
    // Ids the inspector builds into its own markup are told apart by prefix
    // rather than by a list: a list of them rots every time a form gains a
    // field, and a rotting test gets deleted rather than fixed. Everything the
    // inspector creates is named `f-something`, which is a convention worth
    // holding to on its own -- so those are checked against the script, and
    // everything else against the page.
    for (index, _) in APP_JS.match_indices("el(\"") {
        let id: String = APP_JS[index + 4..]
            .chars()
            .take_while(|c| *c != '"')
            .collect();
        if id.starts_with("f-") {
            assert!(
                APP_JS.contains(&format!("id=\"{id}\"")),
                "app.js looks up #{id} but never builds it either"
            );
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

// The page is served under a Content-Security-Policy, and a browser enforces
// it quietly: whatever the policy refuses is left out, with a line in the
// console that no test here would ever see. So these read the sources for
// what the policy would refuse. app.js changes often, and this is what keeps
// an edit from blanking part of the page without anyone noticing.

#[test]
fn the_ui_builds_no_style_attributes_for_the_policy_to_drop() {
    // `style-src 'self'` drops every style attribute and inline <style>
    // element, so a knob ring or a theme swatch would just render blank.
    // Styles set from script go through the CSSOM -- `node.style.x = ...` or
    // `style.setProperty` -- which the policy allows, and a colour the markup
    // needs rides on `data-colour` for paintColours to apply.
    let style_src = allowed("style-src");
    assert!(style_src.contains(&"'self'"), "app.css would not load");
    assert!(!style_src.contains(&"'unsafe-inline'"));

    for (name, text) in [("index.html", INDEX), ("app.js", APP_JS)] {
        for needle in ["style=\"", "style='", "<style", "setAttribute(\"style\""] {
            if let Some(at) = text.find(needle) {
                panic!(
                    "{name} line {} builds {needle}, which the policy drops",
                    line_of(text, at)
                );
            }
        }
    }
    // The page is all markup, so any spelling of it is an attribute.
    if let Some(at) = INDEX.find("style=") {
        panic!(
            "index.html line {} sets a style attribute",
            line_of(INDEX, at)
        );
    }
    // In the script, an unquoted `style=${vars}` in a template is an
    // attribute all the same. It sits where markup puts one, after whitespace
    // or the quote closing the attribute before it; script spaces its `=`.
    for (at, _) in APP_JS.match_indices("style=") {
        let before = APP_JS[..at].chars().next_back().unwrap_or(' ');
        let markup = before.is_whitespace() || "/\"'".contains(before);
        if markup && !APP_JS[at + "style=".len()..].starts_with('=') {
            panic!(
                "app.js line {} builds a style attribute, which the policy drops",
                line_of(APP_JS, at)
            );
        }
    }
}

#[test]
fn the_page_runs_one_script_and_it_is_the_module_served_beside_it() {
    // `script-src 'self'` runs a script only when it is fetched from here, so
    // anything written into the page itself is dead.
    assert!(allowed("script-src").contains(&"'self'"));
    assert_eq!(
        INDEX.to_ascii_lowercase().matches("<script").count(),
        1,
        "index.html should load app.js and nothing else"
    );
    assert!(INDEX.contains(r#"<script type="module" src="/app.js"></script>"#));
    assert!(
        !APP_JS.to_ascii_lowercase().contains("<script"),
        "app.js builds a script element"
    );
}

/// Every `on...=` sitting where markup puts an attribute: after whitespace, a
/// `/`, or the quote closing the attribute before it. A property assignment
/// such as `events.onerror = ...` follows a dot and is script, not markup.
fn inline_handlers(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found = Vec::new();
    for (at, _) in lower.match_indices("on") {
        let before = if at == 0 { b' ' } else { bytes[at - 1] };
        if !(before.is_ascii_whitespace() || b"/\"'".contains(&before)) {
            continue;
        }
        let name = bytes[at + 2..]
            .iter()
            .take_while(|b| b.is_ascii_alphabetic())
            .count();
        let rest = &bytes[at + 2 + name..];
        if name > 0 && rest.first() == Some(&b'=') && rest.get(1) != Some(&b'=') {
            let end = at + 2 + name;
            found.push(format!("line {}: {}", line_of(text, at), &text[at..end]));
        }
    }
    found
}

#[test]
fn no_markup_carries_an_inline_event_handler() {
    // An `onclick="..."` attribute or a `javascript:` URL is inline script,
    // which the policy refuses to run; handlers are attached with
    // addEventListener instead.
    for (name, text) in [("index.html", INDEX), ("app.js", APP_JS)] {
        let handlers = inline_handlers(text);
        assert!(
            handlers.is_empty(),
            "{name} has inline handlers: {handlers:?}"
        );
        assert!(
            !text.contains("setAttribute(\"on"),
            "{name} sets a handler attribute"
        );
        assert!(
            !text.to_ascii_lowercase().contains("javascript:"),
            "{name} has a javascript: URL"
        );
    }
}

#[test]
fn the_handler_check_tells_markup_from_script() {
    // The check above passing proves little unless it can fail.
    assert_eq!(inline_handlers(r#"<b onclick="go()">"#).len(), 1);
    assert_eq!(inline_handlers(r#"<b class="x"onload=go()>"#).len(), 1);
    assert_eq!(inline_handlers("<svg/onload=go()>").len(), 1);
    assert!(inline_handlers("events.onerror = () => {};").is_empty());
    assert!(inline_handlers("if (x.on===y) once = 1;").is_empty());
}

#[test]
fn the_script_talks_only_to_the_daemon_that_served_it() {
    // `connect-src 'self'` refuses any other address, and the failure shows
    // only in a browser console. A literal path is required rather than any
    // argument allowed, because a variable can hold anything; and `//host`
    // and `/\host` start with a slash but name another origin.
    assert!(allowed("connect-src").contains(&"'self'"));
    let identifier = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let mut calls = 0;
    for opener in ["fetch(", "new EventSource("] {
        for (at, _) in APP_JS.match_indices(opener) {
            if APP_JS[..at].ends_with(identifier) {
                continue; // `refetch(` and the like are someone's own function
            }
            calls += 1;
            let mut argument = APP_JS[at + opener.len()..].trim_start().chars();
            let quoted = matches!(argument.next(), Some('"' | '`'));
            let rooted = argument.next() == Some('/');
            let local = !matches!(argument.next(), Some('/' | '\\'));
            assert!(
                quoted && rooted && local,
                "app.js line {}: {opener}...) is not a same-origin path",
                line_of(APP_JS, at)
            );
        }
    }
    assert!(calls > 0, "app.js should call the daemon");
    for other in ["XMLHttpRequest", "WebSocket", "sendBeacon"] {
        assert!(
            !APP_JS.contains(other),
            "app.js uses {other}, which this test does not check"
        );
    }
}

#[test]
fn the_script_makes_no_object_urls() {
    // The policy allows no `blob:` source anywhere, so an object URL would
    // load nothing. Nothing needs one: previews come back as data: URLs.
    assert!(!APP_JS.contains("createObjectURL"));
}

#[test]
fn the_gallery_previews_are_images_the_policy_allows() {
    // render_widget answers with `data:image/jpeg` URLs that go straight into
    // an <img>, so without `data:` in img-src every gallery preview is a
    // broken image. The empty favicon is a data: URL too, and the key and
    // screen previews are fetched from here.
    let img_src = allowed("img-src");
    assert!(img_src.contains(&"'self'"));
    if APP_JS.contains("render_widget") || INDEX.contains("\"data:") {
        assert!(
            img_src.contains(&"data:"),
            "the gallery shows data: previews, which img-src refuses"
        );
    }
}
