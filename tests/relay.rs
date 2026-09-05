// The ssdp relay reflected end to end over loopback: a tunnelled scream
// answers a search from a searcher, the reply comes back. The M-SEARCH is
// sent unicast to the relay's group socket (which is bound to accept it) so
// the test needs no working multicast route, group membership is one line of
// std and covered by the direct MulticastIo path. A non-standard port keeps
// it clear of a real 1900

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use scream::dlna::{run_relay, SsdpIo, TunnelIo};

#[test]
fn relay_reflects_msearch_to_a_tunnelled_scream_and_the_answer_back() {
    let lan_port = 19000u16;
    let listen = "127.0.0.1:19001";

    std::thread::spawn(move || {
        let _ = run_relay(listen, Ipv4Addr::LOCALHOST, lan_port);
    });
    std::thread::sleep(Duration::from_millis(200));

    // A scream instance reachable only over the tunnel, keepalive so the
    // relay learns where to reach it
    let scream = TunnelIo::connect(listen, Arc::new(AtomicBool::new(false)))
        .expect("connect tunnel");
    scream
        .send_to(&[], SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // A searcher M-SEARCHes and waits for the answer
    let searcher = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    searcher
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    searcher
        .send_to(
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: ssdp:all\r\nMX: 1\r\n\r\n",
            SocketAddr::from((Ipv4Addr::LOCALHOST, lan_port)),
        )
        .expect("send m-search");

    // scream receives the forwarded search and answers it
    let mut buf = [0u8; 2048];
    let (n, peer) = scream.recv_from(&mut buf).expect("scream sees the m-search");
    assert!(buf[..n].starts_with(b"M-SEARCH"));
    scream
        .send_to(b"HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\n\r\n", peer)
        .unwrap();

    // the searcher gets the 200 OK back on the wire
    let (n, _) = searcher.recv_from(&mut buf).expect("searcher gets a reply");
    assert!(buf[..n].starts_with(b"HTTP/1.1 200 OK"));
}
