//! The IMAP adapter: `MailProvider` over `crate::protocol::imap`.
//!
//! The incremental rules live here and only here: a warm folder resumes at the
//! UID one past the highest stored one, a folder with no usable mark takes the
//! newest `BACKFILL_LIMIT` messages by sequence number, and a mark from another
//! UIDVALIDITY generation is dropped. The database is not consulted: the marks
//! arrive in a `FolderMark` and go back in a `FolderBatch`.

use crate::error::{MailError, MailResult};
use crate::mail;
use crate::models::{Account, Protocol};
use crate::protocol::imap::ImapClient;

use super::{
    mail_is_gone, Capabilities, DeleteOutcome, FetchKind, FolderBatch, FolderMark, MailProvider,
    OlderBatch, BACKFILL_LIMIT,
};

pub struct ImapProvider {
    client: ImapClient,
}

impl ImapProvider {
    /// Connect, negotiate TLS and log in. The session is the provider's whole
    /// life: every operation below runs on it, and `logout` ends it.
    pub fn open(account: &Account) -> MailResult<Self> {
        let mut client = ImapClient::connect(account)?;
        // A refused login still ends the session politely, the way the old
        // `with_imap` did: the server has a session to close even when it said
        // no to the credentials.
        if let Err(error) = client.login(&account.username, &account.password) {
            client.logout();
            return Err(error);
        }
        Ok(Self { client })
    }
}

impl MailProvider for ImapProvider {
    fn capabilities(&self) -> Capabilities {
        super::capabilities(Protocol::Imap)
    }

    fn folders(&mut self) -> MailResult<Vec<String>> {
        self.client.list_folders()
    }

    fn fetch_folder(&mut self, folder: &str, mark: &FolderMark) -> MailResult<FolderBatch> {
        let info = self.client.select(folder)?;
        let resume = resume_uid(mark, info.uidvalidity);
        // How far down the mailbox goes is the server's own answer, and this is
        // where it is asked for: one `FETCH 1 (UID)` per folder, kept in the
        // database so the list knows about older mail without a round trip.
        let oldest = self.client.oldest_uid()?;
        let mut floor: Option<u32> = None;
        let mut messages = Vec::new();
        if info.exists > 0 {
            // A folder that was fetched before is asked for the UIDs it does not
            // have yet: `<next>:*` is everything from the mark on. A server may
            // answer that with the last message when the mark is already past the
            // end, which the filter drops. A folder that was never fetched takes
            // the newest `BACKFILL_LIMIT` messages, by sequence number — and the
            // lowest UID of that window is the floor a `Load older mail` click
            // starts below.
            let fetched = match resume {
                Some(next) => self
                    .client
                    .fetch_bodies(&format!("{next}:*"), true)?
                    .into_iter()
                    .filter(|message| message.uid >= next)
                    .collect(),
                None => {
                    let start = backfill_start(info.exists as usize) as u32;
                    let fetched = self
                        .client
                        .fetch_bodies(&format!("{start}:{}", info.exists), false)?;
                    floor = fetched.iter().map(|message| message.uid).min();
                    fetched
                }
            };
            for fetched in fetched {
                let (seen, flagged) = fetched_state(&fetched.flags);
                let mut message = mail::parse(&fetched.raw, &fetched.uid.to_string(), seen);
                message.flagged = flagged;
                messages.push(message);
            }
        }
        Ok(FolderBatch {
            uidvalidity: info.uidvalidity,
            messages,
            kind: if resume.is_some() {
                FetchKind::Resume
            } else {
                FetchKind::Fill
            },
            first_num: floor,
            // The answer is recorded either way: an empty mailbox is one.
            oldest: Some(oldest),
            // `save_messages` clears both marks when the generation moved, so
            // the new ones have to be written after it.
            marks_before_store: false,
        })
    }

