// DLNA MediaServer announcement and control, so a client like the PS4 media
// player autodiscovers the stream. Three parts, all speaking over what main.rs
// already runs: ssdp on udp 1900 for discovery, device and service
// descriptions as xml over http, and a ContentDirectory answering SOAP Browse
// with one live item pointing at the matroska stream.
//
// ssdp is link-local multicast, which does not cross a NAT the way the http
// and rtsp ports do with a plain port publish. When multicast cannot leave
// where scream runs (a podman container, a bridged VM), point it at a relay
// with --ssdp-relay: every ssdp datagram, in and out, is then framed over
// one unicast udp connection to a `scream --ssdp-relay-server` running on
// the real LAN, which reflects it onto the multicast group. See SsdpIo
// below, MulticastIo is the unchanged direct path
//
// © 2021 Björn Busse (see also: LICENSE)

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

const SSDP_ADDRESS: &str = "239.255.255.250:1900";
const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const CACHE_MAX_AGE: u32 = 1800;
const SERVER_ID: &str = "Linux UPnP/1.0 scream/0.1";
const STREAM_PATH: &str = "/stream.mkv";
// mkv because that is what the shipped gst plugins can mux around x264
// the strict dlna live profile would be mpeg-ts, which needs tsmux from
// gst-plugins-bad
const PROTOCOL_INFO: &str = "http-get:*:video/x-matroska:\
    DLNA.ORG_OP=00;DLNA.ORG_CI=0;\
    DLNA.ORG_FLAGS=01700000000000000000000000000000";

// Base url a client should reach this server at, e.g. http://192.168.1.10:7002.
// Set from --advertise-url when scream cannot tell its own reachable address
// (behind a NAT, in a container): it then replaces the autodetected host in
// the ssdp LOCATION and in the DIDL <res> stream url
static ADVERTISE_URL: OnceLock<Option<String>> = OnceLock::new();

pub fn set_advertise_url(url: Option<String>) {
    let _ = ADVERTISE_URL.set(url.map(|u| u.trim_end_matches('/').to_string()));
}

fn advertise_url() -> Option<&'static str> {
    ADVERTISE_URL.get().and_then(|o| o.as_deref())
}

// The identity a client remembers the server by, so it has to survive a
// restart: the machine id when there is one, a hostname hash otherwise
pub fn device_uuid() -> String {
    if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
        let hex: String = id
            .trim()
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() >= 32 {
            return format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
            );
        }
    }

    let mut hasher = DefaultHasher::new();
    std::env::var("HOSTNAME").unwrap_or_else(|_| "scream".into()).hash(&mut hasher);
    let a = hasher.finish();
    "scream".hash(&mut hasher);
    let b = hasher.finish();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16,
        (b >> 48) as u16,
        b & 0xffff_ffff_ffff
    )
}

// (NT, USN suffix) pairs a MediaServer announces and answers searches for
fn notification_types(uuid: &str) -> Vec<(String, String)> {
    let device = "urn:schemas-upnp-org:device:MediaServer:1";
    let cds = "urn:schemas-upnp-org:service:ContentDirectory:1";
    let cms = "urn:schemas-upnp-org:service:ConnectionManager:1";

    vec![
        ("upnp:rootdevice".into(), format!("uuid:{uuid}::upnp:rootdevice")),
        (format!("uuid:{uuid}"), format!("uuid:{uuid}")),
        (device.into(), format!("uuid:{uuid}::{device}")),
        (cds.into(), format!("uuid:{uuid}::{cds}")),
        (cms.into(), format!("uuid:{uuid}::{cms}")),
    ]
}

// The address this host has toward a peer, which is the one to put into
// LOCATION urls: a connected udp socket answers it without sending anything
fn address_toward(peer: SocketAddr) -> Option<String> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(peer).ok()?;

    Some(probe.local_addr().ok()?.ip().to_string())
}

fn header_value<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    message.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

fn ssdp_group_addr() -> SocketAddr {
    SocketAddr::from((SSDP_GROUP, SSDP_PORT))
}

