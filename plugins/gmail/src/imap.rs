//! A minimal IMAP4rev1 client for Gmail: just the commands the provider
//! operations need, over a host TLS stream, with the `X-GM-EXT-1` extensions
//! carried first-class.
//!
//! What that means concretely:
//!
//! - `AUTHENTICATE XOAUTH2`, including the base64 JSON error continuation
//!   Gmail answers a bad token with;
//! - `FETCH` asking for `X-GM-LABELS`, `X-GM-THRID` and `X-GM-MSGID` beside
//!   `UID`/`FLAGS`/`BODY.PEEK[]` — and quietly *not* asking when the server
//!   did not advertise `X-GM-EXT-1`, which is the degrade path the profile
//!   promises;
//! - `UID STORE ±X-GM-LABELS`, where a system label is a flag-shaped
//!   `\Trash` and a user label is a quoted string;
//! - `UID SEARCH X-GM-RAW "…"`, the one Gmail search worth a round trip.
//!
//! Line-reading handles `{n}` literals, because every body Gmail returns is
//! one.

use crate::abi::{self, NetStream};

pub const HOST: &str = "imap.gmail.com";
pub const PORT: u16 = 993;
/// The Gmail extension everything provider-specific here depends on.
pub const GM_EXT: &str = "X-GM-EXT-1";

/// One untagged response line, with the literal that followed it (a FETCH
/// body) attached if there was one. The continuation after the literal is
/// appended to `text`, so a FETCH item list reads as one line again.
struct Line {
    text: String,
    literal: Option<Vec<u8>>,
}

/// One message as a full FETCH brought it back.
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub labels: Vec<String>,
    pub thrid: Option<String>,
    pub gmid: Option<String>,
    pub raw: Vec<u8>,
}

pub struct SelectInfo {
    pub exists: u32,
    pub uidvalidity: Option<u32>,
}

pub struct Client {
    stream: NetStream,
    buffer: Vec<u8>,
    tag: u32,
    pub capabilities: Vec<String>,
}

/// The failure shapes the operations above turn into error codes: a refused
/// login is `auth_failed` and everything else is the wire's own words.
pub enum Failed {
    Auth(String),
    /// The transport let go: the port is filtered, the peer reset, TLS died.
    /// Distinct from `Protocol` because it is the network's answer, not the
    /// server's, and a user can act on the difference.
    Network(String),
    Protocol(String),
}

impl Client {
    /// Implicit-TLS connect, greeting, capabilities. The greeting often
    /// carries the capability list already; asking again is one cheap line
    /// and keeps the answer current after login.
    pub fn connect() -> Result<Self, Failed> {
        let stream = NetStream::open(HOST, PORT, true).map_err(Failed::Network)?;
        let mut client = Self {
            stream,
            buffer: Vec::new(),
            tag: 0,
            capabilities: Vec::new(),
        };
        let greeting = client.read_line().map_err(|error| match error {
            // The usual shape of a filtered port: the TCP connection is made
            // and then dropped without a byte. Say where it happened, because
            // "the host refused the read" helps nobody.
            Failed::Network(message) | Failed::Protocol(message) => Failed::Network(format!(
                "{HOST}:{PORT} closed before the greeting ({message}) — the network may block IMAP"
            )),
            other => other,
        })?;
        if !greeting.starts_with("* OK") {
            return Err(Failed::Protocol(format!(
                "the server did not greet: {greeting}"
            )));
        }
        client.capabilities()?;
        Ok(client)
    }

