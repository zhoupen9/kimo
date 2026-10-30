//! TCP/TLS transport plus a small buffered reader used by both protocol
//! clients. IMAP literals and POP3 multi-line responses require reading raw
//! byte counts, so the reader exposes `read_line` and `read_exact_n`.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;

use kimo_core::models::TlsMode;

pub enum MailStream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    /// Placeholder used while swapping the stream during a STARTTLS upgrade.
    Closed,
}

impl Read for MailStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            MailStream::Plain(s) => s.read(buf),
            MailStream::Tls(s) => s.read(buf),
            MailStream::Closed => Err(io::Error::new(io::ErrorKind::NotConnected, "stream closed")),
        }
    }
}

impl Write for MailStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            MailStream::Plain(s) => s.write(buf),
            MailStream::Tls(s) => s.write(buf),
            MailStream::Closed => Err(io::Error::new(io::ErrorKind::NotConnected, "stream closed")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            MailStream::Plain(s) => s.flush(),
            MailStream::Tls(s) => s.flush(),
            MailStream::Closed => Ok(()),
        }
    }
}

pub fn tls_wrap(
    tcp: TcpStream,
    host: &str,
) -> io::Result<Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
}

/// Connect to `host:port`. For `Implicit` TLS the connection is wrapped right
/// away; `StartTls`/`Plain` start in the clear (the caller upgrades later).
pub fn connect(host: &str, port: u16, tls: TlsMode) -> io::Result<MailStream> {
    let tcp = TcpStream::connect((host, port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(60)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(60)))?;
    match tls {
        TlsMode::Implicit => Ok(MailStream::Tls(tls_wrap(tcp, host)?)),
        TlsMode::StartTls | TlsMode::Plain => Ok(MailStream::Plain(tcp)),
    }
}

/// Buffered connection: reads CRLF-terminated lines and exact byte counts.
pub struct Conn {
    stream: MailStream,
    buf: Vec<u8>,
    len: usize,
    pos: usize,
}

impl Conn {
    pub fn new(stream: MailStream) -> Self {
        Conn {
            stream,
            buf: vec![0; 8192],
            len: 0,
            pos: 0,
        }
    }

    /// Swap the underlying plain stream for a TLS stream (STARTTLS/STLS).
    /// Must be called with the read buffer fully drained, i.e. right after
    /// the server's OK reply to the upgrade command was consumed.
    pub fn upgrade_tls(&mut self, host: &str) -> io::Result<()> {
        let taken = std::mem::replace(&mut self.stream, MailStream::Closed);
        let upgraded = match taken {
            MailStream::Plain(tcp) => MailStream::Tls(tls_wrap(tcp, host)?),
            other => other,
        };
        self.stream = upgraded;
        self.len = 0;
        self.pos = 0;
        Ok(())
    }

    fn fill(&mut self) -> io::Result<usize> {
        if self.pos < self.len {
            return Ok(self.len - self.pos);
        }
        self.pos = 0;
        self.len = 0;
        let n = self.stream.read(&mut self.buf)?;
        self.len = n;
        Ok(n)
    }

    /// Read one CRLF/LF terminated line, returned without the terminator.
    /// Errors on EOF.
    pub fn read_line(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            if self.fill()? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed",
                ));
            }
            let mut done = false;
            while self.pos < self.len {
                let b = self.buf[self.pos];
                self.pos += 1;
                if b == b'\n' {
                    done = true;
                    break;
                }
                line.push(b);
            }
            if done {
                break;
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(line)
    }

    /// Read exactly `n` raw bytes (IMAP literal payload).
    pub fn read_exact_n(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            if self.fill()? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-literal",
                ));
            }
            let available = self.len - self.pos;
            let take = available.min(n - out.len());
            out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
        }
        Ok(out)
    }

    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()
    }

    pub fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }
}
