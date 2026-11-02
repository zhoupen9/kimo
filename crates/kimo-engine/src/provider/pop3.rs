//! The POP3 adapter: `MailProvider` over `crate::protocol::pop3`.
//!
//! A drop has no folders, no server-side flags and no place to file a sent
//! copy, so the adapter answers one mailbox called INBOX and does the parts
//! POP3 *does* have: UIDL identity, the numbered window a first fetch takes,
//! the floor a `Load older mail` click walks down, and `DELE` committed by
//! `QUIT`. The database is not consulted: the UIDLs it holds arrive in the
//! `FolderMark`.

use crate::error::{MailError, MailResult};
use crate::mail;
use crate::models::{Account, Protocol};
use crate::protocol::pop3::Pop3Client;

use super::{
    mail_is_gone, Capabilities, DeleteOutcome, FetchKind, FolderBatch, FolderMark, MailProvider,
    OlderBatch, BACKFILL_LIMIT, INBOX,
};

pub struct Pop3Provider {
    client: Pop3Client,
    /// Whether this session has already sent `QUIT`. `delete` ends the session
    /// itself (that is what commits the deletions), so `logout` must not send
    /// it a second time.
    closed: bool,
}

impl Pop3Provider {
    /// Connect, negotiate TLS and log in.
    pub fn open(account: &Account) -> MailResult<Self> {
        let mut client = Pop3Client::connect(account)?;
        // A refused login is still a session the server opened: end it the
        // polite way before reporting the refusal.
        if let Err(error) = client.login(&account.username, &account.password) {
            client.quit();
            return Err(error);
        }
        Ok(Self {
            client,
            closed: false,
        })
    }

    /// End the session politely, once.
    fn quit(&mut self) {
        if !self.closed {
            self.client.quit();
            self.closed = true;
        }
    }
}

impl MailProvider for Pop3Provider {
    fn capabilities(&self) -> Capabilities {
        super::capabilities(Protocol::Pop3)
    }

    /// The drop is the account's one mailbox. Sync asks this only when
    /// `Capabilities::folders` is true, which it never is here.
    fn folders(&mut self) -> MailResult<Vec<String>> {
        Ok(vec![INBOX.to_string()])
    }

    fn fetch_folder(&mut self, _folder: &str, mark: &FolderMark) -> MailResult<FolderBatch> {
        let count = self.client.stat()?;
        let uidls = self.client.uidl()?;
        let known = mark.stored.clone().unwrap_or_default();
        // The first sync of a drop takes the newest messages only; the floor it
        // remembers is what keeps every later sync from coming back for the
        // thousands it deliberately skipped. UIDL identity is what makes those
        // later syncs incremental — a message already stored is never
        // downloaded again, whatever its number. The newest-messages window is
        // still consulted, so a drop that shrank below the floor (mail deleted
        // elsewhere, and the numbering with it) cannot disappear.
        let cold = mark.first_num.is_none();
        let window = backfill_start(count);
        let floor = mark
            .first_num
            .map_or(window, |floor| (floor as usize).min(window));
        let mut messages = Vec::new();
        for (num, uid) in uidls {
            if num < floor || known.contains(&uid) {
                continue;
            }
            let raw = self.client.retr(num)?;
            messages.push(mail::parse(&raw, &uid, false));
        }
        Ok(FolderBatch {
            uidvalidity: None,
            messages,
            kind: if cold {
                FetchKind::Fill
            } else {
                FetchKind::Resume
            },
            // A drop's numbering starts at 1, so the window is also the floor;
            // both marks are its cold-start answers and nothing moves them again.
            first_num: cold.then_some(window as u32),
            // One RETR loop walks the window; there is no range left to page.
            more: false,
            oldest: cold.then_some((count > 0).then_some(1)),
            // The floor goes first: a folder with a floor and no mail is fetched
            // from that floor next time, so this order can cost a repeated pass
            // but never a message. Nothing rotates a drop's marks.
            marks_before_store: true,
        })
    }

    fn fetch_older(
        &mut self,
        _folder: &str,
        mark: &FolderMark,
        batch: u32,
    ) -> MailResult<OlderBatch> {
        let uidls = self.client.uidl()?;
        let known = mark.stored.clone().unwrap_or_default();
        let mut first_num = None;
        let mut messages = Vec::new();
        let mut available = false;
        if let Some((low, high)) = mark.first_num.and_then(|floor| older_numbers(floor, batch)) {
            for (num, uid) in uidls {
                let num = num as u32;
                if num < low || num > high || known.contains(&uid) {
                    continue;
                }
                let raw = self.client.retr(num as usize)?;
                messages.push(mail::parse(&raw, &uid, false));
            }
            // The floor moves whether or not the range held anything: it is
            // where this folder has been asked from, and a mailbox with gaps in
            // it would otherwise be asked for the same empty range forever.
            first_num = Some(low);
            available = low > 1;
        }
        Ok(OlderBatch {
            uidvalidity: None,
            messages,
            first_num,
            // POP3's oldest message is 1 and its first fetch already recorded it.
            oldest: None,
            available,
        })
    }

    fn body(&mut self, _folder: &str, uid: &str) -> MailResult<Vec<u8>> {
        self.client.retr_by_uidl(uid)?.ok_or_else(mail_is_gone)
    }

