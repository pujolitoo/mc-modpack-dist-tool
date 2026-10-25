//! Minecraft server status (Server List Ping) and address handling for auto-join.
//!
//! The status query speaks the same protocol the game uses for its multiplayer list:
//! handshake (next state = status) → status request → JSON reply → ping/pong for latency.
//! Works with any Java Edition server ≥ 1.7 (Vanilla, Paper, Forge, NeoForge, Fabric, proxies…).

use crate::i18n::t;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 25565;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Server address typed by the player / set in launcher.toml.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub host: String,
    /// `None` = not specified (25565, or the port from the domain's SRV record).
    pub port: Option<u16>,
}

impl Address {
    /// Port used to join from the game. Without an explicit port the game itself resolves the
    /// SRV record (`_minecraft._tcp`) when it receives the default port, like in the multiplayer list.
    pub fn join_port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }
}

/// Accepts `host`, `host:port`, `1.2.3.4:25565`, `[::1]:25565` or a bare IPv6 address.
pub fn parse_address(text: &str) -> Option<Address> {
    let t = text.trim();
    if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c == '/') {
        return None;
    }
    let (host, port) = if let Some(rest) = t.strip_prefix('[') {
        // [IPv6]:port
        let (h, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?.parse::<u16>().ok()?),
        };
        (h.to_string(), port)
    } else if t.matches(':').count() > 1 {
        // Bare IPv6 without a port.
        (t.to_string(), None)
    } else if let Some((h, p)) = t.rsplit_once(':') {
        (h.to_string(), Some(p.parse::<u16>().ok()?))
    } else {
        (t.to_string(), None)
    };
    if host.is_empty() || port == Some(0) {
        return None;
    }
    if host.parse::<IpAddr>().is_err()
        && !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    {
        return None;
    }
    Some(Address { host, port })
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub online: i64,
    pub max: i64,
    /// Sample of connected players (the server may send only some of them, or none).
    pub sample: Vec<String>,
    pub version: String,
    pub motd: String,
    pub latency_ms: Option<u64>,
    /// Server icon (the one shown in Minecraft's multiplayer list), if it has one.
    pub icon: Option<Icon>,
}

/// Decoded image in RGBA8 (normally 64×64).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icon {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

// ───────── protocol primitives ─────────

fn write_varint(buf: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if v & !0x7F == 0 {
            buf.push(v as u8);
            return;
        }
        buf.push(((v & 0x7F) | 0x80) as u8);
        v >>= 7;
    }
}

fn read_varint(r: &mut impl Read) -> Result<i32> {
    let mut result: u32 = 0;
    for i in 0..5 {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        result |= ((b[0] & 0x7F) as u32) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(result as i32);
        }
    }
    bail!(t("server.invalid_varint_server_reply"))
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as i32);
    buf.extend_from_slice(s.as_bytes());
}

fn send_packet(stream: &mut TcpStream, id: i32, payload: &[u8]) -> Result<()> {
    let mut body = Vec::with_capacity(payload.len() + 5);
    write_varint(&mut body, id);
    body.extend_from_slice(payload);
    let mut packet = Vec::with_capacity(body.len() + 5);
    write_varint(&mut packet, body.len() as i32);
    packet.extend_from_slice(&body);
    stream.write_all(&packet)?;
    Ok(())
}

/// Reads a packet and returns (id, payload).
fn read_packet(stream: &mut TcpStream) -> Result<(i32, Vec<u8>)> {
    let len = read_varint(stream)?;
    if !(1..=4 * 1024 * 1024).contains(&len) {
        bail!(tf!("server.unexpected_packet_size_from_server", len = len));
    }
    let mut data = vec![0u8; len as usize];
    stream.read_exact(&mut data)?;
    let mut cur = std::io::Cursor::new(data);
    let id = read_varint(&mut cur)?;
    let pos = cur.position() as usize;
    let mut data = cur.into_inner();
    data.drain(..pos);
    Ok((id, data))
}

// ───────── SRV ─────────