// A udp socket bound to the ssdp port with the address and port reuse every
// ssdp listener sets, so scream (or the relay) coexists with an mDNS/DLNA
// daemon already on 1900 instead of failing to start
fn bind_ssdp_port(port: u16) -> io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(sock.into())
}

// Base url to hand a client, either the operator-provided one or, toward a
// given peer, whatever local address routes there
fn client_base_url(http_port: u16, toward: SocketAddr) -> Option<String> {
    if let Some(base) = advertise_url() {
        return Some(base.to_string());
    }
    let ip = address_toward(toward)?;
    Some(format!("http://{ip}:{http_port}"))
}

fn location(http_port: u16, toward: SocketAddr) -> Option<String> {
    Some(format!("{}/dlna/device.xml", client_base_url(http_port, toward)?))
}

fn notify(io: &dyn SsdpIo, http_port: u16, uuid: &str, alive: bool) {
    let target = ssdp_group_addr();
    let Some(location) = location(http_port, target) else { return };

    for (nt, usn) in notification_types(uuid) {
        let message = if alive {
            format!(
                "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDRESS}\r\n\
                 CACHE-CONTROL: max-age={CACHE_MAX_AGE}\r\n\
                 LOCATION: {location}\r\nNT: {nt}\r\nNTS: ssdp:alive\r\n\
                 SERVER: {SERVER_ID}\r\nUSN: {usn}\r\n\r\n"
            )
        } else {
            format!(
                "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDRESS}\r\n\
                 NT: {nt}\r\nNTS: ssdp:byebye\r\nUSN: {usn}\r\n\r\n"
            )
        };
        let _ = io.send_to(message.as_bytes(), target);
    }
}

fn answer_search(io: &dyn SsdpIo, peer: SocketAddr, http_port: u16,
                 uuid: &str, search_target: &str) {
    let Some(location) = location(http_port, peer) else { return };

    for (nt, usn) in notification_types(uuid) {
        if search_target != "ssdp:all" && search_target != nt {
            continue;
        }
        let st = if search_target == "ssdp:all" { &nt } else { search_target };
        let response = format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={CACHE_MAX_AGE}\r\n\
             EXT:\r\nLOCATION: {location}\r\nSERVER: {SERVER_ID}\r\n\
             ST: {st}\r\nUSN: {usn}\r\n\r\n"
        );
        let _ = io.send_to(response.as_bytes(), peer);
    }
}

// The transport ssdp_loop speaks: send a datagram toward `dst` (the multicast
// group for a NOTIFY, a searcher for a 200 OK) and receive the next one with
// the peer it came from. Two implementations: straight multicast, or framed
// over a unicast connection to a relay
pub trait SsdpIo: Send {
    fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<()>;
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>;
}

// The direct path: bind 1900, join the group. What scream has always done
pub struct MulticastIo {
    sock: UdpSocket,
}

impl MulticastIo {
    pub fn bind() -> io::Result<Self> {
        let sock = bind_ssdp_port(SSDP_PORT)?;
        sock.join_multicast_v4(&SSDP_GROUP, &Ipv4Addr::UNSPECIFIED)?;
        sock.set_multicast_ttl_v4(4).ok();
        sock.set_read_timeout(Some(Duration::from_secs(1)))?;
        Ok(Self { sock })
    }
}

impl SsdpIo for MulticastIo {
    fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<()> {
        self.sock.send_to(buf, dst).map(|_| ())
    }
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.sock.recv_from(buf)
    }
}

// tunnel frame: 4-byte IPv4 + 2-byte port, big-endian, then the datagram.
// scream->relay the address is the destination, relay->scream it is the
// source peer. A zero-length payload is a keepalive that only refreshes the
// relay's idea of where to reach this scream (and holds a NAT mapping open)
fn encode_frame(addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 6);
    match addr {
        SocketAddr::V4(a) => {
            out.extend_from_slice(&a.ip().octets());
            out.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(_) => out.extend_from_slice(&[0u8; 6]),
    }
    out.extend_from_slice(payload);
    out
}

fn decode_frame(buf: &[u8]) -> Option<(SocketAddr, &[u8])> {
    if buf.len() < 6 {
        return None;
    }
    let ip = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
    let port = u16::from_be_bytes([buf[4], buf[5]]);
    Some((SocketAddr::from((ip, port)), &buf[6..]))
}

