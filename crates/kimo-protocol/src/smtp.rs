//! A minimal SMTP (RFC 5321) submission client: EHLO, STARTTLS, `AUTH PLAIN`
//! or `AUTH LOGIN`, and one message per `MAIL FROM` / `RCPT TO` / `DATA`
//! session. Synchronous like the other two clients, so it runs inside sync
//! worker threads and never on the UI thread.
//!
//! There is no `HELO` fallback and no cleartext authentication: a server that
//! cannot do ESMTP cannot authenticate a submission, and saying so is better
//! than sending a password it will not understand.

use base64::Engine as _;

use super::stream::{connect, Conn};
use kimo_core::error::{MailError, MailResult};
use kimo_core::models::TlsMode;

/// How much of one reply is kept before it is treated as a server that will not
/// stop talking. A capability list is a few hundred bytes; this is a backstop
/// against a reply that never ends, not a limit anybody meets.
const MAX_REPLY_BYTES: usize = 8 * 1024;

pub struct SmtpClient {
    conn: Conn,
    /// The authentication mechanisms EHLO named, uppercase; empty when it named
    /// none, which is a server that wants no login at all. The rest of the
    /// capability list is parsed and dropped: nothing this client does depends
    /// on it.
    auth: Vec<String>,
}

impl SmtpClient {
    /// Connect to `host:port` and say hello.
    ///
    /// `StartTls` is the account's decision, and it is taken seriously: the
    /// session is upgraded before anything else is written, and a server that
    /// will not upgrade is refused here rather than after the password.
    pub fn connect(host: &str, port: u16, tls: TlsMode) -> MailResult<Self> {
        let stream = connect(host, port, tls)?;
        let mut client = SmtpClient {
            conn: Conn::new(stream),
            auth: Vec::new(),
        };
        let (code, lines) = client.read_reply()?;
        check(code, &lines.join(" "), &[220], "the server's greeting")?;
        if tls == TlsMode::StartTls {
            client.command("STARTTLS", &[220], "STARTTLS")?;
            client.conn.upgrade_tls(host)?;
        }
        let lines = client.command_lines("EHLO localhost", &[250], "EHLO")?;
        client.auth = auth_mechanisms(&lines);
        Ok(client)
    }

    /// Log in, when the server offers a mechanism we speak.
    ///
    /// A server that advertises no `AUTH` at all is left unauthenticated — a
    /// local relay — and one that offers only mechanisms this client does not
    /// speak is refused by name. Every refusal from here is an
    /// `MailError::Auth`: the credentials are what the server said no to.
    pub fn login(&mut self, user: &str, pass: &str) -> MailResult<()> {
        if user.contains(['\r', '\n']) || pass.contains(['\r', '\n']) {
            return Err(MailError::Auth("credentials must not contain CR/LF".into()));
        }
        if self.auth.is_empty() {
            return Ok(());
        }
        let result = if self.auth.iter().any(|mechanism| mechanism == "PLAIN") {
            // The initial-response form: one line, the NUL-separated pair.
            let token =
                base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"));
            self.command(&format!("AUTH PLAIN {token}"), &[235], "AUTH PLAIN")
        } else if self.auth.iter().any(|mechanism| mechanism == "LOGIN") {
            self.auth_login(user, pass)
        } else {
            return Err(MailError::Auth(format!(
                "the server offers no authentication this client speaks (it offers {})",
                self.auth.join(", ")
            )));
        };
        result.map(|_| ()).map_err(|error| match error {
            MailError::Refused(message) | MailError::Protocol(message) => MailError::Auth(message),
            other => other,
        })
    }

    /// `AUTH LOGIN`: the username and then the password, each base64, each
    /// answered by a `334` challenge.
    fn auth_login(&mut self, user: &str, pass: &str) -> MailResult<String> {
        self.command("AUTH LOGIN", &[334], "AUTH LOGIN")?;
        let user = base64::engine::general_purpose::STANDARD.encode(user.as_bytes());
        self.command(&user, &[334], "AUTH LOGIN (the username)")?;
        let pass = base64::engine::general_purpose::STANDARD.encode(pass.as_bytes());
        self.command(&pass, &[235], "AUTH LOGIN (the password)")
    }

