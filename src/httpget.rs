//! The smallest HTTP client that fetches one JSON document.
//!
//! The hub is a router, not a service mesh: the only outbound request it ever
//! makes is a `GET` for a small, plain-HTTP JSON document on the tailnet (the
//! mirrored home display), and nothing about that needs a redirect policy, a
//! cookie jar, or a TLS stack. Rather than take a full client as a dependency
//! for one call, this module speaks just enough HTTP/1.1 to do it.
//!
//! The parts that are easy to get subtly wrong — where the head ends, and how a
//! chunked body is framed — are pure functions, tested directly. The socket
//! part is a connect-write-read with a single deadline over all three.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A parsed `http://host[:port]/path` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Join a configured base with a path, without doubling or dropping the slash.
///
/// `ROOST_MIRROR_URL=http://nas:3009` and `http://nas:3009/` must both yield
/// `http://nas:3009/api/state`.
pub fn join(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Parse a plain-HTTP URL. `https://` is rejected with a message that says why:
/// these endpoints are on the tailnet, where TLS buys nothing and would add a
/// trust store to the image.
pub fn parse_http_url(raw: &str) -> Result<HttpUrl> {
    let rest = raw
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("only http:// URLs are supported, got {raw:?}"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        bail!("no host in {raw:?}");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| anyhow!("bad port in {raw:?}"))?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        bail!("no host in {raw:?}");
    }
    Ok(HttpUrl {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// `GET` a URL and parse its body as JSON, under one deadline for the whole
/// exchange. A mirror that has stopped answering must not hold the browser's
/// request open.
pub async fn get_json(url: &HttpUrl, timeout: Duration) -> Result<Value> {
    let raw = tokio::time::timeout(timeout, fetch(url))
        .await
        .map_err(|_| anyhow!("timed out after {}ms", timeout.as_millis()))??;
    let body = parse_response(&raw)?;
    Ok(serde_json::from_slice(&body)?)
}

async fn fetch(url: &HttpUrl) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect((url.host.as_str(), url.port)).await?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nUser-Agent: roost\r\nConnection: close\r\n\r\n",
        url.path,
        host_header(url)
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    // `Connection: close` means the body ends at EOF, so reading to the end is
    // the whole framing story for everything except a chunked response.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    Ok(raw)
}

/// The `Host` header: the port only when it is not the default, per RFC 9110.
fn host_header(url: &HttpUrl) -> String {
    if url.port == 80 {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    }
}

/// Split a raw response into its head and body at the first blank line.
fn split_head(raw: &[u8]) -> Result<(&[u8], &[u8])> {
    match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(index) => Ok((&raw[..index], &raw[index + 4..])),
        None => bail!("malformed response: no header terminator"),
    }
}

/// Turn a raw response into its body, rejecting a non-2xx status and undoing
/// chunked framing when the server used it. (The device does: its JSON is sent
/// `Transfer-Encoding: chunked`.)
pub fn parse_response(raw: &[u8]) -> Result<Vec<u8>> {
    let (head, body) = split_head(raw)?;
    let head = std::str::from_utf8(head).map_err(|_| anyhow!("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed status line: {status:?}"))?;
    if !(200..300).contains(&code) {
        bail!("upstream returned {:?}", status);
    }
    let chunked = lines.any(|line| match line.split_once(':') {
        Some((name, value)) => {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        }
        None => false,
    });
    if chunked {
        dechunk(body)
    } else {
        Ok(body.to_vec())
    }
}

/// Decode a chunked body: `size-in-hex[;ext] CRLF bytes CRLF`, repeated, ended
/// by a zero-size chunk. Trailer fields after the terminator are ignored.
fn dechunk(body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(newline) = rest.windows(2).position(|window| window == b"\r\n") else {
            bail!("truncated chunked body");
        };
        let size_line = std::str::from_utf8(&rest[..newline])?.trim();
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| anyhow!("bad chunk size {size_hex:?}"))?;
        rest = &rest[newline + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            bail!(
                "truncated chunk: declared {size} bytes, {} left",
                rest.len()
            );
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_and_an_explicit_port_both_parse() {
        let bare = parse_http_url("http://nas:3009/api/state").unwrap();
        assert_eq!(
            bare,
            HttpUrl {
                host: "nas".into(),
                port: 3009,
                path: "/api/state".into()
            }
        );
        // A host with no port and no path still has a Host header and a path.
        let plain = parse_http_url("http://display.lan").unwrap();
        assert_eq!(plain.port, 80);
        assert_eq!(plain.path, "/");
    }

    #[test]
    fn https_is_refused_with_a_reason() {
        let error = parse_http_url("https://nas:3009/api/state").unwrap_err();
        assert!(error.to_string().contains("only http://"));
        // And a nonsense scheme is refused the same way, not silently guessed.
        assert!(parse_http_url("nas:3009").is_err());
        assert!(parse_http_url("http://").is_err());
        assert!(parse_http_url("http://nas:notaport/").is_err());
    }

    #[test]
    fn joining_a_base_never_doubles_or_drops_the_slash() {
        for base in ["http://nas:3009", "http://nas:3009/"] {
            assert_eq!(join(base, "/api/state"), "http://nas:3009/api/state");
            assert_eq!(join(base, "api/state"), "http://nas:3009/api/state");
        }
    }

    #[test]
    fn a_content_length_body_is_returned_whole() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\"}";
        assert_eq!(parse_response(raw).unwrap(), b"{\"status\":\"ok\"}");
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        // The exact framing the device uses: header case ignored, JSON split
        // across chunks, terminated by a zero chunk.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n9\r\n{\"status\"\r\n6\r\n:\"ok\"}\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), b"{\"status\":\"ok\"}");
    }

    #[test]
    fn a_chunk_extension_is_not_mistaken_for_the_size() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), b"hello");
    }

    #[test]
    fn a_non_2xx_status_is_an_error_not_a_body() {
        let raw = b"HTTP/1.1 503 Service Unavailable\r\n\r\nnope";
        let error = parse_response(raw).unwrap_err();
        assert!(error.to_string().contains("503"), "{error}");
    }

    #[test]
    fn truncated_framing_fails_loudly_rather_than_returning_half_a_document() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nno terminator").is_err());
        assert!(dechunk(b"9\r\n{\"status\"").is_err());
        assert!(dechunk(b"zz\r\nnope\r\n").is_err());
    }
}