// The tunnelled path: one connected udp socket to the relay, every ssdp
// datagram framed with its far-side address
pub struct TunnelIo {
    sock: UdpSocket,
}

impl TunnelIo {
    pub fn connect(relay: &str) -> io::Result<Self> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        sock.connect(relay)?;
        sock.set_read_timeout(Some(Duration::from_secs(1)))?;

        // Announce ourselves and keep the relay's return path (and any NAT in
        // between) alive while the loop is quiet
        let keepalive = sock.try_clone()?;
        std::thread::spawn(move || {
            let ping = encode_frame(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), &[]);
            loop {
                if keepalive.send(&ping).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(15));
            }
        });

        Ok(Self { sock })
    }
}

impl SsdpIo for TunnelIo {
    fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<()> {
        self.sock.send(&encode_frame(dst, buf)).map(|_| ())
    }
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut frame = [0u8; 4096];
        let n = self.sock.recv(&mut frame)?;
        match decode_frame(&frame[..n]) {
            Some((src, payload)) if !payload.is_empty() => {
                let len = payload.len().min(buf.len());
                buf[..len].copy_from_slice(&payload[..len]);
                Ok((len, src))
            }
            // keepalive echo or junk: look like a timeout, the loop retries
            _ => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }
}

// How main.rs wants ssdp run
pub struct SsdpConfig {
    pub http_port: u16,
    pub advertise: Option<String>,
    pub relay: Option<String>,
}

fn build_io(cfg: &SsdpConfig) -> io::Result<Box<dyn SsdpIo>> {
    match &cfg.relay {
        Some(addr) => {
            println!("SSDP: tunnelling to relay {addr}");
            Ok(Box::new(TunnelIo::connect(addr)?))
        }
        None => Ok(Box::new(MulticastIo::bind()?)),
    }
}

pub fn spawn_ssdp(cfg: SsdpConfig, shutdown: Arc<AtomicBool>) {
    set_advertise_url(cfg.advertise.clone());
    std::thread::spawn(move || {
        let io = match build_io(&cfg) {
            Ok(io) => io,
            Err(e) => {
                eprintln!("ssdp: {e}");
                return;
            }
        };
        ssdp_loop(io.as_ref(), cfg.http_port, shutdown);
    });
}

fn ssdp_loop(io: &dyn SsdpIo, http_port: u16, shutdown: Arc<AtomicBool>) {
    let uuid = device_uuid();
    println!("SSDP: announcing MediaServer uuid:{uuid}");

    let notify_every = Duration::from_secs((CACHE_MAX_AGE / 3) as u64);
    let mut last_notify: Option<Instant> = None;
    let mut buffer = [0u8; 2048];

    while !shutdown.load(Ordering::Relaxed) {
        if last_notify.map_or(true, |t| t.elapsed() >= notify_every) {
            notify(io, http_port, &uuid, true);
            last_notify = Some(Instant::now());
        }

        let (len, peer) = match io.recv_from(&mut buffer) {
            Ok(received) => received,
            Err(_) => continue,
        };
        let Ok(text) = std::str::from_utf8(&buffer[..len]) else { continue };
        if !text.starts_with("M-SEARCH") {
            continue;
        }
        if let Some(st) = header_value(text, "st") {
            answer_search(io, peer, http_port, &uuid, st);
        }
    }

    notify(io, http_port, &uuid, false);
}

