//! A minimal IMAP4rev1 client: just enough to log in, list folders, select a
//! mailbox, fetch messages (with literals) and set flags. Everything is
//! synchronous — it runs inside sync worker threads, never on the UI thread.

use super::stream::{connect, Conn};
use crate::error::{MailError, MailResult};
use crate::models::{Account, TlsMode};

pub struct MailboxInfo {
    pub exists: u32,
    pub uidvalidity: Option<u32>,
}

pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub raw: Vec<u8>,
}

pub struct ImapClient {
    conn: Conn,
    tag: u32,
}

impl ImapClient {
    pub fn connect(account: &Account) -> MailResult<Self> {
        let stream = connect(&account.host, account.port, account.tls)?;
        let mut client = ImapClient {
            conn: Conn::new(stream),
            tag: 0,
        };
        let greeting = client.read_text_line()?;
        if !greeting.starts_with("* OK") {
            return Err(MailError::Protocol(format!(
                "unexpected IMAP greeting: {greeting}"
            )));
        }
        if account.tls == TlsMode::StartTls {
            client.run("STARTTLS")?;
            client.conn.upgrade_tls(&account.host)?;
        }
        Ok(client)
    }

    pub fn login(&mut self, user: &str, pass: &str) -> MailResult<()> {
        // Non-ASCII passwords cannot be quoted; send them as a literal.
        if pass.bytes().any(|b| b >= 0x80) || pass.contains('\r') || pass.contains('\n') {
            return self.login_with_literal(user, pass);
        }
        self.run(&format!(
            "LOGIN {} {}",
            quote_string(user),
            quote_string(pass)
        ))
        .map_err(|e| match e {
            MailError::Protocol(msg) => MailError::Auth(msg),
            other => other,
        })
        .map(|_| ())
    }

    fn login_with_literal(&mut self, user: &str, pass: &str) -> MailResult<()> {
        let tag = self.next_tag();
        self.conn.write_line(&format!(
            "{} LOGIN {} {{{}}}",
            tag,
            quote_string(user),
            pass.len()
        ))?;
        let cont = self.read_text_line()?;
        if !cont.starts_with('+') {
            return Err(MailError::Protocol(format!(
                "expected literal continuation, got: {cont}"
            )));
        }
        self.conn.write_raw(pass.as_bytes())?;
        self.conn.write_line("")?;
        let (final_line, _) = self.read_until_tagged(&tag)?;
        if !final_line.starts_with("OK") {
            return Err(MailError::Auth(final_line.trim().to_string()));
        }
        Ok(())
    }

    pub fn list_folders(&mut self) -> MailResult<Vec<String>> {
        let (_, untagged) = self.run("LIST \"\" \"*\"")?;
        let mut folders: Vec<String> = untagged
            .iter()
            .filter_map(|line| line.strip_prefix("* LIST "))
            .filter_map(parse_mailbox_name)
            .collect();
        folders.sort();
        folders.dedup();
        if let Some(pos) = folders.iter().position(|f| f.eq_ignore_ascii_case("INBOX")) {
            let inbox = folders.remove(pos);
            folders.insert(0, inbox);
        }
        Ok(folders)
    }

    pub fn select(&mut self, folder: &str) -> MailResult<MailboxInfo> {
        let (_, untagged) = self.run(&format!("SELECT {}", quote_string(folder)))?;
        let mut info = MailboxInfo {
            exists: 0,
            uidvalidity: None,
        };
        for line in &untagged {
            let Some(rest) = line.strip_prefix("* ") else {
                continue;
            };
            if let Some(n) = rest.strip_suffix(" EXISTS") {
                info.exists = n.trim().parse().unwrap_or(0);
            }
            if let Some(rest) = rest.strip_prefix("OK [UIDVALIDITY ") {
                if let Some(close) = rest.find(']') {
                    info.uidvalidity = rest[..close].trim().parse().ok();
                }
            }
        }
        Ok(info)
    }

