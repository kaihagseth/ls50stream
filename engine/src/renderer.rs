//! UPnP AVTransport control: tells a speaker to play (or stop) our stream.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::discovery::Speaker;
use crate::stream::BITS;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// DLNA protocolInfo for our live stream (no seeking, streaming transfer).
pub fn protocol_info() -> String {
    "http-get:*:audio/wav:DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000".into()
}

pub fn didl(url: &str, rate: u32) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"mac\" parentID=\"0\" restricted=\"1\">\
         <dc:title>Mac</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"{}\" sampleFrequency=\"{rate}\" bitsPerSample=\"{BITS}\" nrAudioChannels=\"2\">{}</res>\
         </item></DIDL-Lite>",
        protocol_info(),
        escape(url)
    )
}

pub fn envelope(action: &str, args: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"><InstanceID>0</InstanceID>{args}</u:{action}>\
         </s:Body></s:Envelope>"
    )
}

async fn call(speaker: &Speaker, action: &str, args: &str) -> Result<(), String> {
    call_text(speaker, action, args).await.map(|_| ())
}

/// Performs an action and returns the response body.
async fn call_text(speaker: &Speaker, action: &str, args: &str) -> Result<String, String> {
    let body = envelope(action, args);
    let request = format!(
        "POST {} HTTP/1.0\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPACTION: \"urn:schemas-upnp-org:service:AVTransport:1#{action}\"\r\nContent-Length: {}\r\n\r\n{body}",
        speaker.av_transport,
        speaker.address,
        body.len()
    );
    let response = timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(speaker.address).await.map_err(|e| e.to_string())?;
        stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut response = Vec::new();
        stream.take(64 * 1024).read_to_end(&mut response).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(String::from_utf8_lossy(&response).into_owned())
    })
    .await
    .map_err(|_| format!("{action}: no answer"))??;

    let status = response.split_whitespace().nth(1).unwrap_or("");
    if status == "200" {
        Ok(response)
    } else {
        let detail = response
            .split("<errorDescription>")
            .nth(1)
            .and_then(|s| s.split('<').next())
            .unwrap_or("")
            .to_string();
        Err(format!("{action}: HTTP {status} {detail}").trim().to_string())
    }
}

/// Points the speaker at our stream and starts it. The speaker switches its own
/// input to the network streamer when playback starts.
pub async fn play(speaker: &Speaker, url: &str, rate: u32) -> Result<(), String> {
    let args = format!("<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>", escape(url), escape(&didl(url, rate)));
    call(speaker, "SetAVTransportURI", &args).await?;
    call(speaker, "Play", "<Speed>1</Speed>").await
}

pub async fn stop(speaker: &Speaker) -> Result<(), String> {
    call(speaker, "Stop", "").await
}

/// Queues `url` to play gaplessly after the current track.
pub async fn set_next(speaker: &Speaker, url: &str, rate: u32) -> Result<(), String> {
    let args = format!("<NextURI>{}</NextURI><NextURIMetaData>{}</NextURIMetaData>", escape(url), escape(&didl(url, rate)));
    call(speaker, "SetNextAVTransportURI", &args).await
}

/// (current track URI, next track URI), empty when unset.
pub async fn media_info(speaker: &Speaker) -> Result<(String, String), String> {
    let body = call_text(speaker, "GetMediaInfo", "").await?;
    Ok((element(&body, "CurrentURI"), element(&body, "NextURI")))
}

/// e.g. "PLAYING", "STOPPED", "TRANSITIONING".
pub async fn transport_state(speaker: &Speaker) -> Result<String, String> {
    Ok(element(&call_text(speaker, "GetTransportInfo", "").await?, "CurrentTransportState"))
}

fn element(xml: &str, name: &str) -> String {
    let open = format!("<{name}>");
    xml.split_once(&open)
        .and_then(|(_, rest)| rest.split_once(&format!("</{name}>")))
        .map(|(value, _)| crate::discovery::unescape(value.trim()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_media_info() {
        let body = "HTTP/1.0 200 OK\r\n\r\n<s:Envelope><s:Body><u:GetMediaInfoResponse><NrTracks>1</NrTracks>\
            <CurrentURI>http://192.168.50.63:8300/stream/48000/0.wav</CurrentURI><NextURI></NextURI>\
            </u:GetMediaInfoResponse></s:Body></s:Envelope>";
        assert_eq!(element(body, "CurrentURI"), "http://192.168.50.63:8300/stream/48000/0.wav");
        assert_eq!(element(body, "NextURI"), "");
        assert_eq!(element(body, "Missing"), "");
    }

    #[test]
    fn metadata_is_escaped_once_per_level() {
        let url = "http://192.168.50.67:8300/stream/192000.wav";
        let didl = didl(url, 192000);
        assert!(didl.contains(">http://192.168.50.67:8300/stream/192000.wav</res>"));
        assert!(didl.contains("sampleFrequency=\"192000\" bitsPerSample=\"24\""));
        let args = format!("<CurrentURIMetaData>{}</CurrentURIMetaData>", escape(&didl));
        assert!(args.contains("&lt;DIDL-Lite"));
        assert!(!args.contains("<DIDL-Lite"));
    }
}