// The relay end of --ssdp-relay: run where the real LAN is (a linux host with
// --network host, the macOS host outside the podman VM). It joins the ssdp
// group, forwards every M-SEARCH it hears to each connected scream, and puts
// whatever they send back onto the wire: NOTIFY to the group, 200 OK unicast
// to the searcher. `iface` is the local address to join the group on, or
// 0.0.0.0 for the default. `lan_port` is 1900 outside tests
pub fn run_relay(listen: &str, iface: Ipv4Addr, lan_port: u16) -> io::Result<()> {
    let lan = bind_ssdp_port(lan_port)?;
    lan.join_multicast_v4(&SSDP_GROUP, &iface)?;
    lan.set_multicast_loop_v4(false).ok();
    lan.set_multicast_ttl_v4(4).ok();

    let tun = UdpSocket::bind(listen)?;
    println!(
        "ssdp-relay: LAN group {SSDP_GROUP}:{lan_port} on {}, scream clients on {}",
        if iface.is_unspecified() { "default route".to_string() } else { iface.to_string() },
        tun.local_addr()?,
    );

    // scream instances that have spoken to us recently
    let clients: Arc<Mutex<HashMap<SocketAddr, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

    // scream -> LAN
    let up = {
        let clients = clients.clone();
        let lan = lan.try_clone()?;
        let tun = tun.try_clone()?;
        std::thread::spawn(move || {
            let mut frame = [0u8; 4096];
            loop {
                let Ok((n, from)) = tun.recv_from(&mut frame) else { continue };
                let Some((dst, payload)) = decode_frame(&frame[..n]) else { continue };
                clients.lock().unwrap().insert(from, Instant::now());
                if payload.is_empty() {
                    continue; // keepalive
                }
                let _ = lan.send_to(payload, dst);
            }
        })
    };

    // LAN -> scream
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = match lan.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => {
                if up.is_finished() {
                    return Ok(());
                }
                continue;
            }
        };
        if !buf[..n].starts_with(b"M-SEARCH") {
            continue;
        }
        let frame = encode_frame(from, &buf[..n]);
        let now = Instant::now();
        let mut guard = clients.lock().unwrap();
        guard.retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(120));
        for client in guard.keys() {
            let _ = tun.send_to(&frame, client);
        }
    }
}

// HTTP side: descriptions, control and eventing under /dlna/

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn device_description(name: &str, uuid: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<device>
<deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
<friendlyName>{name}</friendlyName>
<manufacturer>opsboost</manufacturer>
<modelName>scream</modelName>
<UDN>uuid:{uuid}</UDN>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
<serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
<SCPDURL>/dlna/cds.xml</SCPDURL>
<controlURL>/dlna/cds/control</controlURL>
<eventSubURL>/dlna/cds/event</eventSubURL>
</service>
<service>
<serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
<serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
<SCPDURL>/dlna/cms.xml</SCPDURL>
<controlURL>/dlna/cms/control</controlURL>
<eventSubURL>/dlna/cms/event</eventSubURL>
</service>
</serviceList>
</device>
</root>"#,
        name = xml_escape(name),
    )
}

// The service descriptions name the actions a client may call. Kept to what
// is actually answered, a client that wants more gets a SOAP fault
const CDS_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>Browse</name></action>
<action><name>GetSystemUpdateID</name></action>
<action><name>GetSortCapabilities</name></action>
<action><name>GetSearchCapabilities</name></action>
</actionList>
<serviceStateTable/>
</scpd>"#;

const CMS_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>GetProtocolInfo</name></action>
</actionList>
<serviceStateTable/>
</scpd>"#;

fn soap_envelope(action: &str, service: &str, arguments: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:{action}Response xmlns:u="{service}">{arguments}</u:{action}Response></s:Body>
</s:Envelope>"#
    )
}

fn didl_root_container() -> String {
    r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
<container id="0" parentID="-1" restricted="1" childCount="1">
<dc:title>ISS Display</dc:title>
<upnp:class>object.container.storageFolder</upnp:class>
</container>
</DIDL-Lite>"#
        .to_string()
}

fn didl_stream_item(name: &str, base_url: &str) -> String {
    format!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
<item id="1" parentID="0" restricted="1">
<dc:title>{name}</dc:title>
<upnp:class>object.item.videoItem</upnp:class>
<res protocolInfo="{PROTOCOL_INFO}">{base_url}{STREAM_PATH}</res>
</item>
</DIDL-Lite>"#,
        name = xml_escape(name),
    )
}

fn body_argument(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;

    Some(body[start..end].trim().to_string())
}

