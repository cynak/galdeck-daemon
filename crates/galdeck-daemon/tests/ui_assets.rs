//! The UI is served from bytes compiled into the binary, so nothing at build
//! time checks that its references line up. These do.

use galdeck_http::CONTENT_SECURITY_POLICY;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const KEYBOARD_JS: &str = include_str!("../ui/keyboard.js");
/// Every script the page runs: app.js, and the modules it imports. Each is
/// held to what app.js is.
const SCRIPTS: [(&str, &str); 2] = [("app.js", APP_JS), ("keyboard.js", KEYBOARD_JS)];
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

/// The values of a `const NAME = [["value", "label"], ...];` list in app.js.
fn offered(list: &str) -> Vec<String> {
    let start = APP_JS
        .find(&format!("const {list} = ["))
        .unwrap_or_else(|| panic!("app.js has no {list}"));
    let body = &APP_JS[start..start + APP_JS[start..].find("];").expect("the list ends")];
    body.match_indices("[\"")
        .map(|(at, _)| body[at + 2..].chars().take_while(|c| *c != '"').collect())
        .collect()
}

#[test]
fn every_effect_the_theme_editor_offers_is_one_the_model_takes() {
    // Offered by name and parsed by the model, so a misspelt one is a save
    // the daemon refuses with an error nobody can do anything about.
    let effects = offered("LIGHTING_EFFECTS");
    assert!(effects.len() >= 5, "{effects:?}");
    for effect in effects {
        let parsed: Result<galdeck_model::Lighting, _> =
            toml::from_str(&format!("effect = \"{effect}\""));
        assert!(parsed.is_ok(), "the UI offers lighting effect {effect:?}");
    }
    let motions = offered("MOTIONS");
    assert!(motions.len() >= 5, "{motions:?}");
    for motion in motions {
        let parsed: Result<galdeck_model::Backdrop, _> =
            toml::from_str(&format!("animation = \"{motion}\""));
        assert!(
            parsed.is_ok(),
            "the UI offers background animation {motion:?}"
        );
    }
    // Widget looks: each list names one field of `[widgets]`.
    for (list, field) in [
        ("LOOK_VIEWS", "view"),
        ("GRAPH_STYLES", "graph"),
        ("BAR_STYLES", "bar"),
    ] {
        let names = offered(list);
        assert!(names.len() >= 3, "{list}: {names:?}");
        for name in names {
            let parsed: Result<galdeck_model::WidgetLooks, _> =
                toml::from_str(&format!("{field} = \"{name}\""));
            assert!(parsed.is_ok(), "the UI offers {field} {name:?}");
        }
    }
    // Motion: each list names the kinds of one setting of `[motion]`.
    for (list, field) in [
        ("PRESS_KINDS", "press"),
        ("ALARM_MOTIONS", "alarm"),
        ("RING_MOTIONS", "rings"),
    ] {
        let names = offered(list);
        assert!(names.len() >= 2, "{list}: {names:?}");
        for name in names {
            let parsed: Result<galdeck_model::MotionStyle, _> =
                toml::from_str(&format!("{field} = {{ kind = \"{name}\" }}"));
            assert!(parsed.is_ok(), "the UI offers {field} {name:?}");
        }
    }
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
        "/api/login",
        "/api/preview",
        "/api/preview/key/",
        "/api/preview/lcd.jpg",
        "/api/logos",
    ];
    for (name, script) in SCRIPTS {
        for (index, _) in script.match_indices("/api/") {
            let tail: String = script[index..]
                .chars()
                .take_while(|c| !"\"'`?$ \n".contains(*c))
                .collect();
            assert!(
                known.iter().any(|k| tail.starts_with(k)),
                "{name} fetches {tail:?}, which the router does not answer"
            );
        }
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
        "get_themes",
        "create_theme",
        "preview_theme",
        "save_asset",
        "set_key_state",
        "icon_names",
        "render_key_state",
        "which",
        "fetch_asset",
        "catalog",
        "run_action",
        "geocode",
        "audio_targets",
        "keyboard_layout",
        "keyboard_frame",
        "preview_lighting",
    ];
    for (script_name, script) in SCRIPTS {
        for (index, _) in script.match_indices("cmd: \"") {
            let name: String = script[index + 6..]
                .chars()
                .take_while(|c| *c != '"')
                .collect();
            assert!(
                known.contains(&name.as_str()),
                "{script_name} sends unknown cmd {name:?}"
            );
        }
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
    for (name, script) in SCRIPTS {
        for (index, _) in script.match_indices("el(\"") {
            let id: String = script[index + 4..]
                .chars()
                .take_while(|c| *c != '"')
                .collect();
            if id.starts_with("f-") {
                assert!(
                    script.contains(&format!("id=\"{id}\"")),
                    "{name} looks up #{id} but never builds it either"
                );
                continue;
            }
            assert!(
                INDEX.contains(&format!("id=\"{id}\"")),
                "{name} looks up #{id}, which index.html does not define"
            );
        }
    }
}