    /// FETCH `set` (UID FLAGS BODY.PEEK[]) and return each message with its raw
    /// RFC 5322 bytes. `set` is a sequence set, or a UID set when `by_uid` —
    /// the caller knows which range it asked for. Handles `{n}` literal
    /// framing.
    pub fn fetch_bodies(&mut self, set: &str, by_uid: bool) -> MailResult<Vec<Fetched>> {
        let tag = self.next_tag();
        let command = if by_uid { "UID FETCH" } else { "FETCH" };
        self.conn
            .write_line(&format!("{tag} {command} {set} (UID FLAGS BODY.PEEK[])"))?;
        let mut out = Vec::new();
        loop {
            let line = self.read_text_line()?;
            if line.starts_with(&format!("{tag} ")) {
                let final_line = line[tag.len() + 1..].trim().to_string();
                if !final_line.starts_with("OK") {
                    return Err(MailError::Protocol(final_line));
                }
                break;
            }
            let Some(rest) = line.strip_prefix("* ") else {
                continue;
            };
            if !rest.contains(" FETCH ") {
                // EXISTS / RECENT / FLAGS / OK status lines etc.
                continue;
            }
            let mut uid = extract_uid(rest);
            let mut flags = extract_flags(rest);
            let Some(n) = literal_size(rest) else {
                continue;
            };
            let raw = self.conn.read_exact_n(n)?;
            let trailing = self.read_text_line()?;
            if let Some(u) = extract_uid(&trailing) {
                uid = Some(u);
            }
            let trailing_flags = extract_flags(&trailing);
            if !trailing_flags.is_empty() {
                flags = trailing_flags;
            }
            if let Some(uid) = uid {
                out.push(Fetched { uid, flags, raw });
            }
        }
        Ok(out)
    }

    /// Push a flag change back to the server, e.g. `+FLAGS.SILENT (\Seen)`.
    pub fn uid_store_flags_silent(&mut self, uid: u32, flag_command: &str) -> MailResult<()> {
        self.run(&format!("UID STORE {uid} {flag_command}"))
            .map(|_| ())
    }

