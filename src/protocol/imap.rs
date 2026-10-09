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

    /// FETCH `seq_set` (UID FLAGS BODY.PEEK[]) and return each message with
    /// its raw RFC 5322 bytes. Handles `{n}` literal framing.
    pub fn fetch_bodies(&mut self, seq_set: &str) -> MailResult<Vec<Fetched>> {
        let tag = self.next_tag();
        self.conn
            .write_line(&format!("{tag} FETCH {seq_set} (UID FLAGS BODY.PEEK[])"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(quote_string("plain"), "\"plain\"");
        assert_eq!(quote_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
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
}
