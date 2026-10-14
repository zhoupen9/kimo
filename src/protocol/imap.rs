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
            MailError::Protocol(msg) | MailError::Refused(msg) => MailError::Auth(msg),
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
        order_mailboxes(&mut folders);
        // Every mailbox has at least an INBOX, so an answer that names none is
        // not one: taking it for a list would say the server has no mailboxes
        // at all — and a caller using this to decide whether a mailbox is gone
        // would believe it. It is reported as the protocol failure it is.
        if folders.is_empty() {
            return Err(MailError::Protocol(
                "LIST named no mailbox, not even INBOX".to_string(),
            ));
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

    /// The oldest message the selected mailbox holds: `FETCH 1 (UID)` asks for
    /// the *first* message by sequence number, and sequence numbers run in UID
    /// order, so the first one is the oldest — one line of answer instead of a
    /// UID set the size of the mailbox. `None` is an empty mailbox, which is an
    /// answer too.
    pub fn oldest_uid(&mut self) -> MailResult<Option<u32>> {
        let (_, untagged) = self.run("FETCH 1 (UID)")?;
        Ok(untagged.iter().find_map(|line| extract_uid(line)))
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

    /// The whole raw source of one message, if the mailbox selected now still
    /// has it: one `UID FETCH <uid> (UID FLAGS BODY.PEEK[])`, the command a sync
    /// sends for a range. Both details matter. `BODY.PEEK[]` rather than
    /// `BODY[]`, because fetching a mail to export it must not mark it read;
    /// the UID rather than a sequence number, because a sequence number names
    /// whatever stands in that position now. `None` is a UID the server does
    /// not answer for — mail this mailbox no longer has.
    pub fn uid_body(&mut self, uid: u32) -> MailResult<Option<Vec<u8>>> {
        Ok(self
            .fetch_bodies(&uid.to_string(), true)?
            .into_iter()
            .find(|fetched| fetched.uid == uid)
            .map(|fetched| fetched.raw))
    }

    /// Put a message into a mailbox the server keeps: `APPEND`, with the
    /// message as a synchronizing literal — the copy of a mail that was just
    /// sent, so the account holds what it sent.
    ///
    /// The `+` continuation is what allows the bytes, exactly as in
    /// `login_with_literal`: nothing has to guess at a size the server would
    /// accept, and a server that says no says it before any of the message is
    /// written. The flags are `(\Seen)` — a copy of a sent mail is not
    /// something to come back to unread.
    pub fn append(&mut self, mailbox: &str, raw: &[u8]) -> MailResult<()> {
        let tag = self.next_tag();
        self.conn.write_line(&format!(
            "{} APPEND {} (\\Seen) {{{}}}",
            tag,
            quote_string(mailbox),
            raw.len()
        ))?;
        let continuation = self.read_text_line()?;
        if !continuation.starts_with('+') {
            return Err(MailError::Refused(format!(
                "APPEND {mailbox}: {continuation}"
            )));
        }
        self.conn.write_raw(raw)?;
        // The literal's own line ending, which ends the message: what `raw`
        // carries is the mail, not the transport's terminator.
        self.conn.write_line("")?;
        let (answer, _) = self.read_until_tagged(&tag)?;
        if !answer.starts_with("OK") {
            return Err(MailError::Refused(format!("APPEND {mailbox}: {answer}")));
        }
        Ok(())
    }

    /// Delete one message from `folder`, if the server still has it.
    ///
    /// The mailbox is checked against LIST first: one the server no longer
    /// lists is gone, and its mail with it — there is nothing remote left to
    /// delete, and the caller is free to drop its local copy. A mailbox that
    /// *is* listed but refuses SELECT is an error instead: the server may
    /// simply be unwilling to open it right now, and mail that is still on
    /// the server must not disappear here.
    ///
    /// The UID is then probed before anything is flagged, so a message
    /// another client has already removed takes the same local-only path.
    /// Only a message the server still answers for is flagged `\Deleted`
    /// and expunged, which is what keeps the two copies from disagreeing.
    pub fn delete_message(&mut self, folder: &str, uid: u32) -> MailResult<()> {
        if !self.list_folders()?.iter().any(|listed| listed == folder) {
            return Ok(());
        }
        self.select(folder)?;
        if !self.uid_exists(uid)? {
            return Ok(());
        }
        self.uid_store_flags_silent(uid, "+FLAGS.SILENT (\\Deleted)")?;
        self.uid_expunge(uid)
    }

    /// Whether `uid` is still in the selected mailbox: `UID FETCH <uid>
    /// (UID)` answers with one untagged FETCH line when it is, and with none
    /// when it is not.
    fn uid_exists(&mut self, uid: u32) -> MailResult<bool> {
        let (_, untagged) = self.run(&format!("UID FETCH {uid} (UID)"))?;
        Ok(untagged
            .iter()
            .any(|line| line.contains(" FETCH ") && extract_uid(line) == Some(uid)))
    }

    /// Expunge exactly one message: `UID EXPUNGE` (RFC 4315, the UIDPLUS
    /// extension) names the UID, so nothing another client left flagged goes
    /// with it. A server without UIDPLUS answers BAD, and the mailbox-wide
    /// `EXPUNGE` is then the only way — the message was flagged `\Deleted`
    /// right before, so it is what that command is aimed at.
    fn uid_expunge(&mut self, uid: u32) -> MailResult<()> {
        match self.run(&format!("UID EXPUNGE {uid}")) {
            Ok(_) => Ok(()),
            Err(MailError::Refused(_)) => self.run("EXPUNGE").map(|_| ()),
            Err(error) => Err(error),
        }
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
                // NO and BAD are the server's own answers — it read the
                // command and said no — which a caller with a fallback has to
                // be able to tell apart from a connection that went wrong.
                return Err(MailError::Refused(rest.to_string()));
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

/// The order a folder list is shown in: INBOX first, then the rest by name,
/// with duplicates dropped. A `LIST` answer goes through this, and so does the
/// folder list the database cached — the column draws the same mailboxes in the
/// same order before and after a sync, instead of reordering itself when one
/// lands.
pub fn order_mailboxes(folders: &mut Vec<String>) {
    folders.sort();
    folders.dedup();
    if let Some(pos) = folders.iter().position(|f| f.eq_ignore_ascii_case("INBOX")) {
        let inbox = folders.remove(pos);
        folders.insert(0, inbox);
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
    use std::io::{BufRead, Read, Write};

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

    #[test]
    fn the_folder_order_puts_inbox_first() {
        // The order the column draws folders in, and the one a `LIST` answer
        // comes back in, so a sync does not reorder it.
        let mut folders = vec![
            "Sent".to_string(),
            "Archive".to_string(),
            "INBOX".to_string(),
            "Archive".to_string(),
        ];
        order_mailboxes(&mut folders);
        assert_eq!(folders, ["INBOX", "Archive", "Sent"]);
    }

    /// An answer that names no mailbox is not a mailbox list. Taking it for one
    /// would say the account has no folders at all — and since a `LIST` is also
    /// how a delete asks whether the server still has a mailbox, it would say
    /// every folder is gone.
    #[test]
    fn a_list_that_names_no_mailbox_is_refused() {
        let (account, server) = one_shot_server(vec![(
            "LIST \"\" \"*\"",
            "{tag} OK LIST completed\r\n".to_string(),
        )]);
        let mut client = ImapClient::connect(&account).unwrap();
        assert!(client.list_folders().is_err());
        server.join().unwrap();
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
        (local_account(port), server)
    }

    /// The account the canned servers here listen for: loopback, plain, one
    /// pair of credentials.
    fn local_account(port: u16) -> Account {
        Account {
            id: "local".to_string(),
            name: "Local".to_string(),
            protocol: Protocol::Imap,
            host: "127.0.0.1".to_string(),
            port,
            username: "me".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Plain,
            smtp: None,
        }
    }

    /// A canned mailbox: LIST knows INBOX, SELECT opens it, and one message
    /// (UID 45) is in it. Each test scripts what its server answers after
    /// that, so the commands a delete sends are pinned.
    fn incipit() -> Vec<(&'static str, String)> {
        vec![
            (
                "LIST \"\" \"*\"",
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK LIST completed\r\n"
                    .to_string(),
            ),
            (
                "SELECT \"INBOX\"",
                "* 3 EXISTS\r\n{tag} OK [READ-WRITE] SELECT completed\r\n".to_string(),
            ),
            (
                "UID FETCH 45 (UID)",
                "* 3 FETCH (UID 45)\r\n{tag} OK FETCH completed\r\n".to_string(),
            ),
        ]
    }

    #[test]
    fn deleting_a_message_flags_the_uid_and_expunges_it() {
        let mut script = incipit();
        script.push((
            "UID STORE 45 +FLAGS.SILENT (\\Deleted)",
            "{tag} OK STORE completed\r\n".to_string(),
        ));
        script.push((
            "UID EXPUNGE 45",
            "{tag} OK EXPUNGE completed\r\n".to_string(),
        ));
        let (account, server) = one_shot_server(script);
        let mut client = ImapClient::connect(&account).unwrap();

        client.delete_message("INBOX", 45).unwrap();
        assert_eq!(server.join().unwrap().len(), 5);
    }

    /// The oldest message is asked for by *sequence* number — `FETCH 1 (UID)`,
    /// not a UID set — because a sequence number is what is ordered by UID and
    /// can name the oldest message in one line. An empty mailbox answers with
    /// no FETCH line at all, which is not an error.
    #[test]
    fn the_oldest_uid_is_the_first_message_by_sequence_number() {
        let (account, server) = one_shot_server(vec![
            (
                "LIST \"\" \"*\"",
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK LIST completed\r\n"
                    .to_string(),
            ),
            (
                "SELECT \"INBOX\"",
                "* 3 EXISTS\r\n{tag} OK [READ-WRITE] SELECT completed\r\n".to_string(),
            ),
            (
                "FETCH 1 (UID)",
                "* 1 FETCH (UID 480)\r\n{tag} OK FETCH completed\r\n".to_string(),
            ),
        ]);
        let mut client = ImapClient::connect(&account).unwrap();
        client.list_folders().unwrap();
        client.select("INBOX").unwrap();
        assert_eq!(client.oldest_uid().unwrap(), Some(480));
        server.join().unwrap();

        // The same mailbox, empty: the server answers the FETCH with nothing.
        let (account, server) = one_shot_server(vec![
            (
                "LIST \"\" \"*\"",
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK LIST completed\r\n"
                    .to_string(),
            ),
            (
                "SELECT \"INBOX\"",
                "* 0 EXISTS\r\n{tag} OK [READ-WRITE] SELECT completed\r\n".to_string(),
            ),
            ("FETCH 1 (UID)", "{tag} OK FETCH completed\r\n".to_string()),
        ]);
        let mut client = ImapClient::connect(&account).unwrap();
        client.list_folders().unwrap();
        client.select("INBOX").unwrap();
        assert_eq!(client.oldest_uid().unwrap(), None);
        server.join().unwrap();
    }

    /// A server without UIDPLUS rejects `UID EXPUNGE`; the message has just
    /// been flagged, so the mailbox-wide EXPUNGE is the way to finish.
    #[test]
    fn a_server_without_uidplus_falls_back_to_the_plain_expunge() {
        let mut script = incipit();
        script.push((
            "UID STORE 45 +FLAGS.SILENT (\\Deleted)",
            "{tag} OK STORE completed\r\n".to_string(),
        ));
        script.push((
            "UID EXPUNGE 45",
            "{tag} BAD Unknown command\r\n".to_string(),
        ));
        script.push((
            "EXPUNGE",
            "* 3 EXPUNGE\r\n{tag} OK EXPUNGE completed\r\n".to_string(),
        ));
        let (account, server) = one_shot_server(script);
        let mut client = ImapClient::connect(&account).unwrap();

        client.delete_message("INBOX", 45).unwrap();
        assert_eq!(server.join().unwrap().len(), 6);
    }

    /// The mail is not on the server any more, so there is nothing to flag or
    /// expunge: the commands stop at the probe, and the caller drops its own
    /// copy. `UID FETCH` answers a UID it does not have with no FETCH line.
    #[test]
    fn a_uid_the_server_no_longer_has_is_never_stored_or_expunged() {
        let mut script = vec![incipit()[0].clone(), incipit()[1].clone()];
        script.push((
            "UID FETCH 45 (UID)",
            "{tag} OK FETCH completed\r\n".to_string(),
        ));
        let (account, server) = one_shot_server(script);
        let mut client = ImapClient::connect(&account).unwrap();

        client.delete_message("INBOX", 45).unwrap();
        assert_eq!(server.join().unwrap().len(), 3);
    }

    /// A mailbox the server no longer lists is gone with its mail: no SELECT,
    /// no STORE — nothing remote is left to delete.
    #[test]
    fn a_mailbox_the_server_no_longer_lists_is_never_selected() {
        let script = vec![incipit()[0].clone()];
        let (account, server) = one_shot_server(script);
        let mut client = ImapClient::connect(&account).unwrap();

        // The server lists INBOX alone: a mail cached under Archive has no
        // mailbox left to be deleted from.
        client.delete_message("Archive", 45).unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
    }

    /// A mailbox that is listed but refuses SELECT is not a mailbox whose mail
    /// may be dropped: the refusal travels back to the caller, which keeps its
    /// copy.
    #[test]
    fn a_refused_select_keeps_the_callers_copy() {
        let script = vec![
            incipit()[0].clone(),
            (
                "SELECT \"INBOX\"",
                "{tag} NO mailbox is locked\r\n".to_string(),
            ),
        ];
        let (account, server) = one_shot_server(script);
        let mut client = ImapClient::connect(&account).unwrap();

        let error = client.delete_message("INBOX", 45).unwrap_err();
        assert!(matches!(error, MailError::Refused(_)), "{error}");
        assert_eq!(server.join().unwrap().len(), 2);
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

    /// One body is fetched by UID for an export, and with PEEK: the UID names
    /// the mail that was asked for rather than whatever stands in that
    /// position, the PEEK keeps it unread, and a UID the mailbox no longer has
    /// is `None` — the server answers it with no FETCH line at all — rather
    /// than a failed fetch.
    #[test]
    fn one_body_is_fetched_by_uid_with_a_peek() {
        let body = "Subject: hi\r\n\r\nbody\r\n";
        let response = format!(
            "* 3 FETCH (UID 45 BODY[] {{{}}}\r\n{body})\r\n{{tag}} OK FETCH completed\r\n",
            body.len()
        );
        let (account, server) = one_shot_server(vec![
            (
                "SELECT \"INBOX\"",
                "* 3 EXISTS\r\n{tag} OK [READ-WRITE] SELECT completed\r\n".to_string(),
            ),
            ("UID FETCH 45 (UID FLAGS BODY.PEEK[])", response),
            (
                "UID FETCH 46 (UID FLAGS BODY.PEEK[])",
                "{tag} OK FETCH completed\r\n".to_string(),
            ),
        ]);
        let mut client = ImapClient::connect(&account).unwrap();
        client.select("INBOX").unwrap();

        assert_eq!(client.uid_body(45).unwrap().unwrap(), body.as_bytes());
        assert_eq!(client.uid_body(46).unwrap(), None);
        assert_eq!(server.join().unwrap().len(), 3);
    }
    /// The copy of a sent mail goes up as an `APPEND` literal: the command names the
    /// mailbox, the size and the flag, the `+` continuation is what lets the
    /// bytes through, and the server's tagged `OK` is the answer.
    #[test]
    fn a_sent_copy_is_appended_as_a_seen_literal() {
        const RAW: &[u8] = b"Subject: hi\r\n\r\nhello\r\n";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"* OK ready\r\n").unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(
                command,
                format!("APPEND \"INBOX.Sent\" (\\Seen) {{{}}}", RAW.len())
            );
            socket.write_all(b"+ Ready for literal data\r\n").unwrap();
            let mut literal = vec![0_u8; RAW.len()];
            reader.read_exact(&mut literal).unwrap();
            // The client's own empty line closes the literal.
            let mut end = String::new();
            reader.read_line(&mut end).unwrap();
            assert_eq!(end, "\r\n");
            socket
                .write_all(format!("{tag} OK APPEND completed\r\n").as_bytes())
                .unwrap();
            literal
        });

        let account = local_account(port);
        let mut client = ImapClient::connect(&account).unwrap();
        client.append("INBOX.Sent", RAW).unwrap();
        assert_eq!(server.join().unwrap(), RAW);
    }
}