    pub fn logout(&mut self) {
        let _ = self.run("LOGOUT");
    }

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("A{:04}", self.tag)
    }

    /// Send a command and collect untagged responses until the tagged reply.
    fn run(&mut self, command: &str) -> MailResult<(String, Vec<String>)> {
        let tag = self.next_tag();
        self.conn.write_line(&format!("{tag} {command}"))?;
        self.read_until_tagged(&tag)
    }

    fn read_until_tagged(&mut self, tag: &str) -> MailResult<(String, Vec<String>)> {
        let prefix = format!("{tag} ");
        let mut untagged = Vec::new();
        loop {
            let line = self.read_text_line()?;
            if let Some(rest) = line.strip_prefix(&prefix) {
                let rest = rest.trim();
                if rest.starts_with("OK") {
                    return Ok((rest.to_string(), untagged));
                }
                let msg = rest.to_string();
                return Err(if rest.starts_with("NO") {
                    MailError::Protocol(format!("server rejected command: {msg}"))
                } else {
                    MailError::Protocol(format!("BAD: {msg}"))
                });
            }
            untagged.push(line);
        }
    }

    fn read_text_line(&mut self) -> MailResult<String> {
        let bytes = self.conn.read_line()?;
        if bytes.is_empty() {
            return Err(MailError::Protocol("connection closed".into()));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Quote a string for IMAP, escaping `"` and `\`. Only valid for pure-ASCII
/// content without CR/LF (the literal path covers the rest).
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `123 FETCH (UID 45 …)` → `45`
fn extract_uid(s: &str) -> Option<u32> {
    let idx = s.find("UID ")? + 4;
    s[idx..].split([' ', ')']).next()?.parse().ok()
}

/// `… FLAGS (\Seen \Answered) …` → `["\Seen", "\Answered"]`
fn extract_flags(s: &str) -> Vec<String> {
    let Some(open) = s.find("FLAGS (") else {
        return Vec::new();
    };
    let rest = &s[open + 7..];
    let Some(close) = rest.find(')') else {
        return Vec::new();
    };
    rest[..close]
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Trailing `{123}` of a FETCH line → literal byte count.
fn literal_size(line: &str) -> Option<usize> {
    let line = line.trim_end();
    let open = line.rfind('{')?;
    if !line.ends_with('}') || open + 1 >= line.len() - 1 {
        return None;
    }
    line[open + 1..line.len() - 1].parse().ok()
}

/// Parse the mailbox name out of the tail of an untagged LIST response,
/// e.g. `(\HasNoChildren) "/" "Sent Items"` → `Sent Items`. The hierarchy
/// delimiter is itself quoted, so skip flags + delimiter before taking the
/// (optionally quoted) name.
fn parse_mailbox_name(rest: &str) -> Option<String> {
    let after_flags = rest.split_once(')')?.1.trim_start();
    let after_delim = if let Some(stripped) = after_flags.strip_prefix('"') {
        let delim_end = stripped.find('"')? + 1;
        &after_flags[delim_end + 1..]
    } else {
        let nil_len = after_flags.find(char::is_whitespace)?;
        &after_flags[nil_len..]
    };
    let name = after_delim.trim();
    if let Some(stripped) = name.strip_prefix('"') {
        let end = stripped.rfind('"')?;
        Some(stripped[..end].to_string())
    } else if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Decode a modified UTF-7 (RFC 3501 §5.1.3) mailbox name for display,
/// e.g. `&XfJT0ZAB-` → `已发送`. Non-ASCII runs are wrapped in `&`…`-`
/// around a base64-style alphabet (`,` = 63) encoding UTF-16BE; `&-` is a
/// literal `&`. This is display-only: every command sent to the server
/// (SELECT, UID STORE, …) keeps the encoded wire form.
///
/// Lenient by design — segments that fail to decode pass through unchanged.
pub fn decode_mailbox_name(name: &str) -> String {
    if !name.contains('&') {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        match rest.find('-') {
            Some(dash) => {
                let segment = &rest[..dash];
                rest = &rest[dash + 1..];
                if segment.is_empty() {
                    out.push('&'); // `&-` escapes a literal ampersand
                } else if let Some(decoded) = decode_mutf7_segment(segment) {
                    out.push_str(&decoded);
                } else {
                    out.push('&');
                    out.push_str(segment);
                    out.push('-');
                }
            }
            None => {
                // Unterminated shift — show the rest untouched.
                out.push('&');
                out.push_str(rest);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode one `&`…`-` segment (base64 alphabet with `,` = 63, UTF-16BE).
fn decode_mutf7_segment(segment: &str) -> Option<String> {
    let mut acc: u32 = 0;
    let mut bit_count = 0;
    let mut bytes: Vec<u8> = Vec::with_capacity(segment.len() * 3 / 4 + 1);
    for c in segment.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            ',' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            bytes.push(((acc >> bit_count) & 0xFF) as u8);
        }
    }
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None; // no character, or half a UTF-16 unit
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Protocol;
    use std::io::{BufRead, Write};

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(quote_string("plain"), "\"plain\"");
        assert_eq!(quote_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn mailbox_names_decode_from_modified_utf7() {
        // Real-world names from a Chinese IMAP server.
        assert_eq!(decode_mailbox_name("INBOX"), "INBOX");
        assert_eq!(decode_mailbox_name("&V4NXPpCuTvY-"), "垃圾邮件");
        assert_eq!(decode_mailbox_name("&XfJSIJZkkK5O9g-"), "已删除邮件");
        assert_eq!(decode_mailbox_name("&XfJT0ZAB-"), "已发送");
        assert_eq!(decode_mailbox_name("&g0l6Pw-"), "草稿");
        // Mixed ASCII/non-ASCII.
        assert_eq!(decode_mailbox_name("&XfJT0ZAB-/2024"), "已发送/2024");
        // `&-` escapes a literal ampersand.
        assert_eq!(decode_mailbox_name("A&-B"), "A&B");
        // `,` is base64 value 63: "&D,8-" encodes UTF-16BE 0x0FFF.
        assert_eq!(decode_mailbox_name("&D,8-"), "\u{0FFF}");
    }

    #[test]
    fn malformed_mutf7_passes_through_unchanged() {
        assert_eq!(decode_mailbox_name("&!!-"), "&!!-"); // not base64
        assert_eq!(decode_mailbox_name("&abc"), "&abc"); // unterminated
        assert_eq!(decode_mailbox_name("&A-"), "&A-"); // zero characters
        assert_eq!(decode_mailbox_name("&zz-"), "&zz-"); // half a UTF-16 unit
                                                         // Unpaired surrogate is not valid UTF-16 → pass through.
        assert_eq!(decode_mailbox_name("&2AA-"), "&2AA-");
    }

    #[test]
    fn uid_extraction() {
        assert_eq!(
            extract_uid("23 FETCH (UID 45 FLAGS (\\Seen) BODY[] {10}"),
            Some(45)
        );
        assert_eq!(extract_uid("23 FETCH (FLAGS (\\Seen) UID 7)"), Some(7));
        assert_eq!(extract_uid("23 FETCH (FLAGS (\\Seen))"), None);
    }

    #[test]
    fn flags_extraction() {
        assert_eq!(
            extract_flags("23 FETCH (UID 5 FLAGS (\\Seen \\Answered) BODY[] {9}"),
            vec!["\\Seen", "\\Answered"]
        );
        assert!(extract_flags("23 FETCH (UID 5)").is_empty());
    }

    #[test]
    fn literal_size_parsing() {
        assert_eq!(literal_size("23 FETCH (UID 5 BODY[] {2912}"), Some(2912));
        assert_eq!(literal_size("23 FETCH (UID 5)"), None);
        assert_eq!(literal_size("a {12"), None);
    }

    #[test]
    fn mailbox_name_parsing() {
        assert_eq!(
            parse_mailbox_name("(\\HasNoChildren) \"/\" \"Sent Items\""),
            Some("Sent Items".to_string())
        );
        assert_eq!(
            parse_mailbox_name("(\\HasNoChildren) \"/\" INBOX"),
            Some("INBOX".to_string())
        );
    }

    /// A canned IMAP server on a loopback port: greet, then answer each
    /// (command, response) pair, asserting the command is exactly what the test
    /// expected — a UID set sent as a sequence set is the kind of mistake that
    /// quietly fetches the wrong mail. `{tag}` in a response is replaced with
    /// the tag of the command it answers.
    fn one_shot_server(
        script: Vec<(&'static str, String)>,
    ) -> (Account, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"* OK ready\r\n").unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut commands = Vec::new();
            for (expected, response) in script {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_string();
                let (tag, command) = line.split_once(' ').expect("a tagged command");
                assert_eq!(command, expected, "the client sent a different command");
                socket
                    .write_all(response.replace("{tag}", tag).as_bytes())
                    .unwrap();
                commands.push(line);
            }
            commands
        });
        let account = Account {
            id: "local".to_string(),
            name: "Local".to_string(),
            protocol: Protocol::Imap,
            host: "127.0.0.1".to_string(),
            port,
            username: "me".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Plain,
        };
        (account, server)
    }

    #[test]
    fn the_fetch_command_matches_the_range_it_was_given() {
        let uid_response = concat!(
            "* 1 FETCH (UID 1813 FLAGS (\\Seen) BODY[] {11}\r\n",
            "hello world",
            ")\r\n",
            "{tag} OK FETCH completed\r\n",
        )
        .to_string();
        let sequence_response =
            "* 1 FETCH (UID 7 FLAGS ())\r\n{tag} OK FETCH completed\r\n".to_string();
        let (account, server) = one_shot_server(vec![
            ("UID FETCH 1813:* (UID FLAGS BODY.PEEK[])", uid_response),
            ("FETCH 1:50 (UID FLAGS BODY.PEEK[])", sequence_response),
        ]);
        let mut client = ImapClient::connect(&account).unwrap();

        let fetched = client.fetch_bodies("1813:*", true).unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].uid, 1813);
        assert_eq!(fetched[0].flags, ["\\Seen"]);
        assert_eq!(fetched[0].raw, b"hello world");

        // A response with no literal carries no body and is skipped: that is
        // what makes a flags-only answer harmless.
        assert!(client.fetch_bodies("1:50", false).unwrap().is_empty());
        assert_eq!(server.join().unwrap().len(), 2);
    }
}
