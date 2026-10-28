//! Gmail SMTP submission over the host's TLS stream, with XOAUTH2 as the
//! only authentication — an app password belongs to the IMAP `LOGIN` path,
//! and a submission that could silently fall back to it would be a surprise,
//! not a fallback.
//!
//! The whole exchange is one connection, one message, no retry: hard rule 4
//! (nothing is queued) applies to a plugin exactly as it does to the built-in
//! client, and "ambiguous after DATA" is reported as ambiguous, never
//! re-sent.

use crate::abi::NetStream;
use crate::oauth;

pub const HOST: &str = "smtp.gmail.com";
pub const PORT: u16 = 465;

pub struct Rejected {
    /// Whether the failure is known to have happened *after* the message was
    /// accepted: `true` is "it may be sent, do not try again".
    pub ambiguous: bool,
    pub message: String,
}

/// Submit `raw` (RFC 5322 bytes) from `envelope_from` to each of
/// `recipients`, dot-stuffed per RFC 5321 §4.5.2.
pub fn submit(
    user: &str,
    token: &str,
    envelope_from: &str,
    recipients: &[String],
    raw: &[u8],
) -> Result<(), Rejected> {
    let connect = || -> Result<Client, String> {
        let stream = NetStream::open(HOST, PORT, true)?;
        let mut client = Client {
            stream,
            buffer: Vec::new(),
        };
        client.expect_reply(220)?;
        client.command("EHLO kimo", 250)?;
        Ok(client)
    };
    let mut client = connect().map_err(|message| Rejected {
        ambiguous: false,
        message,
    })?;

    let blob = crate::oauth::base64_std(oauth::xoauth2(user, token).as_bytes());
    client
        .command(&format!("AUTH XOAUTH2 {blob}"), 235)
        .map_err(|message| Rejected {
            ambiguous: false,
            message: format!("authentication refused: {message}"),
        })?;

    client
        .command(&format!("MAIL FROM:<{envelope_from}>"), 250)
        .map_err(|message| Rejected {
            ambiguous: false,
            message,
        })?;
    for recipient in recipients {
        client
            .command(&format!("RCPT TO:<{recipient}>"), 250)
            .map_err(|message| Rejected {
                ambiguous: false,
                message: format!("{recipient}: {message}"),
            })?;
    }
    client.command("DATA", 354).map_err(|message| Rejected {
        ambiguous: false,
        message,
    })?;

    // Everything past this point may already have delivered the message.
    let stuffed = dot_stuffed(raw);
    let send = client
        .write_bytes(&stuffed)
        .and_then(|()| client.command("\r\n.", 250));
    if send.is_err() {
        return Err(Rejected {
            ambiguous: true,
            message: "the connection was lost after the message was sent — it may \
                      have been delivered"
                .to_string(),
        });
    }
    let _ = client.command("QUIT", 221);
    client.stream.close();
    Ok(())
}

struct Client {
    stream: NetStream,
    buffer: Vec<u8>,
}

impl Client {
    fn command(&mut self, line: &str, expected: u16) -> Result<String, String> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.stream.write(&bytes)?;
        self.expect_reply(expected)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream.write(bytes)
    }

    fn expect_reply(&mut self, expected: u16) -> Result<String, String> {
        loop {
            let line = self.read_line()?;
            // A reply's last line has a space after the code; a dash means
            // more lines follow. Gmail's banner and EHLO answer are multiline.
            if line.len() >= 4 && line.as_bytes()[3] == b'-' {
                continue;
            }
            let code = line.get(..3).and_then(|digits| digits.parse::<u16>().ok());
            let text = line.get(4..).unwrap_or_default().to_string();
            return match code {
                Some(code) if code == expected => Ok(text),
                Some(code) => Err(format!("{code} {text}")),
                None => Err(format!("the server said: {line}")),
            };
        }
    }

    fn read_line(&mut self) -> Result<String, String> {
        loop {
            if let Some(line) = crate::wire::take_line(&mut self.buffer) {
                return Ok(line);
            }
            let mut chunk = [0u8; 4096];
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                return Err("the server closed the connection".to_string());
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

impl Drop for Client {
    /// The host stream lives in a table keyed by its id until it is closed;
    /// a submission that finished or failed must not leave the socket there.
    fn drop(&mut self) {
        self.stream.close();
    }
}

/// Dot-stuffing: a line that starts with `.` gains a second one, and the
/// message always ends with CRLF so the terminating `.` stands alone.
pub fn dot_stuffed(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 8);
    let mut at_line_start = true;
    for &byte in raw {
        if at_line_start && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        at_line_start = byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_at_a_line_start_is_doubled() {
        assert_eq!(
            dot_stuffed(b".hidden\r\nplain\n.dots"),
            b"..hidden\r\nplain\n..dots\r\n"
        );
    }

    #[test]
    fn a_message_without_a_final_crlf_gets_one() {
        assert_eq!(dot_stuffed(b"body"), b"body\r\n");
        assert_eq!(dot_stuffed(b"body\r\n"), b"body\r\n");
    }
}