    pub fn has(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case(name))
    }

    fn capabilities(&mut self) -> Result<(), Failed> {
        for line in self.run("CAPABILITY")? {
            if let Some(rest) = line.text.strip_prefix("* CAPABILITY ") {
                self.capabilities = rest.split_whitespace().map(str::to_string).collect();
            }
        }
        Ok(())
    }

    /// XOAUTH2 with the error continuation: Gmail answers a bad token with
    /// `+ <base64 JSON>` and only refuses after the client sends an empty
    /// line. Skipping that step leaves the session wedged, so it is here —
    /// and after the token, which is where Gmail actually puts it. The first
    /// version only read it before the token, so a refused token became a
    /// sixty-second read timeout instead of an authentication failure.
    pub fn authenticate_xoauth2(&mut self, user: &str, token: &str) -> Result<(), Failed> {
        use base64::Engine as _;
        let tag = self.next_tag();
        self.write_line(&format!("{tag} AUTHENTICATE XOAUTH2"))?;
        let answer = self.read_line()?;
        if !answer.starts_with('+') {
            return Err(Failed::Protocol(format!(
                "the server did not ask for the token: {answer}"
            )));
        }
        // The first continuation is the empty one. A payload in it is the
        // server refusing before it ever asked for the token.
        if !answer[1..].trim().is_empty() {
            return Err(self.refuse_auth(&tag, &answer[1..]));
        }
        let blob =
            base64::engine::general_purpose::STANDARD.encode(crate::oauth::xoauth2(user, token));
        self.write_line(&blob)?;
        // After the token: the tagged answer, or one more continuation with
        // the base64 JSON refusal that has to be cancelled before the tag.
        loop {
            let line = self.read_line()?;
            if let Some(payload) = line.strip_prefix('+') {
                return Err(self.refuse_auth(&tag, payload));
            }
            let Some(status) = line.strip_prefix(&format!("{tag} ")) else {
                continue;
            };
            let reason = status
                .split_once(' ')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| status.to_string());
            return if status.starts_with("OK") {
                Ok(())
            } else if status.starts_with("NO") {
                Err(Failed::Auth(reason))
            } else {
                Err(Failed::Protocol(reason))
            };
        }
    }

    /// The base64 JSON in a refusal continuation: its own word for the
    /// message, then the empty line the SASL exchange needs to end, and the
    /// tagged answer read so the session is not left half-way.
    fn refuse_auth(&mut self, tag: &str, payload: &str) -> Failed {
        use base64::Engine as _;
        let reason = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|json| {
                json.get("sasl")
                    .or_else(|| json.get("error"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "the token was refused".to_string());
        let _ = self.write_line("");
        let _ = self.read_until_tag(tag);
        Failed::Auth(reason)
    }

    /// The app-password path: plain LOGIN, no less a citizen than OAuth.
    pub fn login(&mut self, user: &str, password: &str) -> Result<(), Failed> {
        let tag = self.next_tag();
        self.write_line(&format!(
            "{tag} LOGIN {} {}",
            quoted(user),
            quoted(password)
        ))?;
        self.expect_tag(&tag)
    }

    pub fn list(&mut self) -> Result<Vec<String>, Failed> {
        let mut folders = Vec::new();
        for line in self.run(r#"LIST "" "*""#)? {
            // * LIST (\HasNoChildren) "/" "[Gmail]/All Mail"
            if let Some(rest) = line.text.strip_prefix("* LIST ") {
                if let Some(name) = rest.rsplit('"').nth(1) {
                    folders.push(name.to_string());
                }
            }
        }
        Ok(folders)
    }

    pub fn select(&mut self, folder: &str) -> Result<SelectInfo, Failed> {
        let mut info = SelectInfo {
            exists: 0,
            uidvalidity: None,
        };
        for line in self.run(&format!("SELECT {}", quoted(folder)))? {
            let text = &line.text;
            if let Some(rest) = text.strip_prefix("* ") {
                if let Some(count) = rest.strip_suffix(" EXISTS") {
                    info.exists = count.trim().parse().unwrap_or(0);
                }
            }
            if let Some(open) = text.find("[UIDVALIDITY ") {
                let rest = &text[open + "[UIDVALIDITY ".len()..];
                if let Some(value) = rest.split([' ', ']']).next() {
                    info.uidvalidity = value.parse().ok();
                }
            }
        }
        Ok(info)
    }

    /// The server's own answer for how far down the mailbox goes, from one
    /// `FETCH 1 (UID)`. An empty folder is not asked: SELECT already said
    /// there is no first message to name.
    pub fn oldest_uid(&mut self, exists: u32) -> Result<Option<u32>, Failed> {
        if exists == 0 {
            return Ok(None);
        }
        for line in self.run("FETCH 1 (UID)")? {
            if let Some(fetched) = parse_fetch(&line.text, line.literal.as_deref()) {
                return Ok(Some(fetched.uid));
            }
        }
        Ok(None)
    }

    /// The full fetch the sync runs: flags, bodies and — when the extension
    /// is there — labels and both Gmail ids.
    pub fn fetch_full(&mut self, set: &str, by_uid: bool) -> Result<Vec<Fetched>, Failed> {
        self.fetch_bounded(set, by_uid, usize::MAX)
            .map(|(messages, _)| messages)
    }

    /// The same fetch, stopping once `budget` bytes of body have been read.
    /// The second value says the range was not finished, so the caller's new
    /// mark picks the rest up in the next call: one interpreted call has one
    /// fuel budget, and a backfill of fifty full mails is more than it can
    /// base64 in one go. A range whose first mail alone is over budget is
    /// still read — progress before the cap.
    pub fn fetch_bounded(
        &mut self,
        set: &str,
        by_uid: bool,
        budget: usize,
    ) -> Result<(Vec<Fetched>, bool), Failed> {
        let items = if self.has(GM_EXT) {
            "(UID FLAGS X-GM-LABELS X-GM-THRID X-GM-MSGID BODY.PEEK[])"
        } else {
            "(UID FLAGS BODY.PEEK[])"
        };
        let verb = if by_uid { "UID FETCH" } else { "FETCH" };
        let tag = self.next_tag();
        self.write_line(&format!("{tag} {verb} {set} {items}"))?;
        let prefix = format!("{tag} ");
        let mut messages = Vec::new();
        let mut bytes = 0usize;
        loop {
            let text = self.read_line()?;
            if let Some(status) = text.strip_prefix(&prefix) {
                if status.starts_with("OK") {
                    return Ok((messages, false));
                }
                let reason = status
                    .split_once(' ')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_else(|| status.to_string());
                return Err(Failed::Protocol(reason));
            }
            let mut line = Line {
                text,
                literal: None,
            };
            if let Some(size) = trailing_literal(&line.text) {
                // The next body does not fit. Leave it unread: dropping the
                // client closes the stream, and the mark resumes at it.
                if bytes >= budget && !messages.is_empty() {
                    return Ok((messages, true));
                }
                line.literal = Some(self.read_exact(size)?);
                bytes += size;
                line.text.push_str(&self.read_line()?);
            }
            if abi::cancelled() {
                return Err(Failed::Protocol("the call was cancelled".to_string()));
            }
            if let Some(fetched) = parse_fetch(&line.text, line.literal.as_deref()) {
                messages.push(fetched);
            }
            if bytes >= budget && !messages.is_empty() {
                return Ok((messages, true));
            }
        }
    }

    pub fn uid_store_flags(&mut self, uid: u32, add: bool, flag: &str) -> Result<(), Failed> {
        let sign = if add { "+" } else { "-" };
        let tag = self.next_tag();
        self.write_line(&format!(
            "{tag} UID STORE {uid} {sign}FLAGS.SILENT ({flag})"
        ))?;
        self.expect_tag(&tag)
    }

    /// `UID STORE ±X-GM-LABELS (…)`: a system label keeps its `\Inbox`
    /// shape, a user label is a quoted string — the wire's own two forms,
    /// not a choice of ours.
    pub fn uid_store_labels(
        &mut self,
        uid: u32,
        add: bool,
        labels: &[String],
    ) -> Result<(), Failed> {
        if labels.is_empty() {
            return Ok(());
        }
        let sign = if add { "+" } else { "-" };
        let list = labels
            .iter()
            .map(|label| label_wire(label))
            .collect::<Vec<_>>()
            .join(" ");
        let tag = self.next_tag();
        self.write_line(&format!("{tag} UID STORE {uid} {sign}X-GM-LABELS ({list})"))?;
        self.expect_tag(&tag)
    }

    /// `UID SEARCH X-GM-RAW "…"`: the expression is Gmail's own search
    /// language, passed through untouched but for its quoting.
    pub fn uid_search_raw(&mut self, expression: &str) -> Result<Vec<u32>, Failed> {
        let tag = self.next_tag();
        self.write_line(&format!("{tag} UID SEARCH X-GM-RAW {}", quoted(expression)))?;
        let mut uids = Vec::new();
        for line in self.read_until_tag(&tag)? {
            if let Some(rest) = line.text.strip_prefix("* SEARCH") {
                for word in rest.split_whitespace() {
                    if let Ok(uid) = word.parse() {
                        uids.push(uid);
                    }
                }
            }
        }
        Ok(uids)
    }

    pub fn append(&mut self, mailbox: &str, raw: &[u8]) -> Result<(), Failed> {
        let tag = self.next_tag();
        self.write_line(&format!(
            "{tag} APPEND {} {{{}}}",
            quoted(mailbox),
            raw.len()
        ))?;
        let answer = self.read_line()?;
        if !answer.starts_with('+') {
            return Err(Failed::Protocol(format!(
                "the server refused the append: {answer}"
            )));
        }
        self.write_bytes(raw)?;
        self.write_line("")?;
        self.expect_tag(&tag)
    }

    pub fn logout(&mut self) {
        let tag = self.next_tag();
        let _ = self.write_line(&format!("{tag} LOGOUT"));
    }

    // ----- the command/response engine --------------------------------------

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("T{}", self.tag)
    }

    /// Run a command to its tagged answer, collecting every untagged line —
    /// literals read and their continuation appended.
    fn run(&mut self, command: &str) -> Result<Vec<Line>, Failed> {
        let tag = self.next_tag();
        self.write_line(&format!("{tag} {command}"))?;
        self.read_until_tag(&tag)
    }

    fn read_until_tag(&mut self, tag: &str) -> Result<Vec<Line>, Failed> {
        let prefix = format!("{tag} ");
        let mut lines = Vec::new();
        loop {
            let text = self.read_line()?;
            if let Some(status) = text.strip_prefix(&prefix) {
                if status.starts_with("OK") {
                    return Ok(lines);
                }
                let reason = status
                    .split_once(' ')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_else(|| status.to_string());
                return Err(Failed::Protocol(reason));
            }
            let mut line = Line {
                text,
                literal: None,
            };
            // A line that ends in `{n}` holds a literal: read the bytes,
            // then the line the server continues with.
            if let Some(size) = trailing_literal(&line.text) {
                line.literal = Some(self.read_exact(size)?);
                line.text.push_str(&self.read_line()?);
            }
            lines.push(line);
        }
    }

    fn expect_tag(&mut self, tag: &str) -> Result<(), Failed> {
        self.read_until_tag(tag).map(|_| ())
    }

    fn write_line(&mut self, line: &str) -> Result<(), Failed> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.stream.write(&bytes).map_err(Failed::Network)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), Failed> {
        self.stream.write(bytes).map_err(Failed::Network)
    }

    fn read_line(&mut self) -> Result<String, Failed> {
        loop {
            if let Some(line) = crate::wire::take_line(&mut self.buffer) {
                return Ok(line);
            }
            let mut chunk = [0u8; 4096];
            let read = self.stream.read(&mut chunk).map_err(Failed::Network)?;
            if read == 0 {
                return Err(Failed::Network("the server closed the connection".into()));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn read_exact(&mut self, len: usize) -> Result<Vec<u8>, Failed> {
        while self.buffer.len() < len {
            let mut chunk = [0u8; 16384];
            let read = self.stream.read(&mut chunk).map_err(Failed::Network)?;
            if read == 0 {
                return Err(Failed::Network(
                    "the server closed the connection mid-literal".into(),
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
        Ok(self.buffer.drain(..len).collect())
    }
}

impl Drop for Client {
    /// The host keeps every open stream in a table keyed by id; one left open
    /// is a socket and its TLS state held for the whole job. Every operation
    /// ends with logout(), so the drop is what actually closes the wire.
    fn drop(&mut self) {
        self.stream.close();
    }
}

/// `{3417}` at the end of a line: the literal size that follows.
fn trailing_literal(line: &str) -> Option<usize> {
    if !line.ends_with('}') {
        return None;
    }
    let open = line.rfind('{')?;
    line[open + 1..line.len() - 1].parse().ok()
}

/// An IMAP quoted string, with `\` and `"` escaped.
pub fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        if matches!(character, '\\' | '"') {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

/// A label on the wire: system labels (`\Trash`, `\Inbox`, …) are atoms of
/// their own, everything else is a quoted string.
pub fn label_wire(label: &str) -> String {
    if label.starts_with('\\') {
        label.to_string()
    } else {
        quoted(label)
    }
}

// ----- FETCH response parsing -----------------------------------------------

/// One token of a FETCH item list.
enum Token {
    Atom(String),
    Quoted(String),
    Open,
    Close,
}

fn tokenize(text: &str) -> Vec<Token> {
    // An index walk rather than an iterator: a quoted string's escape pairs
    // need to advance two characters at once, which an owned `Vec<char>`
    // makes trivial and a borrowed iterator makes a double-borrow error.
    let characters: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        index += 1;
        match character {
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            ' ' => {}
            '"' => {
                let mut value = String::new();
                while index < characters.len() {
                    let next = characters[index];
                    index += 1;
                    if next == '\\' {
                        if index < characters.len() {
                            value.push(characters[index]);
                            index += 1;
                        }
                    } else if next == '"' {
                        break;
                    } else {
                        value.push(next);
                    }
                }
                tokens.push(Token::Quoted(value));
            }
            other => {
                let mut value = String::new();
                value.push(other);
                while index < characters.len() {
                    let next = characters[index];
                    if next == ' ' || next == '(' || next == ')' || next == '"' {
                        break;
                    }
                    value.push(next);
                    index += 1;
                }
                tokens.push(Token::Atom(value));
            }
        }
    }
    tokens
}

/// A `(…)` list that starts at `Open`, as its strings — the shape both
/// `FLAGS` and `X-GM-LABELS` answer with.
fn take_list(tokens: &[Token], at: usize) -> Option<(Vec<String>, usize)> {
    if !matches!(tokens.get(at), Some(Token::Open)) {
        return None;
    }
    let mut values = Vec::new();
    let mut index = at + 1;
    loop {
        match tokens.get(index) {
            Some(Token::Atom(value)) => values.push(value.clone()),
            Some(Token::Quoted(value)) => values.push(value.clone()),
            Some(Token::Close) => return Some((values, index + 1)),
            _ => return None,
        }
        index += 1;
    }
}

/// One untagged FETCH line into a `Fetched`. The literal is `BODY[]`, whose
/// `{n}` marker was stripped from the text by the reader; it is the only
/// item that carries one.
pub fn parse_fetch(text: &str, literal: Option<&[u8]>) -> Option<Fetched> {
    let open = text.find('(')?;
    let close = text.rfind(')').filter(|&position| position > open)?;
    let tokens = tokenize(&text[open + 1..close]);
    let mut fetched = Fetched {
        uid: 0,
        flags: Vec::new(),
        labels: Vec::new(),
        thrid: None,
        gmid: None,
        raw: Vec::new(),
    };
    let mut index = 0;
    while index < tokens.len() {
        let name = match &tokens[index] {
            Token::Atom(name) => name.clone(),
            _ => return None,
        };
        index += 1;
        match name.to_ascii_uppercase().as_str() {
            "UID" | "X-GM-THRID" | "X-GM-MSGID" => {
                let Some(Token::Atom(value)) = tokens.get(index) else {
                    return None;
                };
                match name.to_ascii_uppercase().as_str() {
                    "UID" => fetched.uid = value.parse().ok()?,
                    "X-GM-THRID" => fetched.thrid = Some(value.clone()),
                    _ => fetched.gmid = Some(value.clone()),
                }
                index += 1;
            }
            "FLAGS" | "X-GM-LABELS" => {
                let (values, next) = take_list(&tokens, index)?;
                if name == "FLAGS" {
                    fetched.flags = values;
                } else {
                    fetched.labels = values;
                }
                index = next;
            }
            "BODY" | "BODY[]" | "RFC822" => {
                // The `{n}` marker ended the line; the bytes are the literal.
                fetched.raw = literal?.to_vec();
                // Skip whatever followed the marker (usually just `)`).
                break;
            }
            other => {
                // An item this parser does not name: skip its value if it
                // is a plain one, or its list.
                if let Some((_, next)) = take_list(&tokens, index) {
                    index = next;
                } else if matches!(
                    tokens.get(index),
                    Some(Token::Atom(_)) | Some(Token::Quoted(_))
                ) {
                    index += 1;
                } else {
                    let _ = other;
                    return None;
                }
            }
        }
    }
    if fetched.uid == 0 {
        return None;
    }
    Some(fetched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_gmail_fetch_line_parses_with_its_literal() {
        // The reader appends the line after the literal, so `BODY[]` sits
        // just inside the closing parenthesis.
        let text = "* 7 FETCH (UID 4412 FLAGS (\\Seen) X-GM-THRID 1770462772738602100 \
                    X-GM-MSGID 1770462772738602101 X-GM-LABELS (\\Inbox \"work receipts\" \
                    Receipts) BODY[])";
        let literal = b"Subject: hi\r\n\r\nbody";
        let fetched = parse_fetch(text, Some(literal)).unwrap();
        assert_eq!(fetched.uid, 4412);
        assert!(fetched.flags.contains(&"\\Seen".to_string()));
        assert_eq!(fetched.labels.len(), 3);
        assert!(fetched.labels.contains(&"\\Inbox".to_string()));
        assert!(fetched.labels.contains(&"work receipts".to_string()));
        assert!(fetched.labels.contains(&"Receipts".to_string()));
        assert_eq!(fetched.thrid.as_deref(), Some("1770462772738602100"));
        assert_eq!(fetched.gmid.as_deref(), Some("1770462772738602101"));
        assert_eq!(fetched.raw, literal);
    }

    #[test]
    fn a_plain_imap_fetch_line_parses_without_the_extension() {
        let text = "* 2 FETCH (UID 9 FLAGS () BODY[])";
        let fetched = parse_fetch(text, Some(b"abc")).unwrap();
        assert_eq!(fetched.uid, 9);
        assert!(fetched.flags.is_empty());
        assert!(fetched.labels.is_empty());
        assert!(fetched.thrid.is_none());
        assert_eq!(fetched.raw, b"abc");
    }

    #[test]
    fn labels_take_the_wire_s_two_shapes() {
        assert_eq!(label_wire("\\Trash"), "\\Trash");
        assert_eq!(label_wire("Receipts"), "\"Receipts\"");
        assert_eq!(label_wire("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn quoting_escapes_and_the_literal_marker_reads() {
        assert_eq!(quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(trailing_literal("BODY[] {3417}"), Some(3417));
        assert_eq!(trailing_literal("FLAGS (\\Seen)"), None);
        assert_eq!(trailing_literal("literal {not-a-number}"), None);
    }
}