    /// One message: the envelope first, then the bytes.
    ///
    /// A recipient the server refuses fails the send — no `DATA` is sent, and
    /// the address and the server's own words are in the error — because
    /// delivering to some of the people somebody wrote to, quietly, is worse
    /// than not sending at all.
    pub fn send(&mut self, from: &str, to: &[String], raw: &[u8]) -> MailResult<()> {
        self.command(
            &format!("MAIL FROM:<{from}>"),
            &[250],
            &format!("MAIL FROM:<{from}>"),
        )?;
        for addr in to {
            self.command(
                &format!("RCPT TO:<{addr}>"),
                &[250, 251],
                &format!("RCPT TO:<{addr}>"),
            )?;
        }
        self.command("DATA", &[354], "DATA")?;
        self.conn.write_raw(&stuffed(raw))?;
        let (code, lines) = self.read_reply()?;
        check(
            code,
            &lines.join(" "),
            &[250],
            "the server rejected the message",
        )
        .map(|_| ())
    }

    /// End the session politely. The answer is not read for its opinion: the
    /// mail is either accepted by now or it never will be.
    pub fn quit(&mut self) {
        let _ = self.conn.write_line("QUIT");
        let _ = self.read_reply();
    }

    /// Send one command and check its reply.
    fn command(&mut self, line: &str, ok: &[u16], what: &str) -> MailResult<String> {
        self.conn.write_line(line)?;
        let (code, lines) = self.read_reply()?;
        check(code, &lines.join(" "), ok, what)
    }

    /// The same, keeping the reply's lines apart: only EHLO cares, and only
    /// because its capabilities are one per line.
    fn command_lines(&mut self, line: &str, ok: &[u16], what: &str) -> MailResult<Vec<String>> {
        self.conn.write_line(line)?;
        let (code, lines) = self.read_reply()?;
        check(code, &lines.join(" "), ok, what)?;
        Ok(lines)
    }

    /// One SMTP reply: its code and the text of its lines. `250-one` continues
    /// a reply and `250 two` — or a bare `250` — ends it; the lines are kept
    /// apart because an EHLO reply's capabilities are one per line.
    fn read_reply(&mut self) -> MailResult<(u16, Vec<String>)> {
        let mut lines = Vec::new();
        let mut total = 0_usize;
        loop {
            let line = String::from_utf8_lossy(&self.conn.read_line()?).into_owned();
            let digits = line
                .get(..3)
                .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()));
            let Some(digits) = digits else {
                return Err(MailError::Protocol(format!("not an SMTP reply: {line:?}")));
            };
            let code: u16 = digits.parse().unwrap_or(0);
            let rest = line[3..].trim_start_matches(['-', ' ']).trim();
            total += rest.len();
            if total > MAX_REPLY_BYTES {
                return Err(MailError::Protocol("the reply never ended".into()));
            }
            lines.push(rest.to_string());
            if !line[3..].starts_with('-') {
                return Ok((code, lines));
            }
        }
    }
}

/// One reply, checked against the codes this step expects.
///
/// A 4xx or a 5xx is a `Refused` — the server read the command and said no, and
/// its words are kept, because "550 no such user here" is the whole answer to
/// why a mail did not go out. Anything else is a reply this client cannot make
/// sense of.
fn check(code: u16, text: &str, ok: &[u16], what: &str) -> MailResult<String> {
    if ok.contains(&code) {
        return Ok(text.to_string());
    }
    let detail = if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    };
    match code {
        400..=599 => Err(MailError::Refused(format!("{what} — {code}{detail}"))),
        _ => Err(MailError::Protocol(format!(
            "{what} — unexpected reply {code}{detail}"
        ))),
    }
}

/// The authentication mechanisms an EHLO reply names, uppercase.
///
/// Both spellings RFC 4954 inherits are read — `AUTH PLAIN LOGIN` and the old
/// `AUTH=PLAIN` — and the mechanisms are the words after the keyword *on its
/// line*: what follows on the next line is another capability, not another
/// mechanism, which is the mistake that turns `SIZE` into a login method.
pub fn auth_mechanisms(lines: &[String]) -> Vec<String> {
    let mut mechanisms = Vec::new();
    for line in lines.iter().skip(1) {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else {
            continue;
        };
        let keyword = first.to_uppercase();
        if keyword == "AUTH" {
            mechanisms.extend(words.map(|word| word.to_uppercase()));
        } else if let Some(rest) = keyword.strip_prefix("AUTH=") {
            mechanisms.extend(rest.split(',').map(str::to_string));
            mechanisms.extend(words.map(|word| word.to_uppercase()));
        }
    }
    mechanisms
}