    fn fetch_older(
        &mut self,
        folder: &str,
        mark: &FolderMark,
        batch: u32,
    ) -> MailResult<OlderBatch> {
        let info = self.client.select(folder)?;
        let oldest = self.client.oldest_uid()?;
        // The stored UIDs are the address of the mail this folder holds. Under
        // another UIDVALIDITY they address different mail, and a batch fetched
        // now would replace them with a slice out of the middle of the mailbox:
        // the sync is what rotates a folder, so this only says what it found.
        let same_generation = matches!(
            (mark.uidvalidity, info.uidvalidity),
            (Some(stored), Some(selected)) if stored == selected
        );
        if !same_generation {
            return Err(MailError::Protocol(
                "the folder changed on the server — sync it before loading older mail".to_string(),
            ));
        }
        let mut first_num = None;
        let mut messages = Vec::new();
        let mut available = false;
        let range = match (mark.first_num, oldest) {
            (Some(floor), Some(oldest)) => {
                older_uids(floor, oldest, batch).map(|range| (range, oldest))
            }
            _ => None,
        };
        if let Some(((low, high), oldest)) = range {
            let fetched = self.client.fetch_bodies(&format!("{low}:{high}"), true)?;
            for fetched in fetched {
                let (seen, flagged) = fetched_state(&fetched.flags);
                let mut message = mail::parse(&fetched.raw, &fetched.uid.to_string(), seen);
                message.flagged = flagged;
                messages.push(message);
            }
            // The floor moves whether or not the range held anything: it is
            // where this folder has been asked from, and a mailbox with gaps in
            // it would otherwise be asked for the same empty range forever.
            first_num = Some(low);
            available = low > oldest;
        }
        Ok(OlderBatch {
            uidvalidity: info.uidvalidity,
            messages,
            first_num,
            oldest: Some(oldest),
            available,
        })
    }

    fn body(&mut self, folder: &str, uid: &str) -> MailResult<Vec<u8>> {
        let number = imap_uid(uid)?;
        self.client.select(folder)?;
        self.client.uid_body(number)?.ok_or_else(mail_is_gone)
    }

    fn set_seen(&mut self, folder: &str, uid: &str) -> MailResult<()> {
        let number = imap_uid(uid)?;
        self.client.select(folder)?;
        self.client
            .uid_store_flags_silent(number, "+FLAGS.SILENT (\\Seen)")
    }

    fn set_flagged(&mut self, folder: &str, uid: &str, flagged: bool) -> MailResult<()> {
        let number = imap_uid(uid)?;
        let change = if flagged { "+FLAGS" } else { "-FLAGS" };
        self.client.select(folder)?;
        self.client
            .uid_store_flags_silent(number, &format!("{change}.SILENT (\\Flagged)"))
    }

    fn delete(&mut self, folder: &str, uids: &[String]) -> DeleteOutcome {
        let mut numbers = Vec::with_capacity(uids.len());
        let mut first_error = None;
        for uid in uids {
            match uid.parse::<u32>() {
                Ok(number) => numbers.push((uid.as_str(), number)),
                // A uid the protocol cannot address is a mail that cannot go,
                // and its local copy stays with it.
                Err(_) => {
                    first_error = first_error
                        .or_else(|| Some(MailError::Protocol(format!("not an IMAP UID: {uid}"))))
                }
            }
        }
        if numbers.is_empty() {
            return DeleteOutcome {
                gone: Vec::new(),
                error: first_error,
            };
        }
        let asked: Vec<u32> = numbers.iter().map(|(_, number)| *number).collect();
        let (gone, error) = self.client.delete_messages(folder, &asked);
        let first_error = first_error.or(error);
        let mut deleted = Vec::new();
        for (uid, number) in numbers {
            if gone.contains(&number) {
                deleted.push(uid.to_string());
            }
        }
        DeleteOutcome {
            gone: deleted,
            error: first_error,
        }
    }

    fn append(&mut self, mailbox: &str, raw: &[u8]) -> MailResult<()> {
        self.client.append(mailbox, raw)
    }

    fn logout(&mut self) {
        self.client.logout();
    }
}

/// An IMAP UID as the string the database stores it as.
fn imap_uid(uid: &str) -> MailResult<u32> {
    uid.parse::<u32>()
        .map_err(|_| MailError::Protocol(format!("not an IMAP UID: {uid}")))
}

/// The lowest 1-based position a first sync reaches: the newest
/// `BACKFILL_LIMIT` messages of `total`.
fn backfill_start(total: usize) -> usize {
    total.saturating_sub(BACKFILL_LIMIT as usize) + 1
}

/// The UID a fetch resumes from, or `None` when the folder has to be fetched
/// afresh. Both sides have to agree on the UIDVALIDITY generation: a mark from
/// another one describes mail that is gone, and a server that reports none does
/// not promise its UIDs survive the session — such a folder keeps the old
/// behaviour of refetching the newest messages rather than risking a skip.
fn resume_uid(mark: &FolderMark, selected: Option<u32>) -> Option<u32> {
    match (mark.uidvalidity, selected) {
        (Some(stored), Some(selected)) if stored == selected => {
            mark.uid_next.filter(|uid| *uid > 0)
        }
        _ => None,
    }
}

/// The UID range one click takes out of an IMAP folder: the batch below the
/// floor the folder has been fetched down to, or the server's oldest message,
/// whichever is closer — and `None` when there is nothing below the floor at
/// all, either because the floor is the first UID there is or because the
/// server's oldest message is one we already hold.
fn older_uids(floor: u32, oldest: u32, batch: u32) -> Option<(u32, u32)> {
    if floor <= 1 || oldest >= floor {
        return None;
    }
    Some((oldest.max(floor.saturating_sub(batch)), floor - 1))
}