/// Looks up the `_minecraft._tcp.<host>` SRV record over DNS-over-HTTPS (the standard library cannot
/// query SRV records). Any failure simply means «no SRV»: then <host>:25565 is used.
fn resolve_srv(host: &str) -> Option<(String, u16)> {
    let url = format!("https://cloudflare-dns.com/dns-query?name=_minecraft._tcp.{host}&type=SRV");
    let text = ureq::get(&url)
        .set("accept", "application/dns-json")
        .timeout(Duration::from_secs(3))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let body: Value = serde_json::from_str(&text).ok()?;
    body.get("Answer")?.as_array()?.iter().find_map(|a| {
        if a.get("type")?.as_i64()? != 33 {
            return None;
        }
        // "priority weight port target."
        let data = a.get("data")?.as_str()?;
        let parts: Vec<&str> = data.split_whitespace().collect();
        let port = parts.get(2)?.parse::<u16>().ok()?;
        let target = parts.get(3)?.trim_end_matches('.').to_string();
        if target.is_empty() { None } else { Some((target, port)) }
    })
}

// ───────── status ─────────

/// Connects to the server and asks for its status. Blocking (call it from a thread).
pub fn query(addr: &Address) -> Result<Status> {
    let (conn_host, conn_port) = match addr.port {
        Some(p) => (addr.host.clone(), p),
        None if addr.host.parse::<IpAddr>().is_ok() => (addr.host.clone(), DEFAULT_PORT),
        None => resolve_srv(&addr.host).unwrap_or_else(|| (addr.host.clone(), DEFAULT_PORT)),
    };

    let sock = (conn_host.as_str(), conn_port)
        .to_socket_addrs()
        .with_context(|| tf!("server.could_not_resolve", host = conn_host))?
        .next()
        .ok_or_else(|| anyhow!(tf!("server.could_not_resolve", host = conn_host)))?;
    let mut stream = TcpStream::connect_timeout(&sock, TIMEOUT).map_err(|e| match e.kind() {
        std::io::ErrorKind::TimedOut => anyhow!(t("server.no_response_timed_out")),
        std::io::ErrorKind::ConnectionRefused => anyhow!(t("server.connection_refused_server_down")),
        _ => anyhow!(tf!("server.could_not_connect", error = e)),
    })?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let _ = stream.set_nodelay(true);

    // Handshake: protocol -1 («any»), the address the player typed (proxies use it for virtual hosts),
    // port, next state 1 = status.
    let mut hs = Vec::new();
    write_varint(&mut hs, -1);
    write_string(&mut hs, &addr.host);
    hs.extend_from_slice(&addr.join_port().to_be_bytes());
    write_varint(&mut hs, 1);
    send_packet(&mut stream, 0x00, &hs)?;
    send_packet(&mut stream, 0x00, &[])?;

    let (id, payload) = read_packet(&mut stream).context(t("server.server_did_not_respond"))?;
    if id != 0x00 {
        bail!(tf!("server.unexpected_reply_from_server_packet", id = format!("{:#x}", id)));
    }
    let mut cur = std::io::Cursor::new(payload);
    let len = read_varint(&mut cur)? as usize;
    let start = cur.position() as usize;
    let data = cur.into_inner();
    let json_bytes = data.get(start..start + len).ok_or_else(|| anyhow!(t("server.incomplete_reply_from_server")))?;
    let json: Value = serde_json::from_slice(json_bytes).context(t("server.invalid_reply_from_server"))?;
    let mut status = parse_status(&json);

    // Ping/pong for latency (optional: some servers close the connection here).
    let t0 = Instant::now();
    let token = 0x6D63_6C61_756E_6368_i64; // any value; the server echoes it back
    if send_packet(&mut stream, 0x01, &token.to_be_bytes()).is_ok() {
        if let Ok((0x01, _)) = read_packet(&mut stream) {
            status.latency_ms = Some(t0.elapsed().as_millis() as u64);
        }
    }
    Ok(status)
}