/// The message as DATA needs it: CRLF line endings throughout, every line that
/// starts with a dot doubled — that is the transport's own escape for the dot
/// that ends the message — and the terminating line itself.
///
/// The body is base64 by the time it gets here, so no line of it can start with
/// a dot; the stuffing is still done, because this is the transport's rule and
/// nothing above it should have to know that.
fn stuffed(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 32 + 8);
    let mut line_start = true;
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        if line_start && byte == b'.' {
            out.push(b'.');
        }
        match byte {
            b'\r' => {
                out.push(b'\r');
                out.push(b'\n');
                if raw.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                line_start = true;
            }
            b'\n' => {
                out.push(b'\r');
                out.push(b'\n');
                line_start = true;
            }
            other => {
                out.push(other);
                line_start = false;
            }
        }
        index += 1;
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};

    /// What the canned server saw: every command line, and the body of the
    /// message when a DATA phase happened.
    struct Seen {
        commands: Vec<String>,
        message: Option<String>,
    }

    /// A canned SMTP server on a loopback port: greet, then answer each
    /// (command, response) pair, asserting the command is exactly what the test
    /// expected — a `MAIL FROM` without its angle brackets, or a `RCPT` for the
    /// wrong address, is a mail that never arrives.
    ///
    /// `DATA` is the one command the server answers *and* reads more after: the
    /// reply that says "go ahead" is followed by the message itself, up to the
    /// line that is a dot on its own. An entry whose expected command starts
    /// with `@` sends its response without reading a command, which is how a
    /// script answers that dot.
    fn one_shot_server(script: Vec<(String, String)>) -> (u16, std::thread::JoinHandle<Seen>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"220 canned ready\r\n").unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut seen = Seen {
                commands: Vec::new(),
                message: None,
            };
            for (expected, response) in script {
                if expected.starts_with('@') {
                    socket.write_all(response.as_bytes()).unwrap();
                    continue;
                }
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let line = line.trim_end().to_string();
                assert_eq!(line, expected, "the client sent a different command");
                socket.write_all(response.as_bytes()).unwrap();
                seen.commands.push(line.clone());
                if line == "DATA" {
                    let mut body = String::new();
                    loop {
                        let mut body_line = String::new();
                        if reader.read_line(&mut body_line).unwrap() == 0 {
                            break;
                        }
                        if body_line == ".\r\n" || body_line == ".\n" {
                            break;
                        }
                        body.push_str(&body_line);
                    }
                    seen.message = Some(body);
                }
            }
            seen
        });
        (port, server)
    }

    /// The script of a send that goes through: EHLO with two mechanisms, an
    /// AUTH PLAIN, the envelope, the message.
    fn ok_script() -> Vec<(String, String)> {
        let token = base64::engine::general_purpose::STANDARD.encode("\0me\0secret");
        vec![
            (
                "EHLO localhost".to_string(),
                "250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250-SIZE 35882577\r\n250 HELP\r\n"
                    .to_string(),
            ),
            (
                format!("AUTH PLAIN {token}"),
                "235 2.7.0 accepted\r\n".to_string(),
            ),
            (
                "MAIL FROM:<ada@example.com>".to_string(),
                "250 2.1.0 sender ok\r\n".to_string(),
            ),
            (
                "RCPT TO:<bob@example.com>".to_string(),
                "250 2.1.5 recipient ok\r\n".to_string(),
            ),
            ("DATA".to_string(), "354 go ahead\r\n".to_string()),
            (
                "@the reply to the message".to_string(),
                "250 2.0.0 queued as 12345\r\n".to_string(),
            ),
            ("QUIT".to_string(), "221 2.0.0 bye\r\n".to_string()),
        ]
    }

    const MESSAGE: &[u8] =
        b"From: ada@example.com\r\nTo: bob@example.com\r\nSubject: Hi\r\n\r\nbody\r\n.a dotted line\r\n";

    #[test]
    fn a_message_goes_out_with_its_envelope_and_its_body() {
        let (port, server) = one_shot_server(ok_script());
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        client.login("me", "secret").unwrap();
        client
            .send("ada@example.com", &["bob@example.com".to_string()], MESSAGE)
            .unwrap();
        client.quit();

        let seen = server.join().unwrap();
        assert_eq!(seen.commands.len(), 6);
        // The transport's own escaping: the dot of a body line doubled, and
        // the terminator that is a line of its own.
        assert_eq!(
            seen.message.unwrap(),
            "From: ada@example.com\r\nTo: bob@example.com\r\nSubject: Hi\r\n\r\nbody\r\n..a dotted line\r\n"
        );
    }

    #[test]
    fn auth_login_is_used_when_plain_is_not_offered() {
        let user = base64::engine::general_purpose::STANDARD.encode("me");
        let pass = base64::engine::general_purpose::STANDARD.encode("secret");
        let script = vec![
            (
                "EHLO localhost".to_string(),
                "250-localhost\r\n250-AUTH LOGIN\r\n250 HELP\r\n".to_string(),
            ),
            ("AUTH LOGIN".to_string(), "334 VXNlcm5hbWU6\r\n".to_string()),
            (user, "334 UGFzc3dvcmQ6\r\n".to_string()),
            (pass, "235 2.7.0 accepted\r\n".to_string()),
            (
                "MAIL FROM:<ada@example.com>".to_string(),
                "250 2.1.0 sender ok\r\n".to_string(),
            ),
            (
                "RCPT TO:<bob@example.com>".to_string(),
                "250 2.1.5 recipient ok\r\n".to_string(),
            ),
            ("DATA".to_string(), "354 go ahead\r\n".to_string()),
            (
                "@the reply to the message".to_string(),
                "250 2.0.0 queued\r\n".to_string(),
            ),
        ];
        let (port, server) = one_shot_server(script);
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        client.login("me", "secret").unwrap();
        client
            .send("ada@example.com", &["bob@example.com".to_string()], MESSAGE)
            .unwrap();
        assert_eq!(server.join().unwrap().commands.len(), 7);
    }

    /// A server that advertises no AUTH wants no login — a local relay — and
    /// nothing is sent for one.
    #[test]
    fn a_server_that_asks_for_no_authentication_is_not_authenticated() {
        let script = vec![
            (
                "EHLO localhost".to_string(),
                "250-localhost\r\n250 SIZE 10240000\r\n".to_string(),
            ),
            (
                "MAIL FROM:<ada@example.com>".to_string(),
                "250 2.1.0 sender ok\r\n".to_string(),
            ),
            (
                "RCPT TO:<bob@example.com>".to_string(),
                "250 2.1.5 recipient ok\r\n".to_string(),
            ),
            ("DATA".to_string(), "354 go ahead\r\n".to_string()),
            (
                "@the reply to the message".to_string(),
                "250 2.0.0 queued\r\n".to_string(),
            ),
        ];
        let (port, server) = one_shot_server(script);
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        client.login("me", "secret").unwrap();
        client
            .send("ada@example.com", &["bob@example.com".to_string()], MESSAGE)
            .unwrap();
        let seen = server.join().unwrap();
        assert!(
            !seen
                .commands
                .iter()
                .any(|command| command.starts_with("AUTH")),
            "no AUTH was sent: {:?}",
            seen.commands
        );
    }

    /// A server that offers mechanisms this client does not speak is refused by
    /// name and sent nothing: the alternative is a password in a command the
    /// server will not understand.
    #[test]
    fn a_server_with_no_shared_mechanism_is_refused() {
        let script = vec![(
            "EHLO localhost".to_string(),
            "250-localhost\r\n250-AUTH CRAM-MD5 XOAUTH2\r\n250 HELP\r\n".to_string(),
        )];
        let (port, server) = one_shot_server(script);
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        let error = client.login("me", "secret").unwrap_err();
        assert!(matches!(error, MailError::Auth(_)), "{error:?}");
        assert!(error.to_string().contains("CRAM-MD5"), "{error}");
        assert_eq!(server.join().unwrap().commands.len(), 1);
    }

    #[test]
    fn a_refused_login_is_an_authentication_error() {
        let token = base64::engine::general_purpose::STANDARD.encode("\0me\0wrong");
        let script = vec![
            (
                "EHLO localhost".to_string(),
                "250-localhost\r\n250-AUTH PLAIN\r\n250 HELP\r\n".to_string(),
            ),
            (
                format!("AUTH PLAIN {token}"),
                "535 5.7.8 authentication failed\r\n".to_string(),
            ),
        ];
        let (port, server) = one_shot_server(script);
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        let error = client.login("me", "wrong").unwrap_err();
        assert!(matches!(error, MailError::Auth(_)), "{error:?}");
        assert!(error.to_string().contains("535"), "{error}");
        server.join().unwrap();
    }

    /// One refused recipient stops the send: no DATA goes out, and the error
    /// names the address the server would not take.
    #[test]
    fn a_refused_recipient_stops_the_send() {
        let token = base64::engine::general_purpose::STANDARD.encode("\0me\0secret");
        let script = vec![
            (
                "EHLO localhost".to_string(),
                "250-localhost\r\n250-AUTH PLAIN\r\n250 HELP\r\n".to_string(),
            ),
            (format!("AUTH PLAIN {token}"), "235 ok\r\n".to_string()),
            (
                "MAIL FROM:<ada@example.com>".to_string(),
                "250 sender ok\r\n".to_string(),
            ),
            (
                "RCPT TO:<bob@example.com>".to_string(),
                "250 recipient ok\r\n".to_string(),
            ),
            (
                "RCPT TO:<nobody@example.com>".to_string(),
                "550 5.1.1 no such user\r\n".to_string(),
            ),
        ];
        let (port, server) = one_shot_server(script);
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        client.login("me", "secret").unwrap();
        let error = client
            .send(
                "ada@example.com",
                &[
                    "bob@example.com".to_string(),
                    "nobody@example.com".to_string(),
                ],
                MESSAGE,
            )
            .unwrap_err();
        assert!(error.to_string().contains("nobody@example.com"), "{error}");
        assert!(error.to_string().contains("no such user"), "{error}");
        let seen = server.join().unwrap();
        assert!(!seen.commands.iter().any(|command| command == "DATA"));
    }

    /// The server took the whole message and then refused it: that is a
    /// refusal with the server's own reason, not a lost connection.
    #[test]
    fn a_message_refused_after_the_body_is_a_refusal() {
        let mut script = ok_script();
        script.retain(|(expected, _)| expected != "QUIT");
        for (expected, response) in script.iter_mut() {
            if expected.starts_with('@') {
                *response = "554 5.7.1 message rejected\r\n".to_string();
            }
        }
        let (port, server) = one_shot_server(script);
        let mut client = SmtpClient::connect("127.0.0.1", port, TlsMode::Plain).unwrap();
        client.login("me", "secret").unwrap();
        let error = client
            .send("ada@example.com", &["bob@example.com".to_string()], MESSAGE)
            .unwrap_err();
        assert!(matches!(error, MailError::Refused(_)), "{error:?}");
        assert!(error.to_string().contains("554"), "{error}");
        assert!(error.to_string().contains("message rejected"), "{error}");
        server.join().unwrap();
    }

    /// A greeting that is not a 220 is the server saying it will not talk, and
    /// nothing is sent after it.
    #[test]
    fn a_greeting_that_refuses_the_session_ends_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"554 no service here\r\n").unwrap();
        });
        let error = match SmtpClient::connect("127.0.0.1", port, TlsMode::Plain) {
            Ok(_) => panic!("a 554 greeting is not a session"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no service here"), "{error}");
        server.join().unwrap();
    }

    /// STARTTLS is the first thing sent after the greeting, before anything —
    /// least of all a password — goes out in the clear. (The handshake itself
    /// fails against this server, which is what the test is checking the order
    /// of.)
    #[test]
    fn starttls_is_asked_for_before_anything_else() {
        let script = vec![(
            "STARTTLS".to_string(),
            "220 2.0.0 ready to start TLS\r\n".to_string(),
        )];
        let (port, server) = one_shot_server(script);
        assert!(SmtpClient::connect("127.0.0.1", port, TlsMode::StartTls).is_err());
        let seen = server.join().unwrap();
        assert_eq!(seen.commands, ["STARTTLS"]);
    }

    #[test]
    fn mechanisms_are_read_the_way_servers_spell_them() {
        let lines = |reply: &[&str]| {
            reply
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        // The first line is the server's own name, and the keywords after it
        // are one per line: `SIZE`'s value is an argument, not a mechanism.
        let reply = lines(&[
            "mail.example.com at your service",
            "AUTH PLAIN LOGIN",
            "SIZE 35882577",
            "STARTTLS",
            "HELP",
        ]);
        assert_eq!(auth_mechanisms(&reply), ["PLAIN", "LOGIN"]);
        // The older spelling, and a line that names no mechanism at all.
        assert_eq!(
            auth_mechanisms(&lines(&["name", "AUTH=CRAM-MD5", "AUTH"])),
            ["CRAM-MD5"]
        );
        assert_eq!(
            auth_mechanisms(&lines(&["name", "AUTH XOAUTH2"])),
            ["XOAUTH2"]
        );
        assert!(auth_mechanisms(&lines(&["name", "SIZE 100"])).is_empty());
    }

    #[test]
    fn a_message_is_stuffed_and_terminated() {
        assert_eq!(stuffed(b"a\r\n.\r\nb"), b"a\r\n..\r\nb\r\n.\r\n".to_vec());
        // A lone LF (or CR) is a line ending too, and a dot right after one is
        // still a line of its own.
        assert_eq!(stuffed(b"a\n.b"), b"a\r\n..b\r\n.\r\n".to_vec());
        assert_eq!(stuffed(b"a\r.b"), b"a\r\n..b\r\n.\r\n".to_vec());
        // The terminator is added once, whatever the message ended with.
        assert_eq!(stuffed(b"body\r\n"), b"body\r\n.\r\n".to_vec());
    }
}
