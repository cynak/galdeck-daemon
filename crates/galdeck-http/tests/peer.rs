//! Telling which local account is on the other end of a connection.
//!
//! Every account on the machine can connect to 127.0.0.1, so the UI server
//! looks the client's socket up in `/proc/net/tcp` and serves only its own
//! user. These pin down how that table is read.

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

use galdeck_http::peer_uid;

/// An address as the kernel prints it: the 32-bit word in memory order, then
/// the port as a number.
fn hex(address: SocketAddr) -> String {
    let SocketAddr::V4(v4) = address else {
        panic!("the table is IPv4 only");
    };
    format!(
        "{:08X}:{:04X}",
        u32::from_ne_bytes(v4.ip().octets()),
        v4.port()
    )
}

fn at(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";

/// One row of the table, with the columns the kernel fills in around the
/// ones that matter here.
fn row(slot: usize, local: SocketAddr, remote: SocketAddr, state: &str, uid: u32) -> String {
    format!(
        "{slot:4}: {} {} {state} 00000000:00000000 00:00000000 00000000 {uid:5}        0 {} 1 0000000000000000 20 4 30 10 -1",
        hex(local),
        hex(remote),
        4_000_000 + slot
    )
}

const SERVER: u16 = 8787;
const CLIENT: u16 = 51000;

/// The listener, the server's end of a connection, and the client's end,
/// each with its owner.
fn table(client_uid: u32) -> String {
    [
        HEADER.to_string(),
        row(
            0,
            at(SERVER),
            SocketAddr::from(([0, 0, 0, 0], 0)),
            "0A",
            1000,
        ),
        row(1, at(SERVER), at(CLIENT), "01", 1000),
        row(2, at(CLIENT), at(SERVER), "01", client_uid),
    ]
    .join("\n")
}

#[test]
fn the_client_row_gives_the_client_uid() {
    assert_eq!(peer_uid(&table(1000), at(CLIENT), at(SERVER)), Some(1000));
}

#[test]
fn another_account_is_reported_as_itself_not_as_the_server() {
    // The server's end of the same connection is ours whoever connected, so
    // reading the wrong row would let everyone in.
    assert_eq!(peer_uid(&table(1001), at(CLIENT), at(SERVER)), Some(1001));
}

#[test]
fn no_matching_row_is_no_answer() {
    assert_eq!(peer_uid(&table(1000), at(CLIENT + 1), at(SERVER)), None);
    assert_eq!(peer_uid(&table(1000), at(CLIENT), at(SERVER + 1)), None);
    assert_eq!(peer_uid(HEADER, at(CLIENT), at(SERVER)), None);
    assert_eq!(peer_uid("", at(CLIENT), at(SERVER)), None);
}

#[test]
fn a_row_that_does_not_parse_is_skipped_rather_than_trusted() {
    let text = [
        HEADER.to_string(),
        format!("   0: {} {} 01", hex(at(CLIENT)), hex(at(SERVER))),
        format!(
            "   1: {} {} 01 0:0 00:0 0 notanumber",
            hex(at(CLIENT)),
            hex(at(SERVER))
        ),
        "garbage".to_string(),
        "   2: 0100007F:ZZZZ 0100007F:2253 01 0:0 00:0 0 1000".to_string(),
    ]
    .join("\n");
    assert_eq!(peer_uid(&text, at(CLIENT), at(SERVER)), None);
}

#[test]
fn a_closed_connection_on_the_same_ports_is_passed_over() {
    // A connection that closed and was reopened between the same two ports
    // can leave its old row behind, owned by nobody (uid 0).
    let text = [
        HEADER.to_string(),
        row(0, at(CLIENT), at(SERVER), "06", 0),
        row(1, at(CLIENT), at(SERVER), "01", 1000),
    ]
    .join("\n");
    assert_eq!(peer_uid(&text, at(CLIENT), at(SERVER)), Some(1000));
    let closed = [HEADER.to_string(), row(0, at(CLIENT), at(SERVER), "06", 0)].join("\n");
    assert_eq!(peer_uid(&closed, at(CLIENT), at(SERVER)), None);
}

#[test]
fn addresses_other_than_loopback_are_matched_exactly() {
    let far = SocketAddr::from(([192, 168, 1, 20], CLIENT));
    let text = [HEADER.to_string(), row(0, far, at(SERVER), "01", 1002)].join("\n");
    assert_eq!(peer_uid(&text, far, at(SERVER)), Some(1002));
    assert_eq!(peer_uid(&text, at(CLIENT), at(SERVER)), None);
}

#[test]
fn ipv6_is_not_looked_up() {
    let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, CLIENT));
    assert_eq!(peer_uid(&table(1000), v6, at(SERVER)), None);
}

#[cfg(target_endian = "little")]
#[test]
fn reads_the_table_as_a_little_endian_kernel_prints_it() {
    // Copied from a real table: 127.0.0.1 comes out as 0100007F, and port
    // 8787 as 2253.
    let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2253 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1234567 1 0000000000000000 100 0 0 10 0
   1: 0100007F:C738 0100007F:2253 01 00000000:00000000 00:00000000 00000000  1000        0 1234568 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:2253 0100007F:C738 01 00000000:00000000 00:00000000 00000000  1000        0 1234569 1 0000000000000000 20 4 30 10 -1";
    assert_eq!(peer_uid(text, at(0xC738), at(8787)), Some(1000));
}

#[test]
fn a_real_connection_to_ourselves_is_ours() {
    // The whole path on this machine's own table: connect to a listener,
    // then find the client's end the way the server does.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();

    let mut text = String::new();
    std::fs::File::open("/proc/net/tcp")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let found = peer_uid(
        &text,
        accepted.peer_addr().unwrap(),
        accepted.local_addr().unwrap(),
    );
    // Safety: getuid cannot fail and touches no memory.
    assert_eq!(found, Some(unsafe { libc::getuid() }));
}
