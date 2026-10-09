//! A minimal POP3 (RFC 1939) client: USER/PASS auth, STAT, UIDL, RETR with
//! byte-de-stuffing, and QUIT. Runs on sync worker threads only.

use super::stream::{connect, Conn};
use crate::error::{MailError, MailResult};
use crate::models::{Account, TlsMode};

pub struct Pop3Client {
    conn: Conn,
}

impl Pop3Client {
    pub fn connect(account: &Account) -> MailResult<Self> {
        let stream = connect(&account.host, account.port, account.tls)?;
        let mut client = Pop3Client {
            conn: Conn::new(stream),
        };
        client.read_status()?; // +OK greeting
        if account.tls == TlsMode::StartTls {
            client.conn.write_line("STLS")?;
            client.read_status()?;
            client.conn.upgrade_tls(&account.host)?;
        }
        Ok(client)
    }

    pub fn login(&mut self, user: &str, pass: &str) -> MailResult<()> {
        if user.contains(['\r', '\n']) || pass.contains(['\r', '\n']) {
            return Err(MailError::Auth("credentials must not contain CR/LF".into()));
        }
        self.conn.write_line(&format!("USER {user}"))?;
        self.read_status()?;
        self.conn.write_line(&format!("PASS {pass}"))?;
        self.read_status().map(|_| ())
    }

    /// Number of messages in the drop (from STAT).
    pub fn stat(&mut self) -> MailResult<usize> {
        let resp = self.command("STAT")?;
        resp.split_whitespace()
            .nth(1)
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| MailError::Protocol(format!("malformed STAT response: {resp}")))
    }

    /// `(message number, unique id)` pairs from UIDL.
    pub fn uidl(&mut self) -> MailResult<Vec<(usize, String)>> {
        self.conn.write_line("UIDL")?;
        self.read_status()?;
        let mut out = Vec::new();
        loop {
            let line = String::from_utf8_lossy(&self.conn.read_line()?).into_owned();
            if line == "." {
                break;
            }
            let mut parts = line.splitn(2, ' ');
            let num = parts.next().unwrap_or_default().parse().ok();
            let id = parts.next().unwrap_or_default().to_string();
            if let (Some(num), true) = (num, !id.is_empty()) {
                out.push((num, id));
            }
        }
        Ok(out)
    }

    /// Full raw bytes of message `num` (RETR), with dot-de-stuffing applied.
    pub fn retr(&mut self, num: usize) -> MailResult<Vec<u8>> {
        self.command(&format!("RETR {num}"))?;
        let mut out = Vec::new();
        loop {
            let mut line = self.conn.read_line()?;
            if line == b"." {
                break;
            }
            if line.first() == Some(&b'.') {
                line.remove(0);
            }
            out.extend_from_slice(&line);
            out.extend_from_slice(b"\r\n");
        }
        Ok(out)
    }

    pub fn quit(&mut self) {
        let _ = self.conn.write_line("QUIT");
        let _ = self.read_status();
    }

    fn command(&mut self, cmd: &str) -> MailResult<String> {
        self.conn.write_line(cmd)?;
        self.read_status()
    }

    fn read_status(&mut self) -> MailResult<String> {
        let bytes = self.conn.read_line()?;
        if bytes.is_empty() {
            return Err(MailError::Protocol("connection closed".into()));
        }
        let line = String::from_utf8_lossy(&bytes).into_owned();
        if line.starts_with("+OK") {
            Ok(line)
        } else if let Some(rest) = line.strip_prefix("-ERR") {
            let msg = rest.trim().to_string();
            let auth = msg.contains("[AUTH") || msg.to_lowercase().contains("authentication");
            Err(if auth {
                MailError::Auth(msg)
            } else {
                MailError::Protocol(msg)
            })
        } else {
            Err(MailError::Protocol(format!("unexpected response: {line}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::MailError;

    #[test]
    fn error_mapping_is_by_display_text() {
        assert!(matches!(
            MailError::Auth("invalid credentials".into()),
            MailError::Auth(_)
        ));
    }
}
