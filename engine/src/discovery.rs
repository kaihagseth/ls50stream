//! Finds supported DLNA renderers (any KEF LS50 Wireless) with SSDP.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use log::debug;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{timeout, Instant};

/// A renderer we may stream to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Speaker {
    pub name: String,
    pub serial: String,
    pub manufacturer: String,
    /// UPnP modelDescription, e.g. "LS50 Wireless"
    pub model: String,
    /// host:port of its UPnP HTTP server
    pub address: SocketAddr,
    /// Path of the AVTransport control endpoint
    pub av_transport: String,
}

const SEARCH: &[u8] = b"M-SEARCH * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
MAN: \"ssdp:discover\"\r\n\
MX: 1\r\n\
ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";

impl Speaker {
    /// Whether we know how to stream to this renderer: the (first-generation) KEF LS50 Wireless.
    /// The LS50 Wireless II reports "LS50 Wireless II" and is not matched.
    pub fn is_supported(&self) -> bool {
        self.manufacturer.trim().eq_ignore_ascii_case("KEF") && self.model.trim().eq_ignore_ascii_case("LS50 Wireless")
    }
}

/// Searches the network for `wait`, returning the supported speakers that answered, sorted by
/// serial number. If `only` is not empty, just the speakers with those serial numbers.
pub async fn search(only: &[String], wait: Duration) -> Vec<Speaker> {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else { return Vec::new() };
    for _ in 0..2 {
        let _ = socket.send_to(SEARCH, "239.255.255.250:1900").await;
    }
    let deadline = Instant::now() + wait;
    let mut locations = HashSet::new();
    let mut buf = [0u8; 2048];
    while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
        if let Some(location) = header(&String::from_utf8_lossy(&buf[..n]), "location") {
            locations.insert(location);
        }
    }

    let mut found = Vec::new();
    for location in locations {
        match describe(&location).await {
            Some(s) if !s.is_supported() => {
                debug!("Ignoring renderer {} ({} {}): not supported", s.name, s.manufacturer, s.model)
            }
            Some(s) if !only.is_empty() && !only.iter().any(|w| w.eq_ignore_ascii_case(&s.serial)) => {
                debug!("Ignoring {} ({}): not one of the chosen speakers", s.name, s.serial)
            }
            Some(s) => {
                if !found.iter().any(|f: &Speaker| f.serial == s.serial) {
                    found.push(s);
                }
            }
            None => {}
        }
    }
    found.sort_by(|a, b| a.serial.cmp(&b.serial));
    found
}

fn header(response: &str, name: &str) -> Option<String> {
    response.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

/// Splits "http://host:port/path" into (address, path).
pub fn split_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let address = if authority.contains(':') { authority.parse().ok()? } else { format!("{authority}:80").parse().ok()? };
    Some((address, path))
}

/// Minimal HTTP/1.0 GET; returns the body.
pub async fn http_get(address: SocketAddr, path: &str) -> Option<String> {
    let request = format!("GET {path} HTTP/1.0\r\nHost: {address}\r\n\r\n");
    let response = timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(address).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut body = Vec::new();
        stream.take(512 * 1024).read_to_end(&mut body).await.ok()?;
        Some(body)
    })
    .await
    .ok()??;
    let text = String::from_utf8_lossy(&response).into_owned();
    text.split_once("\r\n\r\n").map(|(_, body)| body.to_string())
}

async fn describe(location: &str) -> Option<Speaker> {
    let (address, path) = split_url(location)?;
    parse_description(&http_get(address, &path).await?, address)
}

pub fn parse_description(xml: &str, address: SocketAddr) -> Option<Speaker> {
    let service_start = xml.find("urn:schemas-upnp-org:service:AVTransport:1</serviceType>")?;
    let service = &xml[service_start..];
    let service = &service[..service.find("</service>").unwrap_or(service.len())];
    let mut av_transport = text_of(service, "controlURL")?;
    if !av_transport.starts_with('/') {
        av_transport.insert(0, '/');
    }
    Some(Speaker {
        name: text_of(xml, "friendlyName").unwrap_or_default(),
        serial: text_of(xml, "serialNumber")?,
        manufacturer: text_of(xml, "manufacturer").unwrap_or_default(),
        model: text_of(xml, "modelDescription").unwrap_or_default(),
        address,
        av_transport,
    })
}

/// Text of the first <name>…</name> element (no nesting), entities decoded.
fn text_of(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{name}>"))?;
    Some(unescape(xml[start..end].trim()))
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_ls50w_description() {
        let xml = include_str!("../tests/fixtures/ls50w-description.xml");
        let address: SocketAddr = "192.168.50.100:8080".parse().unwrap();
        let s = parse_description(xml, address).unwrap();
        assert_eq!(s.name, "LS50W");
        assert_eq!(s.serial, "84171500A1B2");
        assert_eq!(s.av_transport, "/AVTransport/ctrl");
        assert_eq!(s.address, address);
        assert!(s.is_supported());
    }

    #[test]
    fn supports_only_the_first_generation_ls50w() {
        let xml = include_str!("../tests/fixtures/ls50w-description.xml");
        let address: SocketAddr = "192.168.50.100:8080".parse().unwrap();
        let other = |from: &str, to: &str| parse_description(&xml.replace(from, to), address).unwrap();
        assert!(!other("<modelDescription>LS50 Wireless</", "<modelDescription>LS50 Wireless II</").is_supported());
        assert!(!other("<modelDescription>LS50 Wireless</", "<modelDescription>LSX</").is_supported());
        assert!(!other("<manufacturer>KEF</", "<manufacturer>Sonos</").is_supported());
        // A renamed speaker is still an LS50W.
        assert!(other("<friendlyName>LS50W</", "<friendlyName>Living room</").is_supported());
    }

    #[test]
    fn splits_urls_and_reads_headers() {
        let (address, path) = split_url("http://192.168.50.100:8080/description.xml").unwrap();
        assert_eq!(address.to_string(), "192.168.50.100:8080");
        assert_eq!(path, "/description.xml");
        assert_eq!(split_url("http://10.0.0.2/").unwrap().0.port(), 80);
        let r = "HTTP/1.1 200 OK\r\nLOCATION: http://x:1/d.xml\r\n\r\n";
        assert_eq!(header(r, "location").as_deref(), Some("http://x:1/d.xml"));
    }
}