fn parse_status(json: &Value) -> Status {
    let players = json.get("players");
    let num = |k: &str| players.and_then(|p| p.get(k)).and_then(|v| v.as_i64()).unwrap_or(0);
    let sample = players
        .and_then(|p| p.get("sample"))
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get("name")?.as_str())
                .map(strip_formatting)
                .filter(|n| !n.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    let version = json
        .get("version")
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .map(strip_formatting)
        .unwrap_or_default();
    let mut motd = String::new();
    if let Some(d) = json.get("description") {
        flatten_chat(d, &mut motd);
    }
    let motd = strip_formatting(&motd)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let icon = json.get("favicon").and_then(|v| v.as_str()).and_then(decode_favicon);
    Status { online: num("online"), max: num("max"), sample, version, motd, latency_ms: None, icon }
}

/// Turns a chat component (string, object with text/extra, or array) into plain text.
fn flatten_chat(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => out.push_str(s),
        Value::Array(a) => a.iter().for_each(|x| flatten_chat(x, out)),
        Value::Object(o) => {
            if let Some(t) = o.get("text") {
                flatten_chat(t, out);
            }
            if let Some(Value::Array(extra)) = o.get("extra") {
                extra.iter().for_each(|x| flatten_chat(x, out));
            }
        }
        _ => {}
    }
}

/// `favicon` comes as `data:image/png;base64,<…>`. Any problem simply means «no icon».
fn decode_favicon(uri: &str) -> Option<Icon> {
    let b64 = uri.strip_prefix("data:image/png;base64,")?;
    let bytes = base64_decode(b64)?;
    decode_png(&bytes)
}

/// Standard base64 decoder (ignores line breaks, which some servers include).
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// PNG → RGBA8 (whatever the original colour type and bit depth).
pub(crate) fn decode_png(bytes: &[u8]) -> Option<Icon> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None, // normalize_to_color8 already expands palettes
    };
    if rgba.len() != w * h * 4 {
        return None;
    }
    Some(Icon { width: w, height: h, rgba })
}

/// Removes legacy formatting codes (§a, §l…).
fn strip_formatting(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(parse_address("play.example.com"), Some(Address { host: "play.example.com".into(), port: None }));
        assert_eq!(parse_address(" mc.test:25570 "), Some(Address { host: "mc.test".into(), port: Some(25570) }));
        assert_eq!(parse_address("[::1]:25565"), Some(Address { host: "::1".into(), port: Some(25565) }));
        assert_eq!(parse_address("::1"), Some(Address { host: "::1".into(), port: None }));
        assert_eq!(parse_address(""), None);
        assert_eq!(parse_address("a b"), None);
        assert_eq!(parse_address("host:99999"), None);
        assert_eq!(parse_address("host:"), None);
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0, 1, 127, 128, 255, 25565, 2097151, i32::MAX, -1] {
            let mut b = Vec::new();
            write_varint(&mut b, v);
            assert_eq!(read_varint(&mut std::io::Cursor::new(b)).unwrap(), v);
        }
    }

    #[test]
    fn status_json() {
        let j: Value = serde_json::from_str(
            r#"{"version":{"name":"NeoForge 1.21.1","protocol":767},
                "players":{"max":50,"online":3,"sample":[{"name":"§aAlex","id":"x"},{"name":"Steve","id":"y"}]},
                "description":{"text":"§6Villa","extra":[{"text":"zucar\n"},{"text":" Welcome"}]}}"#,
        )
        .unwrap();
        let s = parse_status(&j);
        assert_eq!((s.online, s.max), (3, 50));
        assert_eq!(s.sample, vec!["Alex", "Steve"]);
        assert_eq!(s.version, "NeoForge 1.21.1");
        assert_eq!(s.motd, "Villazucar · Welcome");
        assert!(s.icon.is_none());
    }

    #[test]
    fn favicon() {
        // 2×1 RGB PNG: a red and a blue pixel.
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJggg==";
        let icon = decode_favicon(uri).expect("icon");
        assert_eq!((icon.width, icon.height), (2, 1));
        assert_eq!(icon.rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        assert!(decode_favicon("data:image/png;base64,!!!").is_none());
        assert!(decode_favicon("not a data uri").is_none());
    }
}