#[test]
fn the_assets_are_plain_ascii_text_with_unix_line_endings() {
    // A stray carriage return in a served file breaks nothing visibly and is
    // a nuisance to track down later.
    for (name, text) in [
        ("index.html", INDEX),
        ("app.js", APP_JS),
        ("keyboard.js", KEYBOARD_JS),
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
fn the_sign_in_code_leaves_the_address_bar_before_it_is_used() {
    // It arrives in the URL because that is the only way to hand anything
    // to a browser, but leaving it there puts it in history and in every
    // referrer. Taken out first, so a reload never retries a spent code.
    assert!(APP_JS.contains("searchParams.delete(\"code\")"));
    let stripped = APP_JS
        .find("history.replaceState")
        .expect("the page rewrites its address");
    let exchanged = APP_JS
        .find("fetch(\"/api/login\"")
        .expect("the page signs in");
    assert!(
        stripped < exchanged,
        "the code must leave the address before it is posted"
    );
    assert!(INDEX.contains(r#"name="referrer" content="no-referrer""#));
}

#[test]
fn a_token_in_the_address_is_no_longer_taken() {
    // Addresses from before sign-in links carried the token itself, and
    // those are in browser histories. The page only clears them away.
    assert!(!APP_JS.contains("searchParams.get(\"token\")"));
    assert!(APP_JS.contains("searchParams.delete(\"token\")"));
}

#[test]
fn everything_the_page_loads_by_itself_is_served_without_a_token() {
    // The browser fetches a `<script src>` and a `<link href>` with no way to
    // attach a header or a query string, so anything referenced that way has
    // to be public or the page loads with no script and no styling -- which is
    // exactly what happened, and looked like the daemon being unreachable
    // rather than like a 401.
    let public = ["/", "/index.html", "/app.js", "/keyboard.js", "/app.css"];
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
    // A module app.js imports is fetched the same way, with no token.
    for (index, _) in APP_JS.match_indices("import \"./") {
        let start = index + "import \"./".len();
        let file: String = APP_JS[start..].chars().take_while(|c| *c != '"').collect();
        referenced.push(format!("/{file}"));
    }

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
    assert!(APP_JS.contains("Run <code>galdeck ui</code> to open it."));
    assert!(APP_JS.contains("This link has expired: run <code>galdeck ui</code> again."));
    // A link someone else used first is worth a different answer.
    assert!(APP_JS.contains(
        "This link was already used. If that wasn't you, run <code>galdeck ui --new-token</code>."
    ));
}

/// Whether `replacement`, a `replace` or `replaceAll` call from its second
/// argument on, goes in as it is: a function, whose answer does, or a
/// literal alone with no `$` in it. Any other string is read for `$$`, `$&`,
/// `` $` `` and `$'`, and a template may put one there.
fn goes_in_as_it_is(replacement: &str) -> bool {
    let identifier = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let mut chars = replacement.chars();
    match chars.next() {
        Some(quote @ ('"' | '\'')) => {
            let body = chars.as_str();
            let mut escaped = false;
            for (at, c) in body.char_indices() {
                if c == '$' {
                    return false;
                }
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    // The literal alone: `"x" + name` is a string built
                    // with whatever `name` holds.
                    return body[at + 1..].trim_start().starts_with(')');
                }
            }
            false
        }
        // An arrow function's parameters, then its arrow.
        Some('(') => {
            let rest = chars.as_str();
            let mut depth = 1;
            for (at, c) in rest.char_indices() {
                depth += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
                if depth == 0 {
                    return rest[at + 1..].trim_start().starts_with("=>");
                }
            }
            false
        }
        _ => {
            let name = replacement.trim_start_matches(identifier);
            replacement.starts_with("function") || name.trim_start().starts_with("=>")
        }
    }
}

#[test]
fn every_replacement_in_the_script_goes_in_as_it_is() {
    // A VPN toggle put the connection name someone typed, shell-quoted, in
    // as a string, and "Corp $$ VPN" came out "Corp $ VPN": a name nobody
    // has, so the key read and toggled nothing. Read one line at a time, so
    // a call whose first argument runs past its line says so.
    assert!(!goes_in_as_it_is("shellQuote(name));"));
    assert!(!goes_in_as_it_is("\"$&!\");"));
    assert!(!goes_in_as_it_is("`'${name}'`);"));
    assert!(!goes_in_as_it_is("(prefix + name));"));
    assert!(!goes_in_as_it_is("\"'\" + name);"));
    assert!(goes_in_as_it_is("\"'\\\\''\")}'`;"));
    assert!(goes_in_as_it_is("() => quoted);"));
    assert!(goes_in_as_it_is("(c) =>"));
    assert!(goes_in_as_it_is("c => c.toUpperCase());"));

    let mut calls = 0;
    for (name, script) in SCRIPTS {
        for opener in [".replace(", ".replaceAll("] {
            for (at, _) in script.match_indices(opener) {
                calls += 1;
                let line = script[at..].lines().next().unwrap_or_default();
                let replacement = line.split_once(", ").map(|(_, rest)| rest.trim_start());
                assert!(
                    replacement.is_some_and(goes_in_as_it_is),
                    "{name} line {}: {line} hands replace a string it reads `$` in; pass `() => text`",
                    line_of(script, at)
                );
            }
        }
    }
    assert!(calls > 0, "app.js should replace something");
}

#[test]
fn a_finished_timer_is_named_only_on_the_first_page_with_its_id() {
    // Two pages may share an id, and a timer is kept by it: it ran on the
    // first. Shown the second, the toast named the key there, some other
    // timer, as the one that was done.
    let at = APP_JS
        .find("addEventListener(\"timer_done\"")
        .expect("app.js listens for finished timers");
    let handler = &APP_JS[at..];
    let handler = &handler[..handler.find("});").expect("the handler ends")];
    assert!(
        handler.contains("state.layout.pages.indexOf(page) === state.layout.page_index"),
        "{handler}"
    );
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

    for (name, text) in [
        ("index.html", INDEX),
        ("app.js", APP_JS),
        ("keyboard.js", KEYBOARD_JS),
    ] {
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
    for (name, script) in SCRIPTS {
        for (at, _) in script.match_indices("style=") {
            let before = script[..at].chars().next_back().unwrap_or(' ');
            let markup = before.is_whitespace() || "/\"'".contains(before);
            if markup && !script[at + "style=".len()..].starts_with('=') {
                panic!(
                    "{name} line {} builds a style attribute, which the policy drops",
                    line_of(script, at)
                );
            }
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
    for (name, script) in SCRIPTS {
        assert!(
            !script.to_ascii_lowercase().contains("<script"),
            "{name} builds a script element"
        );
    }
    // The only other script is a module app.js imports, which is fetched
    // from here like app.js itself.
    assert!(APP_JS.contains("import \"./keyboard.js\";"));
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
    for (name, text) in [
        ("index.html", INDEX),
        ("app.js", APP_JS),
        ("keyboard.js", KEYBOARD_JS),
    ] {
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
    for (name, script) in SCRIPTS {
        for opener in ["fetch(", "new EventSource("] {
            for (at, _) in script.match_indices(opener) {
                if script[..at].ends_with(identifier) {
                    continue; // `refetch(` and the like are someone's own function
                }
                calls += 1;
                let mut argument = script[at + opener.len()..].trim_start().chars();
                let quoted = matches!(argument.next(), Some('"' | '`'));
                let rooted = argument.next() == Some('/');
                let local = !matches!(argument.next(), Some('/' | '\\'));
                assert!(
                    quoted && rooted && local,
                    "{name} line {}: {opener}...) is not a same-origin path",
                    line_of(script, at)
                );
            }
        }
        for other in ["XMLHttpRequest", "WebSocket", "sendBeacon"] {
            assert!(
                !script.contains(other),
                "{name} uses {other}, which this test does not check"
            );
        }
    }
    assert!(calls > 0, "the scripts should call the daemon");
}

#[test]
fn the_script_makes_no_object_urls() {
    // The policy allows no `blob:` source anywhere, so an object URL would
    // load nothing. Nothing needs one: previews come back as data: URLs.
    for (name, script) in SCRIPTS {
        assert!(!script.contains("createObjectURL"), "{name} makes one");
    }
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