/// The two per-mail states an IMAP `FETCH` reports in its `FLAGS`: whether the
/// mail has been read, and whether the reader flagged it. Both are the
/// *server's* answer only for a mail the database is storing for the first
/// time: a row that is already there keeps what the reader did with it.
fn fetched_state(flags: &[String]) -> (bool, bool) {
    let has = |name: &str| flags.iter().any(|flag| flag.eq_ignore_ascii_case(name));
    (has("\\Seen"), has("\\Flagged"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two states an IMAP `FETCH` reports are read off its `FLAGS`, and
    /// only those two: the rest of the list (`\Answered`, keywords, the
    /// folder's own `\Deleted`) is not something Kimo keeps. The comparison is
    /// the wire's, which is not case-sensitive, and a mail with no flags at all
    /// is neither read nor flagged.
    #[test]
    fn a_fetch_reports_its_two_states() {
        let flags = |list: &[&str]| list.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        assert_eq!(fetched_state(&flags(&["\\Seen"])), (true, false));
        assert_eq!(fetched_state(&flags(&["\\Flagged"])), (false, true));
        assert_eq!(
            fetched_state(&flags(&["\\Seen", "\\Flagged"])),
            (true, true)
        );
        assert_eq!(fetched_state(&flags(&["\\flagged"])), (false, true));
        assert_eq!(
            fetched_state(&flags(&["\\Answered", "$Important"])),
            (false, false)
        );
        assert_eq!(fetched_state(&[]), (false, false));
    }

    #[test]
    fn a_first_sync_takes_the_newest_messages() {
        assert_eq!(backfill_start(0), 1);
        assert_eq!(backfill_start(1), 1);
        assert_eq!(backfill_start(BACKFILL_LIMIT as usize), 1);
        assert_eq!(backfill_start(BACKFILL_LIMIT as usize + 1), 2);
        assert_eq!(backfill_start(5000), 5000 - BACKFILL_LIMIT as usize + 1);
    }

    /// What one `Load older mail` click asks an IMAP folder for: the batch
    /// below the floor, the floor being the oldest UID ever fetched. The
    /// server's own oldest message is where the walk stops — asking below it
    /// would be asking for mail that is not there — and a mailbox whose oldest
    /// message is one already held has nothing to load at all.
    #[test]
    fn a_click_asks_for_the_batch_below_the_floor() {
        // A floor in the middle of a mailbox: one batch, and no further.
        assert_eq!(older_uids(5000, 1, 50), Some((4950, 4999)));
        // A mailbox whose mail starts just below the floor: only what there is.
        assert_eq!(older_uids(5000, 4975, 50), Some((4975, 4999)));
        // And then the bottom, which is the end of the walk.
        assert_eq!(older_uids(4975, 4975, 50), None);
        assert_eq!(older_uids(4975, 4976, 50), None, "deleted elsewhere");
        assert_eq!(older_uids(1, 1, 50), None, "the floor is the first UID");
        // A floor of 2 asks for UID 1 alone, whatever the batch size.
        assert_eq!(older_uids(2, 1, 50), Some((1, 1)));
        // `older_uids` never proposes UID 0: the range starts at the first UID
        // the protocol has, whatever the arithmetic below the batch says.
        assert_eq!(older_uids(2, 1, 500), Some((1, 1)));
    }

    #[test]
    fn a_folder_resumes_from_its_mark_unless_the_uidvalidity_moved() {
        let mark = FolderMark {
            uidvalidity: Some(7),
            uid_next: Some(13),
            ..Default::default()
        };
        assert_eq!(resume_uid(&mark, Some(7)), Some(13));
        // Another generation: the stored UIDs describe mail that is gone.
        assert_eq!(resume_uid(&mark, Some(8)), None);
        // A server that reports no UIDVALIDITY promises nothing about its UIDs
        // surviving, so the folder is fetched afresh, as it always was.
        assert_eq!(resume_uid(&mark, None), None);

        // Nothing stored, a mark without a generation, or a nonsense UID.
        assert_eq!(resume_uid(&FolderMark::default(), Some(7)), None);
        let generation_only = FolderMark {
            uidvalidity: Some(7),
            ..Default::default()
        };
        assert_eq!(resume_uid(&generation_only, Some(7)), None);
        let nonsense = FolderMark {
            uidvalidity: Some(7),
            uid_next: Some(0),
            ..Default::default()
        };
        assert_eq!(resume_uid(&nonsense, Some(7)), None, "UID 0 is no UID");
    }
}
