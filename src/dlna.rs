// DLNA MediaServer announcement and control, so a client like the PS4 media
// player autodiscovers the stream. Three parts, all speaking over what main.rs
// already runs: ssdp on udp 1900 for discovery, device and service
// descriptions as xml over http, and a ContentDirectory answering SOAP Browse
// with one live item pointing at the matroska stream.
//
// © 2021 Björn Busse (see also: LICENSE)

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SSDP_ADDRESS: &str = "239.255.255.250:1900";
const CACHE_MAX_AGE: u32 = 1800;
const SERVER_ID: &str = "Linux UPnP/1.0 scream/0.1";
const STREAM_PATH: &str = "/stream.mkv";
// mkv because that is what the shipped gst plugins can mux around x264;
// the strict dlna live profile would be mpeg-ts, which needs tsmux from
// gst-plugins-bad
const PROTOCOL_INFO: &str = "http-get:*:video/x-matroska:\
    DLNA.ORG_OP=00;DLNA.ORG_CI=0;\
    DLNA.ORG_FLAGS=01700000000000000000000000000000";

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

fn location(http_port: u16, toward: SocketAddr) -> Option<String> {
    let ip = address_toward(toward)?;

    Some(format!("http://{ip}:{http_port}/dlna/device.xml"))
}

fn notify(socket: &UdpSocket, http_port: u16, uuid: &str, alive: bool) {
    let target: SocketAddr = match SSDP_ADDRESS.parse() {
        Ok(a) => a,
        Err(_) => return,
    };
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
        let _ = socket.send_to(message.as_bytes(), SSDP_ADDRESS);
    }
}

fn answer_search(socket: &UdpSocket, peer: SocketAddr, http_port: u16,
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
        let _ = socket.send_to(response.as_bytes(), peer);
    }
}

pub fn spawn_ssdp(http_port: u16, shutdown: Arc<AtomicBool>) {
    std::thread::spawn(move || ssdp_loop(http_port, shutdown));
}

fn ssdp_loop(http_port: u16, shutdown: Arc<AtomicBool>) {
    let socket = match UdpSocket::bind(("0.0.0.0", 1900)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ssdp: cannot bind 1900: {e}");
            return;
        }
    };
    if let Err(e) =
        socket.join_multicast_v4(&Ipv4Addr::new(239, 255, 255, 250), &Ipv4Addr::UNSPECIFIED)
    {
        eprintln!("ssdp: cannot join multicast: {e}");
        return;
    }
    let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));

    let uuid = device_uuid();
    println!("SSDP: announcing MediaServer uuid:{uuid}");

    let notify_every = Duration::from_secs((CACHE_MAX_AGE / 3) as u64);
    let mut last_notify: Option<Instant> = None;
    let mut buffer = [0u8; 2048];

    while !shutdown.load(Ordering::Relaxed) {
        if last_notify.map_or(true, |t| t.elapsed() >= notify_every) {
            notify(&socket, http_port, &uuid, true);
            last_notify = Some(Instant::now());
        }

        let (len, peer) = match socket.recv_from(&mut buffer) {
            Ok(received) => received,
            Err(_) => continue,
        };
        let Ok(text) = std::str::from_utf8(&buffer[..len]) else { continue };
        if !text.starts_with("M-SEARCH") {
            continue;
        }
        if let Some(st) = header_value(text, "st") {
            answer_search(&socket, peer, http_port, &uuid, st);
        }
    }

    notify(&socket, http_port, &uuid, false);
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
// is actually answered; a client that wants more gets a SOAP fault
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

fn didl_stream_item(name: &str, host: &str) -> String {
    format!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
<item id="1" parentID="0" restricted="1">
<dc:title>{name}</dc:title>
<upnp:class>object.item.videoItem</upnp:class>
<res protocolInfo="{PROTOCOL_INFO}">http://{host}{STREAM_PATH}</res>
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

fn browse_response(body: &str, name: &str, host: &str) -> String {
    let flag = body_argument(body, "BrowseFlag").unwrap_or_default();
    let object = body_argument(body, "ObjectID").unwrap_or_default();

    let (didl, returned, total) = if flag == "BrowseMetadata" && object == "0" {
        (didl_root_container(), 1, 1)
    } else {
        (didl_stream_item(name, host), 1, 1)
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
                    host: &str) -> Option<String> {
    if path == "/dlna/cds/control" {
        return match action {
            "Browse" => Some(browse_response(body, name, host)),
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
            let host = stream
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let body = String::from_utf8_lossy(body);
            let response = soap_action(headers)
                .and_then(|action| control_response(path, &action, &body,
                                                    name, &host));
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
        let xml = browse_response(body, "ISS Display", "10.0.0.1:7002");
        assert!(xml.contains("BrowseResponse"));
        assert!(xml.contains("storageFolder"));
    }

    #[test]
    fn browse_children_returns_the_stream_item_pointing_at_the_mkv() {
        let body = "<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>";
        let xml = browse_response(body, "ISS Display", "10.0.0.1:7002");
        assert!(xml.contains("videoItem"));
        assert!(xml.contains("http://10.0.0.1:7002/stream.mkv"));
    }
}
