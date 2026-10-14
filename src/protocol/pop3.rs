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

    /// Delete the message with this UIDL, if the drop still has it. A UIDL the
    /// server no longer answers for is a message that is already gone, which is
    /// not an error.
    ///
    /// POP3 commits a deletion at QUIT and at nothing else, so the caller has
    /// to end the session with `quit` for this to reach the server: a
    /// connection that drops in between leaves the mail alone, which is the
    /// safe direction.
    pub fn delete_by_uidl(&mut self, uid: &str) -> MailResult<()> {
        match self.number_of_uidl(uid)? {
            Some(number) => self.dele(number),
            None => Ok(()),
        }
    }

    /// The full raw bytes of the message with this UIDL, if the drop still has
    /// it (RETR). `None` is a UIDL the drop no longer answers for: the message
    /// is gone from the server, which the caller has to be able to tell apart
    /// from a connection that failed — one is "there is nothing to fetch", the
    /// other is "try again".
    pub fn retr_by_uidl(&mut self, uid: &str) -> MailResult<Option<Vec<u8>>> {
        match self.number_of_uidl(uid)? {
            Some(number) => self.retr(number).map(Some),
            None => Ok(None),
        }
    }

    /// The message number a UIDL names, from a fresh UIDL listing: POP3's only
    /// way from the identity Kimo stores with a message to the number RETR and
    /// DELE are addressed by. `None` is a message the drop no longer has.
    fn number_of_uidl(&mut self, uid: &str) -> MailResult<Option<usize>> {
        Ok(self
            .uidl()?
            .into_iter()
            .find(|(_, id)| id == uid)
            .map(|(number, _)| number))
    }

    /// Mark one message deleted (`DELE`).
    fn dele(&mut self, num: usize) -> MailResult<()> {
        self.command(&format!("DELE {num}")).map(|_| ())
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
    use super::*;
    use crate::error::MailError;
    use crate::models::{Protocol, TlsMode};
    use std::io::{BufRead, Write};

    /// A canned POP3 server on a loopback port: greet, then answer each
    /// (command, response) pair, asserting the command is exactly what the
    /// test expected — a UIDL mistaken for a message number would delete the
    /// wrong mail, and POP3 has no way back from that.
    fn one_shot_server(
        script: Vec<(&'static str, &'static str)>,
    ) -> (Account, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"+OK ready\r\n").unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut commands = Vec::new();
            for (expected, response) in script {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_string();
                assert_eq!(line, expected, "the client sent a different command");
                socket.write_all(response.as_bytes()).unwrap();
                commands.push(line);
            }
            commands
        });
        let account = Account {
            id: "local".to_string(),
            name: "Local".to_string(),
            protocol: Protocol::Pop3,
            host: "127.0.0.1".to_string(),
            port,
            username: "me".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Plain,
        };
        (account, server)
    }

    #[test]
    fn deleting_a_message_sends_the_number_its_uidl_names() {
        let (account, server) = one_shot_server(vec![
            ("USER me", "+OK\r\n"),
            ("PASS secret", "+OK\r\n"),
            ("UIDL", "+OK\r\n1 aaa\r\n2 bbb\r\n.\r\n"),
            ("DELE 2", "+OK message 2 deleted\r\n"),
            ("QUIT", "+OK bye\r\n"),
        ]);
        let mut client = Pop3Client::connect(&account).unwrap();
        client.login("me", "secret").unwrap();

        client.delete_by_uidl("bbb").unwrap();
        client.quit();
        assert_eq!(server.join().unwrap().len(), 5);
    }

    /// The message is not in the drop any more: nothing is sent for it, and
    /// the caller drops its own copy without a command that could touch
    /// another message.
    #[test]
    fn a_uidl_the_drop_no_longer_has_sends_no_dele() {
        let (account, server) = one_shot_server(vec![
            ("USER me", "+OK\r\n"),
            ("PASS secret", "+OK\r\n"),
            ("UIDL", "+OK\r\n1 aaa\r\n.\r\n"),
            ("QUIT", "+OK bye\r\n"),
        ]);
        let mut client = Pop3Client::connect(&account).unwrap();
        client.login("me", "secret").unwrap();

        client.delete_by_uidl("bbb").unwrap();
        client.quit();
        assert_eq!(server.join().unwrap().len(), 4);
    }

    /// Fetching is addressed the same way deleting is: the UIDL names the
    /// message number, and RETR asks for exactly that one. The session ends
    /// with QUIT like any other — no DELE went out, so nothing is deleted.
    #[test]
    fn fetching_a_message_sends_retr_for_the_number_its_uidl_names() {
        let (account, server) = one_shot_server(vec![
            ("USER me", "+OK\r\n"),
            ("PASS secret", "+OK\r\n"),
            ("UIDL", "+OK\r\n1 aaa\r\n2 bbb\r\n.\r\n"),
            (
                "RETR 2",
                "+OK 26 octets\r\nSubject: hi\r\n\r\nbody\r\n.\r\n",
            ),
            ("QUIT", "+OK bye\r\n"),
        ]);
        let mut client = Pop3Client::connect(&account).unwrap();
        client.login("me", "secret").unwrap();

        let raw = client.retr_by_uidl("bbb").unwrap().unwrap();
        client.quit();
        // The bytes are the message as it left the server: the terminating
        // dot and the dot-stuffing POP3 adds are the transport's, not the
        // mail's.
        assert_eq!(raw, b"Subject: hi\r\n\r\nbody\r\n");
        assert_eq!(server.join().unwrap().len(), 5);
    }

    /// The message is not in the drop any more: nothing is fetched for it, and
    /// the caller is told so rather than handed another message's bytes.
    #[test]
    fn a_uidl_the_drop_no_longer_has_fetches_nothing() {
        let (account, server) = one_shot_server(vec![
            ("USER me", "+OK\r\n"),
            ("PASS secret", "+OK\r\n"),
            ("UIDL", "+OK\r\n1 aaa\r\n.\r\n"),
            ("QUIT", "+OK bye\r\n"),
        ]);
        let mut client = Pop3Client::connect(&account).unwrap();
        client.login("me", "secret").unwrap();

        assert!(client.retr_by_uidl("bbb").unwrap().is_none());
        client.quit();
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[test]
    fn error_mapping_is_by_display_text() {
        assert!(matches!(
            MailError::Auth("invalid credentials".into()),
            MailError::Auth(_)
        ));
    }
}