fn browse_response(body: &str, name: &str, base_url: &str) -> String {
    let flag = body_argument(body, "BrowseFlag").unwrap_or_default();
    let object = body_argument(body, "ObjectID").unwrap_or_default();

    let (didl, returned, total) = if flag == "BrowseMetadata" && object == "0" {
        (didl_root_container(), 1, 1)
    } else {
        (didl_stream_item(name, base_url), 1, 1)
    };

    soap_envelope(
        "Browse",
        "urn:schemas-upnp-org:service:ContentDirectory:1",
        &format!(
            "<Result>{}</Result><NumberReturned>{returned}</NumberReturned>\
             <TotalMatches>{total}</TotalMatches><UpdateID>1</UpdateID>",
            xml_escape(&didl)
        ),
    )
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str,
                  body: &str) -> std::io::Result<()> {
    stream.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
             Server: {SERVER_ID}\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )?;

    stream.write_all(body.as_bytes())
}

fn control_response(path: &str, action: &str, body: &str, name: &str,
                    base_url: &str) -> Option<String> {
    if path == "/dlna/cds/control" {
        return match action {
            "Browse" => Some(browse_response(body, name, base_url)),
            "GetSystemUpdateID" => Some(soap_envelope(
                "GetSystemUpdateID",
                "urn:schemas-upnp-org:service:ContentDirectory:1",
                "<Id>1</Id>",
            )),
            "GetSortCapabilities" => Some(soap_envelope(
                "GetSortCapabilities",
                "urn:schemas-upnp-org:service:ContentDirectory:1",
                "<SortCaps></SortCaps>",
            )),
            "GetSearchCapabilities" => Some(soap_envelope(
                "GetSearchCapabilities",
                "urn:schemas-upnp-org:service:ContentDirectory:1",
                "<SearchCaps></SearchCaps>",
            )),
            _ => None,
        };
    }
    if path == "/dlna/cms/control" && action == "GetProtocolInfo" {
        return Some(soap_envelope(
            "GetProtocolInfo",
            "urn:schemas-upnp-org:service:ConnectionManager:1",
            &format!(
                "<Source>{}</Source><Sink></Sink>",
                xml_escape(PROTOCOL_INFO)
            ),
        ));
    }

    None
}

// The action name arrives in the SOAPACTION header as "service#Action"
fn soap_action(headers: &[(String, String)]) -> Option<String> {
    let value = headers
        .iter()
        .find(|(name, _)| name == "soapaction")
        .map(|(_, value)| value.trim_matches('"'))?;

    Some(value.rsplit('#').next()?.to_string())
}

// The base url to write into responses: the advertised one, else whatever
// address this connection came in on
fn response_base_url(stream: &TcpStream) -> String {
    if let Some(base) = advertise_url() {
        return base.to_string();
    }
    let host = stream
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    format!("http://{host}")
}

// Answers a /dlna request on the shared http socket. Returns false for a
// path that is not ours, so main's routing can move on
pub fn handle_request(stream: &mut TcpStream, method: &str, path: &str,
                      headers: &[(String, String)], body: &[u8],
                      name: &str) -> std::io::Result<bool> {
    if !path.starts_with("/dlna/") {
        return Ok(false);
    }

    match (method, path) {
        ("GET", "/dlna/device.xml") => {
            write_response(stream, "200 OK", "text/xml; charset=utf-8",
                           &device_description(name, &device_uuid()))?;
        }
        ("GET", "/dlna/cds.xml") => {
            write_response(stream, "200 OK", "text/xml; charset=utf-8",
                           CDS_SCPD)?;
        }
        ("GET", "/dlna/cms.xml") => {
            write_response(stream, "200 OK", "text/xml; charset=utf-8",
                           CMS_SCPD)?;
        }
        ("POST", "/dlna/cds/control") | ("POST", "/dlna/cms/control") => {
            let base_url = response_base_url(stream);
            let body = String::from_utf8_lossy(body);
            let response = soap_action(headers)
                .and_then(|action| control_response(path, &action, &body,
                                                    name, &base_url));
            match response {
                Some(xml) => write_response(stream, "200 OK",
                                            "text/xml; charset=utf-8", &xml)?,
                None => write_response(stream, "500 Internal Server Error",
                                       "text/xml; charset=utf-8",
                                       SOAP_FAULT)?,
            }
        }
        // Eventing is accepted and never delivers: the directory has one
        // item that never changes, so there is nothing to event about
        ("SUBSCRIBE", _) => {
            stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nSID: uuid:{}\r\nTIMEOUT: Second-{CACHE_MAX_AGE}\r\nContent-Length: 0\r\n\r\n",
                    device_uuid()
                )
                .as_bytes(),
            )?;
        }
        ("UNSUBSCRIBE", _) => {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
        }
        _ => {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
        }
    }

    Ok(true)
}

