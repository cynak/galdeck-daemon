use galdeck_http::sse;

#[test]
fn the_stream_head_says_what_it_is_and_asks_not_to_be_buffered() {
    let head = String::from_utf8(sse::open()).unwrap();
    assert!(head.contains("content-type: text/event-stream"));
    assert!(head.contains("cache-control: no-store"));
    assert!(head.contains("x-accel-buffering: no"));
}

#[test]
fn an_event_is_a_name_and_a_data_line() {
    let bytes = String::from_utf8(sse::event("status", "{\"ok\":true}")).unwrap();
    assert_eq!(bytes, "event: status\ndata: {\"ok\":true}\n\n");
}

#[test]
fn multiline_payloads_become_several_data_lines() {
    // A raw newline in the payload would otherwise truncate the event at the
    // first one, silently.
    let bytes = String::from_utf8(sse::event("log", "one\ntwo\nthree")).unwrap();
    assert_eq!(bytes, "event: log\ndata: one\ndata: two\ndata: three\n\n");
}

#[test]
fn an_unnamed_event_omits_the_event_line() {
    let bytes = String::from_utf8(sse::event("", "hello")).unwrap();
    assert_eq!(bytes, "data: hello\n\n");
}

#[test]
fn a_keepalive_is_a_comment_the_client_never_sees() {
    let bytes = String::from_utf8(sse::keepalive()).unwrap();
    assert!(bytes.starts_with(':'));
    assert!(bytes.ends_with("\n\n"));
}