    fn set_seen(&mut self, _folder: &str, _uid: &str) -> MailResult<()> {
        // POP3 has no server-side flags: the flag is the database's alone.
        Ok(())
    }

    fn set_flagged(&mut self, _folder: &str, _uid: &str, _flagged: bool) -> MailResult<()> {
        Ok(())
    }

    fn delete(&mut self, _folder: &str, uids: &[String]) -> DeleteOutcome {
        let mut marked = Vec::new();
        let mut first_error = None;
        for uid in uids {
            match self.client.delete_by_uidl(uid) {
                Ok(()) => marked.push(uid.clone()),
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        // POP3 commits a deletion at QUIT and at nothing else: the session ends
        // here, before the caller writes a single local row.
        self.quit();
        DeleteOutcome {
            gone: marked,
            error: first_error,
        }
    }

    fn append(&mut self, _mailbox: &str, _raw: &[u8]) -> MailResult<()> {
        Err(MailError::Protocol(
            "POP3 has nowhere to file a sent copy".to_string(),
        ))
    }

    fn logout(&mut self) {
        self.quit();
    }
}

/// The lowest 1-based position a first sync reaches: the newest
/// `BACKFILL_LIMIT` messages of `total`.
fn backfill_start(total: usize) -> usize {
    total.saturating_sub(BACKFILL_LIMIT as usize) + 1
}

/// The walk below the floor for a POP3 drop, whose numbering always starts at
/// 1: the only question is how far above it the floor is. A floor below one
/// batch's worth takes the whole of what is left, which is why the subtraction
/// saturates — the first click of a folder that was backfilled from number 21
/// asks for 1..20, not for an underflow.
fn older_numbers(floor: u32, batch: u32) -> Option<(u32, u32)> {
    if floor <= 1 {
        return None;
    }
    Some((floor.saturating_sub(batch).max(1), floor - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same walk for a POP3 drop, whose numbering starts at 1 — so the
    /// floor alone says how much is left below it, and the walk ends there.
    #[test]
    fn a_drop_is_walked_down_to_its_first_message() {
        assert_eq!(older_numbers(4951, 50), Some((4901, 4950)));
        assert_eq!(older_numbers(60, 50), Some((10, 59)));
        assert_eq!(older_numbers(51, 50), Some((1, 50)));
        assert_eq!(older_numbers(50, 50), Some((1, 49)));
        // A floor *below* one batch — a drop that was backfilled from number
        // 21, say — takes what is left instead of underflowing.
        assert_eq!(older_numbers(21, 50), Some((1, 20)));
        assert_eq!(older_numbers(2, 50), Some((1, 1)));
        assert_eq!(older_numbers(1, 50), None, "the drop starts at number 1");
        assert_eq!(older_numbers(0, 50), None);
    }

    /// A first fetch takes the newest window and records the two cold-start
    /// marks; a warm one with the stored UIDLs asks only for the rest. The
    /// adapter needs no database for either, which is the point of the split.
    #[test]
    fn a_drop_fetches_its_newest_mail_then_only_what_is_new() {
        use std::io::{BufRead, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"+OK canned ready\r\n").unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let command = line.trim_end();
                let verb = command.split(' ').next().unwrap_or_default().to_uppercase();
                match verb.as_str() {
                    "USER" | "PASS" => socket.write_all(b"+OK\r\n").unwrap(),
                    "STAT" => socket.write_all(b"+OK 3 300\r\n").unwrap(),
                    "UIDL" => socket
                        .write_all(b"+OK\r\n1 a\r\n2 b\r\n3 c\r\n.\r\n")
                        .unwrap(),
                    "RETR" => socket
                        .write_all(b"+OK 26 octets\r\nSubject: x\r\n\r\nbody\r\n.\r\n")
                        .unwrap(),
                    "QUIT" => {
                        socket.write_all(b"+OK bye\r\n").unwrap();
                        break;
                    }
                    _ => socket.write_all(b"-ERR unknown\r\n").unwrap(),
                }
            }
        });

        let account = Account {
            id: "drop".to_string(),
            name: "Drop".to_string(),
            protocol: Protocol::Pop3,
            host: "127.0.0.1".to_string(),
            port,
            username: "me@example.com".to_string(),
            password: "secret".to_string(),
            tls: crate::models::TlsMode::Plain,
            smtp: None,
            provider: None,
            provider_config: None,
            access_token: None,
            refresh_token: None,
            token_expires_at: None,
        };
        let mut provider = Pop3Provider::open(&account).unwrap();

        let cold = provider
            .fetch_folder(INBOX, &FolderMark::default())
            .unwrap();
        assert_eq!(cold.kind, FetchKind::Fill);
        assert_eq!(cold.messages.len(), 3);
        assert_eq!(cold.first_num, Some(1));
        assert_eq!(cold.oldest, Some(Some(1)));
        assert!(cold.marks_before_store);

        let warm = provider
            .fetch_folder(
                INBOX,
                &FolderMark {
                    first_num: Some(1),
                    stored: Some(["a".to_string()].into_iter().collect()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(warm.kind, FetchKind::Resume);
        assert_eq!(warm.messages.len(), 2);
        assert_eq!(warm.messages[0].uid, "b");
        assert_eq!(warm.first_num, None);
        assert_eq!(warm.oldest, None);

        provider.logout();
        server.join().unwrap();
    }
}