const SOAP_FAULT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>
<detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>401</errorCode><errorDescription>Invalid Action</errorDescription></UPnPError></detail>
</s:Fault></s:Body></s:Envelope>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_covers_the_five_entities() {
        assert_eq!(xml_escape(r#"a & b < c > "d""#), "a &amp; b &lt; c &gt; &quot;d&quot;");
    }

    #[test]
    fn device_uuid_is_stable_and_shaped_like_a_uuid() {
        let uuid = device_uuid();
        let groups: Vec<&str> = uuid.split('-').collect();
        assert_eq!(groups.iter().map(|g| g.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert_eq!(uuid, device_uuid());
    }

    #[test]
    fn header_value_matches_case_insensitively() {
        let msg = "M-SEARCH * HTTP/1.1\r\nST: ssdp:all\r\nMX: 3\r\n";
        assert_eq!(header_value(msg, "st"), Some("ssdp:all"));
        assert_eq!(header_value(msg, "missing"), None);
    }

    #[test]
    fn notification_types_all_carry_the_uuid() {
        let types = notification_types("abc");
        assert_eq!(types.len(), 5);
        assert!(types.iter().all(|(_, usn)| usn.contains("abc")));
    }

    #[test]
    fn body_argument_extracts_a_named_element() {
        let body = "<Browse><ObjectID>0</ObjectID><BrowseFlag>BrowseMetadata</BrowseFlag></Browse>";
        assert_eq!(body_argument(body, "ObjectID").as_deref(), Some("0"));
        assert_eq!(body_argument(body, "BrowseFlag").as_deref(), Some("BrowseMetadata"));
        assert_eq!(body_argument(body, "Nope"), None);
    }

    #[test]
    fn soap_action_takes_the_name_after_the_hash() {
        let headers = vec![
            ("host".to_string(), "x".to_string()),
            ("soapaction".to_string(),
             "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"".to_string()),
        ];
        assert_eq!(soap_action(&headers).as_deref(), Some("Browse"));
    }

    #[test]
    fn browse_metadata_of_the_root_returns_the_container() {
        let body = "<ObjectID>0</ObjectID><BrowseFlag>BrowseMetadata</BrowseFlag>";
        let xml = browse_response(body, "ISS Display", "http://10.0.0.1:7002");
        assert!(xml.contains("BrowseResponse"));
        assert!(xml.contains("storageFolder"));
    }

    #[test]
    fn browse_children_returns_the_stream_item_pointing_at_the_mkv() {
        let body = "<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>";
        let xml = browse_response(body, "ISS Display", "http://10.0.0.1:7002");
        assert!(xml.contains("videoItem"));
        assert!(xml.contains("http://10.0.0.1:7002/stream.mkv"));
    }

    #[test]
    fn tunnel_frame_round_trips_address_and_payload() {
        let addr = SocketAddr::from(([239, 255, 255, 250], 1900));
        let framed = encode_frame(addr, b"NOTIFY * HTTP/1.1");
        let (back, payload) = decode_frame(&framed).unwrap();
        assert_eq!(back, addr);
        assert_eq!(payload, b"NOTIFY * HTTP/1.1");
    }

    #[test]
    fn tunnel_keepalive_frame_has_no_payload() {
        let framed = encode_frame(SocketAddr::from(([0, 0, 0, 0], 0)), &[]);
        let (_, payload) = decode_frame(&framed).unwrap();
        assert!(payload.is_empty());
    }

    #[test]
    fn decode_frame_rejects_a_short_buffer() {
        assert!(decode_frame(&[1, 2, 3]).is_none());
    }
}
