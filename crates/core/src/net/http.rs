//! A deliberately tiny HTTP/1.1 GET, used only to fetch the current zstd
//! dictionary when a server has retired the bundled one. Pulling in a full
//! HTTP client for one rare request is not worth the dependency weight.

use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_BODY: usize = 8 << 20;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Install the process-wide rustls crypto provider (idempotent).
pub(crate) fn ensure_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, host: &str, path: &str) -> io::Result<(u16, Vec<u8>)> {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: vortex\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    let mut raw = Vec::with_capacity(128 * 1024);
    let mut limited = (&mut stream).take((MAX_BODY + 64 * 1024) as u64);
    // Servers may close without a TLS close_notify; what we got is still usable.
    if let Err(e) = limited.read_to_end(&mut raw).await {
        if raw.is_empty() {
            return Err(e);
        }
    }
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let split = memchr::memmem::find(raw, b"\r\n\r\n").ok_or_else(|| bad("no header terminator"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad("non-UTF-8 headers"))?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("bad status line"))?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        } else if k.eq_ignore_ascii_case("content-length") {
            length = v.parse::<usize>().ok();
        }
    }
    let body = if chunked {
        dechunk(body)?
    } else {
        match length {
            Some(n) if n <= body.len() => body[..n].to_vec(),
            Some(_) => return Err(bad("truncated body")),
            None => body.to_vec(),
        }
    };
    Ok((status, body))
}

fn dechunk(mut data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let eol = memchr::memmem::find(data, b"\r\n").ok_or_else(|| bad("bad chunk header"))?;
        let size_str = std::str::from_utf8(&data[..eol]).map_err(|_| bad("bad chunk size"))?;
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| bad("bad chunk size"))?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 || out.len() + size > MAX_BODY {
            return Err(bad("truncated or oversized chunk"));
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// GET `path` from a websocket-style host spec (`host`, `wss://host`,
/// `ws://host:port`), over TLS unless the scheme says otherwise.
pub(crate) async fn get(host_spec: &str, path: &str) -> io::Result<(u16, Vec<u8>)> {
    let (tls, authority) = match host_spec.split_once("://") {
        Some(("ws" | "http", rest)) => (false, rest),
        Some((_, rest)) => (true, rest),
        None => (true, host_spec),
    };
    let authority = authority.trim_end_matches('/');
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() => (h, p.parse().map_err(|_| bad("bad port"))?),
        _ => (authority, if tls { 443 } else { 80 }),
    };
    let tcp = TcpStream::connect((host, port)).await?;
    tcp.set_nodelay(true).ok();
    if !tls {
        return exchange(tcp, authority, path).await;
    }
    ensure_crypto();
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| bad("bad host name"))?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await?;
    exchange(stream, authority, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_and_chunked() {
        let (s, b) = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX: y\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!((s, b.as_slice()), (200, &b"hello"[..]));
        let (s, b) =
            parse_response(b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=1\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!((s, b.as_slice()), (404, &b"abcde"[..]));
        let (_, b) = parse_response(b"HTTP/1.0 200 OK\r\n\r\nuntil close").unwrap();
        assert_eq!(b, b"until close");
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
    }
}
