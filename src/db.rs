//! The mail database: one SQLite file holding every account, every folder and
//! every cached message, with an FTS5 index over the mail.
//!
//! # Who writes
//!
//! Worker threads own every write (`crate::sync`): a sync job stores the folder
//! list and the batch it has just fetched, `Job::MarkSeen` stores the
//! `\Seen` flag, `Job::DeleteAccount` removes an account. The UI thread only
//! reads, once, at startup — no frame is ever blocked on the disk. Everything
//! goes through one connection behind a mutex (`db`); the lock is never held
//! across a socket.
//!
//! # Schema
//!
//! `PRAGMA user_version` carries the schema version: 0 means "no database yet",
//! so the DDL runs and the JSON of the old layout is imported once. `SCHEMA_SQL`
//! is the schema a *new* database is born with; a change to it also needs a step
//! in `migrate` that brings a database born before it to the same shape.
//!
//! `folders` carries each folder's sync marks as well — `uid_next` for IMAP,
//! `first_num` for POP3. They are what makes a sync fetch only the mail that is
//! not stored yet (see the sync marks section below and `crate::sync`).
//!
//! `messages_fts` is an external-content FTS5 table over `messages`: the three
//! triggers in `SCHEMA_SQL` keep the index in step, so nothing else writes to
//! it. The FTS column names are the content table's column names on purpose —
//! FTS5 reads the content table by name for `snippet()`, `rebuild` and the
//! integrity check. `cjk` holds the unigram/bigram stream of the message's CJK
//! runs (see `crate::search`), not text meant to be read.
//!
//! # The old JSON
//!
//! `accounts.json` and `cache/<account-id>.json` are read exactly once, at
//! `user_version` 0, and each file is deleted as soon as it has been imported.
//! Keep the mirror structs at the bottom of this file until no installation can
//! still predate the database, like `storage::migrate_legacy_dir`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

use crate::error::{MailError, MailResult};
use crate::models::{Account, InlineImage, MailMessage, Protocol, TlsMode};
use crate::storage::{self, Folder, Mailbox};

/// The schema version this build writes and understands.
pub const SCHEMA_VERSION: i64 = 3;

/// How long a statement waits for another writer before giving up. Jobs hold
/// the connection only around their SQL, so this is a backstop, not a plan.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The schema a new database is born with, created in one go at
/// `user_version` 0. Every statement is `IF NOT EXISTS`, so a database left
/// half-built by a crash (or by an import that failed) is simply finished on
/// the next start. A database that already existed is walked to the same shape
/// by the steps in `migrate`, one per version.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    protocol TEXT NOT NULL CHECK (protocol IN ('imap', 'pop3')),
    host     TEXT NOT NULL,
    port     INTEGER NOT NULL,
    username TEXT NOT NULL,
    -- Plain text, as it always was (see AGENTS.md); the file is mode 0600.
    password TEXT NOT NULL,
    tls      TEXT NOT NULL CHECK (tls IN ('implicit', 'start_tls', 'plain')),
    -- The navigation column's order. New accounts append; editing one keeps it.
    position INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS folders (
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,          -- IMAP wire form (modified UTF-7)
    uidvalidity INTEGER,                -- NULL for POP3
    -- How far this folder has been fetched; NULL until it has been synced once.
    -- IMAP: one past the highest UID stored, so every UID below it is here.
    -- POP3: the lowest message number this drop is fetched from — the first
    -- sync takes the newest messages and the older ones stay out of scope.
    uid_next    INTEGER,
    first_num   INTEGER,
    synced      INTEGER NOT NULL DEFAULT 0,
    -- 1 when the last `LIST` named this mailbox, 0 when the row is kept only
    -- for the mail cached for it — a mailbox deleted on the server keeps its
    -- mail here, and the column draws it as one the server no longer has.
    listed      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, name)
);

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY,
    account_id  TEXT NOT NULL,
    folder      TEXT NOT NULL,
    uid         TEXT NOT NULL,          -- IMAP UID as text, or the POP3 UIDL
    subject     TEXT NOT NULL DEFAULT '',
    from_addr   TEXT NOT NULL DEFAULT '',
    to_addr     TEXT NOT NULL DEFAULT '',
    date_header TEXT NOT NULL DEFAULT '',
    ts          INTEGER,                -- unix seconds (UTC), NULL if unparsable
    seen        INTEGER NOT NULL DEFAULT 0,
    body_text   TEXT NOT NULL DEFAULT '',
    body_html   TEXT,
    preview     TEXT NOT NULL DEFAULT '',
    cjk         TEXT NOT NULL DEFAULT '',
    UNIQUE (account_id, folder, uid),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    FOREIGN KEY (account_id, folder) REFERENCES folders(account_id, name) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS messages_folder ON messages (account_id, folder, ts DESC);
CREATE INDEX IF NOT EXISTS messages_recent ON messages (ts DESC);
CREATE INDEX IF NOT EXISTS messages_unread ON messages (account_id, folder) WHERE seen = 0;

CREATE TABLE IF NOT EXISTS attachments (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ordinal    INTEGER NOT NULL,
    name       TEXT NOT NULL,
    PRIMARY KEY (message_id, ordinal)
);

CREATE TABLE IF NOT EXISTS inline_images (
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    cid          TEXT NOT NULL,
    content_type TEXT NOT NULL,
    data         BLOB NOT NULL,
    PRIMARY KEY (message_id, cid)
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    subject, from_addr, to_addr, body_text, cjk,
    content = 'messages',
    content_rowid = 'id',
    tokenize = "unicode61 remove_diacritics 2"
);

CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, subject, from_addr, to_addr, body_text, cjk)
    VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.body_text, new.cjk);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, subject, from_addr, to_addr, body_text, cjk)
    VALUES ('delete', old.id, old.subject, old.from_addr, old.to_addr, old.body_text, old.cjk);
END;

-- Only an actual content change re-indexes: without the WHEN guard, marking a
-- mail read — and every POP3 re-sync, which re-downloads the same bodies —
-- would rewrite the FTS row for nothing.
CREATE TRIGGER IF NOT EXISTS messages_fts_au
AFTER UPDATE OF subject, from_addr, to_addr, body_text, cjk ON messages
WHEN old.subject IS NOT new.subject
  OR old.from_addr IS NOT new.from_addr
  OR old.to_addr IS NOT new.to_addr
  OR old.body_text IS NOT new.body_text
  OR old.cjk IS NOT new.cjk
BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, subject, from_addr, to_addr, body_text, cjk)
    VALUES ('delete', old.id, old.subject, old.from_addr, old.to_addr, old.body_text, old.cjk);
    INSERT INTO messages_fts (rowid, subject, from_addr, to_addr, body_text, cjk)
    VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.body_text, new.cjk);
END;
"#;

/// The step from `user_version` 1 to 2: a sync mark per folder. An existing
/// database then gets its IMAP marks derived from the UIDs it already holds
/// (`derive_imap_marks`), so the upgrade does not refetch a batch it has.
const MIGRATE_V2_SQL: &str = r#"
ALTER TABLE folders ADD COLUMN uid_next  INTEGER;
ALTER TABLE folders ADD COLUMN first_num INTEGER;
"#;

/// The step from `user_version` 2 to 3: whether a folder is one the server
/// still lists. A database from before the step cannot know, and every folder
/// in it came from a `LIST` in the first place, so they all start listed —
/// which is also what the column drew before. The next sync records the truth.
const MIGRATE_V3_SQL: &str = r#"
ALTER TABLE folders ADD COLUMN listed INTEGER NOT NULL DEFAULT 1;
"#;

/// IMAP's sync mark as a function of what is stored: one past the highest UID
/// the folder holds, clamped to the UID space (a stored `u32::MAX` would not
/// otherwise fit the column). Both `save_messages` and the version-1 step of
/// `migrate` derive the mark with this, so the two can never disagree. A POP3
/// folder is left alone by the protocol test inside it: its `uid` is a UIDL
/// string, not a number.
const DERIVE_UID_NEXT_SQL: &str = r#"
UPDATE folders SET uid_next = (
        SELECT MIN(MAX(CAST(m.uid AS INTEGER)) + 1, 4294967295)
          FROM messages m
         WHERE m.account_id = ?1 AND m.folder = ?2
    )
  WHERE account_id = ?1 AND name = ?2
    AND (SELECT protocol FROM accounts WHERE id = ?1) = 'imap'
"#;

/// The message upsert. `seen` only ever grows locally (the `\Seen` the server
/// reports is not a source of truth for mail already shown as read), and the
/// `WHERE` clause makes an unchanged message a no-op: no row comes back, so
/// neither the FTS triggers nor the child rows below are touched.
const MESSAGE_UPSERT: &str = r#"
INSERT INTO messages (
    account_id, folder, uid, subject, from_addr, to_addr, date_header, ts,
    seen, body_text, body_html, preview, cjk
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
ON CONFLICT(account_id, folder, uid) DO UPDATE SET
    subject     = excluded.subject,
    from_addr   = excluded.from_addr,
    to_addr     = excluded.to_addr,
    date_header = excluded.date_header,
    ts          = excluded.ts,
    body_text   = excluded.body_text,
    body_html   = excluded.body_html,
    preview     = excluded.preview,
    cjk         = excluded.cjk,
    seen        = max(messages.seen, excluded.seen)
WHERE messages.seen IS NOT excluded.seen
   OR messages.subject IS NOT excluded.subject
   OR messages.from_addr IS NOT excluded.from_addr
   OR messages.to_addr IS NOT excluded.to_addr
   OR messages.date_header IS NOT excluded.date_header
   OR messages.ts IS NOT excluded.ts
   OR messages.body_text IS NOT excluded.body_text
   OR messages.body_html IS NOT excluded.body_html
   OR messages.preview IS NOT excluded.preview
   OR messages.cjk IS NOT excluded.cjk
RETURNING id
"#;

// ----- the connection --------------------------------------------------------

/// The process-wide database, opened on first use.
static DB: OnceLock<Mutex<MailDb>> = OnceLock::new();

struct MailDb {
    /// `None` only when neither the file nor even an in-memory database could
    /// be opened — an unusable SQLite, or no memory at all.
    conn: Option<Connection>,
    /// Why the file could not be opened, when the app is running on an
    /// in-memory database: nothing will be saved.
    warning: Option<String>,
}

impl MailDb {
    fn open() -> Self {
        let path = storage::mail_db_path();
        match open_file(&path) {
            Ok(conn) => Self {
                conn: Some(conn),
                warning: None,
            },
            Err(error) => {
                let warning = format!("{}: {error}", path.display());
                // An in-memory database keeps the session usable: syncing,
                // reading and searching all work, nothing reaches the disk.
                // The legacy JSON is deliberately *not* imported into it — the
                // import deletes what it reads, and this database is gone when
                // the window closes.
                let memory = Connection::open_in_memory()
                    .map_err(MailError::from)
                    .and_then(|conn| {
                        configure(&conn)?;
                        Ok(conn)
                    })
                    .and_then(|mut conn| {
                        migrate(&mut conn, None)?;
                        Ok(conn)
                    });
                match memory {
                    Ok(conn) => Self {
                        conn: Some(conn),
                        warning: Some(warning),
                    },
                    Err(memory_error) => Self {
                        conn: None,
                        warning: Some(format!("{warning}; in-memory fallback: {memory_error}")),
                    },
                }
            }
        }
    }
}

/// A locked handle on the mail database.
pub struct Db {
    guard: MutexGuard<'static, MailDb>,
}

impl Db {
    /// The connection; infallible once [`db`] returned `Ok`.
    pub fn conn(&self) -> &Connection {
        self.guard
            .conn
            .as_ref()
            .expect("db() checked the connection")
    }

    fn conn_mut(&mut self) -> &mut Connection {
        self.guard
            .conn
            .as_mut()
            .expect("db() checked the connection")
    }
}

fn handle() -> &'static Mutex<MailDb> {
    DB.get_or_init(|| Mutex::new(MailDb::open()))
}

fn lock() -> MutexGuard<'static, MailDb> {
    // A panic in another thread while it held the lock says nothing about the
    // database: the connection is still perfectly usable.
    handle()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Open (once per process) and lock the mail database.
pub fn db() -> MailResult<Db> {
    let guard = lock();
    if guard.conn.is_none() {
        return Err(MailError::Storage(format!(
            "mail database unavailable ({})",
            guard.warning.as_deref().unwrap_or("unknown error")
        )));
    }
    Ok(Db { guard })
}

/// Why the mail database is not being written to disk, when that is the case —
/// the UI shows it once, at startup.
pub fn warning() -> Option<String> {
    lock().warning.clone()
}

fn open_file(path: &Path) -> MailResult<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    restrict_permissions(path);
    configure(&conn)?;
    migrate(
        &mut conn,
        Some((&storage::config_dir(), &storage::cache_dir())),
    )?;
    Ok(conn)
}

/// The database holds the passwords, so it is not for other users to read.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

fn configure(conn: &Connection) -> MailResult<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // WAL lives in the file; `foreign_keys` is per connection and so has to be
    // set every time. `synchronous = NORMAL` is the usual WAL pairing: a commit
    // does not fsync, a checkpoint does.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}

/// Create the schema and, on a fresh database, import the JSON of the old
/// layout. `legacy` is `None` for an in-memory database, which has no business
/// deleting files it reads.
///
/// A database that already existed is walked forward one version at a time,
/// each step one transaction with the `user_version` write inside it (see the
/// ladder below).
fn migrate(conn: &mut Connection, legacy: Option<(&Path, &Path)>) -> MailResult<()> {
    let start: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if start >= SCHEMA_VERSION {
        return Ok(());
    }
    if start == 0 {
        // Born whole: the schema a new database is created with already has
        // everything the steps below add, so it skips them. Every statement is
        // `IF NOT EXISTS`, which is what lets a database whose import failed —
        // its version stays 0 — walk this path again.
        conn.execute_batch(SCHEMA_SQL)?;
        if let Some((config_dir, cache_dir)) = legacy {
            if let Err(error) = import_legacy_json(conn, config_dir, cache_dir) {
                // A half-imported database is not a broken one: leave the
                // version at 0 so the next start finishes the job (the import
                // upserts each message and deletes each file it has read).
                eprintln!(
                    "kimo: importing the old JSON storage failed, retrying on the next start: {error}"
                );
                return Ok(());
            }
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }

    // A database that already existed walks the ladder, one step per version.
    // Each step is one transaction, `user_version` included — the version and
    // the schema it describes are written together, so a crash rolls the whole
    // step back and the next start runs it again instead of finding a database
    // half a step forward. Later steps go below, in order.
    if start < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATE_V2_SQL)?;
        derive_imap_marks(&tx)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    if start < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATE_V3_SQL)?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
    }
    Ok(())
}

/// Give every IMAP folder the mark its stored mail implies. A database born
/// before version 2 has the UIDs but not the mark; deriving one is what keeps
/// the upgrade from refetching a batch it already holds.
fn derive_imap_marks(conn: &Connection) -> MailResult<()> {
    let folders: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT account_id, name FROM folders
              WHERE (SELECT protocol FROM accounts WHERE id = folders.account_id) = 'imap'",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut folders = Vec::new();
        for row in rows {
            folders.push(row?);
        }
        folders
    };
    let mut derive = conn.prepare(DERIVE_UID_NEXT_SQL)?;
    for (account_id, folder) in &folders {
        derive.execute(params![account_id, folder])?;
    }
    Ok(())
}

// ----- accounts --------------------------------------------------------------

/// Every configured account, in the navigation column's order.
pub fn load_accounts() -> MailResult<Vec<Account>> {
    let db = db()?;
    load_accounts_on(db.conn())
}

fn load_accounts_on(conn: &Connection) -> MailResult<Vec<Account>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, protocol, host, port, username, password, tls
           FROM accounts ORDER BY position, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    let mut accounts = Vec::new();
    for row in rows {
        let (id, name, protocol, host, port, username, password, tls) = row?;
        let protocol = Protocol::parse(&protocol).ok_or_else(|| {
            MailError::Storage(format!("account {id}: unknown protocol {protocol:?}"))
        })?;
        let tls = TlsMode::parse(&tls)
            .ok_or_else(|| MailError::Storage(format!("account {id}: unknown TLS mode {tls:?}")))?;
        let port = u16::try_from(port)
            .map_err(|_| MailError::Storage(format!("account {id}: port {port} out of range")))?;
        accounts.push(Account {
            id,
            name,
            protocol,
            host,
            port,
            username,
            password,
            tls,
        });
    }
    Ok(accounts)
}

/// Insert an account, or update the one with the same id in place. An edit
/// keeps the account's `position`, so renaming it neither moves it in the
/// navigation column nor touches its cached mail.
pub fn save_account(account: &Account) -> MailResult<()> {
    let db = db()?;
    save_account_on(db.conn(), account)
}

fn save_account_on(conn: &Connection, account: &Account) -> MailResult<()> {
    conn.execute(
        "INSERT INTO accounts (id, name, protocol, host, port, username, password, tls, position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                 (SELECT COALESCE(MAX(position), -1) + 1 FROM accounts))
         ON CONFLICT(id) DO UPDATE SET
             name     = excluded.name,
             protocol = excluded.protocol,
             host     = excluded.host,
             port     = excluded.port,
             username = excluded.username,
             password = excluded.password,
             tls      = excluded.tls",
        params![
            account.id,
            account.name,
            account.protocol.as_str(),
            account.host,
            account.port,
            account.username,
            account.password,
            account.tls.as_str(),
        ],
    )?;
    Ok(())
}

/// Remove an account and, through the cascades, everything cached for it.
pub fn delete_account(id: &str) -> MailResult<()> {
    let db = db()?;
    delete_account_on(db.conn(), id)
}

fn delete_account_on(conn: &Connection, id: &str) -> MailResult<()> {
    conn.execute("DELETE FROM accounts WHERE id = ?1", [id])?;
    Ok(())
}

/// Forget an account's cached mail while keeping the account itself: the
/// endpoint (protocol, host, port, username) changed, so what is stored
/// describes a mailbox these settings no longer reach.
pub fn purge_mail(account_id: &str) -> MailResult<()> {
    let mut db = db()?;
    purge_mail_on(db.conn_mut(), account_id)
}

fn purge_mail_on(conn: &mut Connection, account_id: &str) -> MailResult<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM messages WHERE account_id = ?1", [account_id])?;
    tx.execute("DELETE FROM folders WHERE account_id = ?1", [account_id])?;
    tx.commit()?;
    Ok(())
}

// ----- sync marks ------------------------------------------------------------

/// What the database remembers about a folder's last sync — the marks that let
/// a sync fetch what is not stored yet instead of refetching a batch every time
/// (see `crate::sync`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderSync {
    /// The IMAP generation `uid_next` belongs to: UIDs stored under another
    /// UIDVALIDITY describe mail that is no longer there.
    pub uidvalidity: Option<u32>,
    /// IMAP: one past the highest UID stored, and therefore the first UID a
    /// sync asks for. `save_messages` keeps it in step with the rows.
    pub uid_next: Option<u32>,
    /// POP3: the lowest message number this drop is synced from. The first sync
    /// of a drop picks it (`sync::BACKFILL_LIMIT` newest messages); the mail
    /// below it is deliberately out of scope.
    pub first_num: Option<u32>,
}

/// What one folder remembers. A folder the database does not know, or one that
/// has never been fetched, reads as the default — every mark `None`, which is
/// what tells a sync to start from scratch.
pub fn folder_sync(account_id: &str, folder: &str) -> MailResult<FolderSync> {
    let db = db()?;
    folder_sync_on(db.conn(), account_id, folder)
}

fn folder_sync_on(conn: &Connection, account_id: &str, folder: &str) -> MailResult<FolderSync> {
    let mark = conn
        .query_row(
            "SELECT uidvalidity, uid_next, first_num FROM folders
              WHERE account_id = ?1 AND name = ?2",
            params![account_id, folder],
            |row| {
                Ok(FolderSync {
                    uidvalidity: row.get(0)?,
                    uid_next: row.get(1)?,
                    first_num: row.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(mark.unwrap_or_default())
}

/// POP3 only: remember the message number the drop is synced from. Written
/// *before* the batch it belongs to, so the window is never lost: a folder with
/// a floor and no mail is simply fetched from that floor next time, which can
/// cost a repeated pass but never a message.
pub fn set_first_num(account_id: &str, folder: &str, first_num: u32) -> MailResult<()> {
    let db = db()?;
    set_first_num_on(db.conn(), account_id, folder, first_num)
}

fn set_first_num_on(
    conn: &Connection,
    account_id: &str,
    folder: &str,
    first_num: u32,
) -> MailResult<()> {
    conn.execute(
        // A POP3 drop has no `LIST` to be listed by, and this is its INBOX: a
        // folder created by fetching mail is one the server has.
        "INSERT INTO folders (account_id, name, uidvalidity, synced, first_num, listed)
         VALUES (?1, ?2, NULL, 0, ?3, 1)
         ON CONFLICT(account_id, name) DO UPDATE SET first_num = excluded.first_num",
        params![account_id, folder, first_num],
    )?;
    Ok(())
}

/// The folders of one account that have been fetched at least once — what a
/// sync catches up on. A folder that was never opened is left out: its first
/// fetch is where the backfill belongs.
pub fn synced_folders(account_id: &str) -> MailResult<Vec<String>> {
    let db = db()?;
    synced_folders_on(db.conn(), account_id)
}

fn synced_folders_on(conn: &Connection, account_id: &str) -> MailResult<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM folders WHERE account_id = ?1 AND synced = 1 ORDER BY name")?;
    let rows = stmt.query_map([account_id], |row| row.get::<_, String>(0))?;
    let mut folders = Vec::new();
    for row in rows {
        folders.push(row?);
    }
    Ok(folders)
}

/// Every `uid` stored for one folder. POP3 is what needs this: a UIDL is the
/// only identity a message has there, so a sync asks for the ones it does not
/// already hold.
pub fn stored_uids(account_id: &str, folder: &str) -> MailResult<HashSet<String>> {
    let db = db()?;
    stored_uids_on(db.conn(), account_id, folder)
}

fn stored_uids_on(
    conn: &Connection,
    account_id: &str,
    folder: &str,
) -> MailResult<HashSet<String>> {
    let mut stmt =
        conn.prepare("SELECT uid FROM messages WHERE account_id = ?1 AND folder = ?2")?;
    let rows = stmt.query_map(params![account_id, folder], |row| row.get::<_, String>(0))?;
    let mut uids = HashSet::new();
    for row in rows {
        uids.insert(row?);
    }
    Ok(uids)
}

// ----- mail ------------------------------------------------------------------

/// Every folder and message of the given accounts, newest first — the working
/// set the UI keeps in memory.
pub fn load_mailboxes(account_ids: &[String]) -> MailResult<HashMap<String, Mailbox>> {
    let db = db()?;
    load_mailboxes_on(db.conn(), account_ids)
}

fn load_mailboxes_on(
    conn: &Connection,
    account_ids: &[String],
) -> MailResult<HashMap<String, Mailbox>> {
    let mut mailboxes: HashMap<String, Mailbox> = account_ids
        .iter()
        .map(|id| (id.clone(), Mailbox::default()))
        .collect();
    if mailboxes.is_empty() {
        return Ok(mailboxes);
    }

    {
        let mut stmt = conn.prepare(
            "SELECT account_id, name, uidvalidity, synced, listed FROM folders ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<u32>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (account_id, name, uidvalidity, synced, listed) = row?;
            if let Some(mailbox) = mailboxes.get_mut(&account_id) {
                mailbox.folders.push(Folder {
                    name,
                    uidvalidity,
                    synced: synced != 0,
                    listed: listed != 0,
                    messages: Vec::new(),
                });
            }
        }
    }

    // Messages, then their attachments and inline images. Loading the child
    // rows per message would be a query per message; the id → index map keeps
    // it at three queries for the whole database.
    let mut loaded: Vec<(i64, String, String, MailMessage)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, account_id, folder, uid, subject, from_addr, to_addr, date_header,
                    ts, seen, body_text, body_html, preview
               FROM messages ORDER BY ts DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                MailMessage {
                    uid: row.get(3)?,
                    subject: row.get(4)?,
                    from: row.get(5)?,
                    to: row.get(6)?,
                    date: row.get(7)?,
                    timestamp: row.get(8)?,
                    seen: row.get::<_, i64>(9)? != 0,
                    body_text: row.get(10)?,
                    body_html: row.get(11)?,
                    preview: row.get(12)?,
                    attachment_names: Vec::new(),
                    inline_images: Vec::new(),
                },
            ))
        })?;
        for row in rows {
            loaded.push(row?);
        }
    }
    let positions: HashMap<i64, usize> = loaded
        .iter()
        .enumerate()
        .map(|(index, (id, _, _, _))| (*id, index))
        .collect();
    {
        let mut stmt =
            conn.prepare("SELECT message_id, name FROM attachments ORDER BY message_id, ordinal")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (message_id, name) = row?;
            if let Some(&index) = positions.get(&message_id) {
                loaded[index].3.attachment_names.push(name);
            }
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT message_id, cid, content_type, data FROM inline_images
              ORDER BY message_id, cid",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                InlineImage {
                    cid: row.get(1)?,
                    content_type: row.get(2)?,
                    data: row.get(3)?,
                },
            ))
        })?;
        for row in rows {
            let (message_id, image) = row?;
            if let Some(&index) = positions.get(&message_id) {
                loaded[index].3.inline_images.push(image);
            }
        }
    }

    for (_, account_id, folder_name, message) in loaded {
        let Some(mailbox) = mailboxes.get_mut(&account_id) else {
            continue;
        };
        if let Some(folder) = mailbox.folders.iter_mut().find(|f| f.name == folder_name) {
            folder.messages.push(message);
        }
    }
    Ok(mailboxes)
}

/// Remember the mailboxes an IMAP `LIST` returned. Folders are not deleted when
/// they disappear from the server: their cached mail is still the only copy of
/// it this machine has. What the answer does change is which of them count as
/// listed — it is the server's current mailbox list, so a folder it left out is
/// marked as one it no longer has.
pub fn save_folder_list(account_id: &str, folders: &[String]) -> MailResult<()> {
    let mut db = db()?;
    save_folder_list_on(db.conn_mut(), account_id, folders)
}

fn save_folder_list_on(
    conn: &mut Connection,
    account_id: &str,
    folders: &[String],
) -> MailResult<()> {
    let tx = conn.transaction()?;
    {
        // The answer replaces the record, so the folders missing from it go
        // first. Everything else about the row — the sync marks, the mail —
        // stays where it was: only what the server *lists* is in question here.
        tx.execute(
            "UPDATE folders SET listed = 0 WHERE account_id = ?1",
            params![account_id],
        )?;
        let mut stmt = tx.prepare(
            "INSERT INTO folders (account_id, name, uidvalidity, synced, listed)
             VALUES (?1, ?2, NULL, 0, 1)
             ON CONFLICT(account_id, name) DO UPDATE SET listed = 1",
        )?;
        for folder in folders {
            stmt.execute(params![account_id, folder])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The folders the last `LIST` named for one account. A sync whose `LIST` failed
/// falls back on this: they are the mailboxes it knows the server has, and so
/// the ones worth trying. A folder the answer left out is deliberately not in
/// it — it is already known to be gone.
pub fn listed_folders(account_id: &str) -> MailResult<Vec<String>> {
    let db = db()?;
    listed_folders_on(db.conn(), account_id)
}

fn listed_folders_on(conn: &Connection, account_id: &str) -> MailResult<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM folders WHERE account_id = ?1 AND listed = 1 ORDER BY name")?;
    let rows = stmt.query_map([account_id], |row| row.get::<_, String>(0))?;
    let mut folders = Vec::new();
    for row in rows {
        folders.push(row?);
    }
    Ok(folders)
}

/// Store a freshly fetched batch for one folder. For an IMAP account the same
/// transaction moves the folder's mark to one past the highest UID stored, so a
/// sync can only ever advance past mail that reached the disk.
pub fn save_messages(
    account_id: &str,
    folder: &str,
    uidvalidity: Option<u32>,
    messages: &[MailMessage],
) -> MailResult<()> {
    let mut db = db()?;
    save_messages_on(db.conn_mut(), account_id, folder, uidvalidity, messages)
}

fn save_messages_on(
    conn: &mut Connection,
    account_id: &str,
    folder: &str,
    uidvalidity: Option<u32>,
    messages: &[MailMessage],
) -> MailResult<()> {
    let tx = conn.transaction()?;

    // A new UIDVALIDITY generation means the stored UIDs describe different
    // mail, so that folder's rows go before the batch lands. `None` on either
    // side (POP3, or a folder fetched for the first time) rotates nothing.
    let stored: Option<u32> = tx
        .query_row(
            "SELECT uidvalidity FROM folders WHERE account_id = ?1 AND name = ?2",
            params![account_id, folder],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if matches!((stored, uidvalidity), (Some(old), Some(new)) if old != new) {
        tx.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder],
        )?;
    }

    tx.execute(
        // Mail was just fetched for it, so the server has this mailbox: the row
        // ends up listed either way. That is also what the column does with a
        // folder a fetch brought mail for, so the two agree across a restart.
        "INSERT INTO folders (account_id, name, uidvalidity, synced, listed)
         VALUES (?1, ?2, ?3, 1, 1)
         ON CONFLICT(account_id, name) DO UPDATE SET
             uidvalidity = excluded.uidvalidity, synced = 1, listed = 1",
        params![account_id, folder, uidvalidity],
    )?;

    {
        let mut upsert = tx.prepare(MESSAGE_UPSERT)?;
        let mut clear_attachments = tx.prepare("DELETE FROM attachments WHERE message_id = ?1")?;
        let mut add_attachment =
            tx.prepare("INSERT INTO attachments (message_id, ordinal, name) VALUES (?1, ?2, ?3)")?;
        let mut clear_images = tx.prepare("DELETE FROM inline_images WHERE message_id = ?1")?;
        let mut add_image = tx.prepare(
            "INSERT INTO inline_images (message_id, cid, content_type, data)
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        for message in messages {
            let cjk = crate::search::cjk_column(message);
            let id: Option<i64> = upsert
                .query_row(
                    params![
                        account_id,
                        folder,
                        message.uid,
                        message.subject,
                        message.from,
                        message.to,
                        message.date,
                        message.timestamp,
                        message.seen as i64,
                        message.body_text,
                        message.body_html,
                        message.preview,
                        cjk,
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            // Nothing came back: the message is already stored exactly like
            // this, so its child rows are still correct.
            let Some(id) = id else {
                continue;
            };
            clear_attachments.execute([id])?;
            for (ordinal, name) in message.attachment_names.iter().enumerate() {
                add_attachment.execute(params![id, ordinal as i64, name])?;
            }
            clear_images.execute([id])?;
            for image in &message.inline_images {
                add_image.execute(params![id, image.cid, image.content_type, image.data])?;
            }
        }
    }

    // The mark is a function of what is stored, not of the batch: a batch that
    // changed nothing (the server answered a UID set with a message already
    // held) cannot move it, and a UIDVALIDITY rotation — whose rows were
    // deleted above — starts the folder over from the new batch.
    tx.execute(DERIVE_UID_NEXT_SQL, params![account_id, folder])?;

    tx.commit()?;
    Ok(())
}

/// Mark one message as read locally. This is the only flag the client keeps:
/// POP3 has no server-side flag at all, and IMAP's is pushed separately.
pub fn set_seen(account_id: &str, folder: &str, uid: &str) -> MailResult<()> {
    let db = db()?;
    set_seen_on(db.conn(), account_id, folder, uid)
}

fn set_seen_on(conn: &Connection, account_id: &str, folder: &str, uid: &str) -> MailResult<()> {
    conn.execute(
        "UPDATE messages SET seen = 1 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
        params![account_id, folder, uid],
    )?;
    Ok(())
}

/// Delete one message: its row, and through the cascades and the FTS trigger
/// its attachments, its inline images and its index entry.
///
/// The folder's sync mark is deliberately left where it is. The mail is gone
/// from the server as well — the caller deletes it there first — so a mark
/// that walked back to it would only make the next sync fetch it again.
pub fn delete_message(account_id: &str, folder: &str, uid: &str) -> MailResult<()> {
    let db = db()?;
    delete_message_on(db.conn(), account_id, folder, uid)
}

fn delete_message_on(
    conn: &Connection,
    account_id: &str,
    folder: &str,
    uid: &str,
) -> MailResult<()> {
    conn.execute(
        "DELETE FROM messages WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
        params![account_id, folder, uid],
    )?;
    Ok(())
}

/// Check the database file and the FTS index. Every mutation test ends with
/// this, so a trigger that stopped firing or a missed `cascade` cannot go
/// unnoticed.
#[cfg(test)]
fn integrity_check_on(conn: &Connection) -> MailResult<()> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(MailError::Storage(format!("integrity_check: {result}")));
    }
    conn.execute_batch("INSERT INTO messages_fts (messages_fts) VALUES ('integrity-check');")?;
    Ok(())
}

// ----- the old JSON ----------------------------------------------------------

/// Import `accounts.json` and then every `cache/<account-id>.json`, deleting
/// each file once it is in the database. Runs once, at `user_version` 0.
fn import_legacy_json(
    conn: &mut Connection,
    config_dir: &Path,
    cache_dir: &Path,
) -> MailResult<()> {
    let accounts_json = config_dir.join("accounts.json");
    if accounts_json.exists() {
        let text = fs::read_to_string(&accounts_json)?;
        let legacy: Vec<LegacyAccount> = serde_json::from_str(&text)?;
        for account in &legacy {
            save_account_on(conn, &account.to_account()?)?;
        }
        fs::remove_file(&accounts_json)?;
        eprintln!(
            "kimo: moved {} account(s) from {} into the mail database",
            legacy.len(),
            accounts_json.display()
        );
    }

    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        // No cache directory: nothing was ever fetched on this machine.
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(account_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // Only for accounts that exist: a cache file left behind by a removed
        // account must not resurrect its mail.
        let known: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM accounts WHERE id = ?1)",
            [account_id],
            |row| row.get(0),
        )?;
        if !known {
            eprintln!(
                "kimo: leaving {} alone, no account with that id",
                path.display()
            );
            continue;
        }
        let text = fs::read_to_string(&path)?;
        let legacy: LegacyMailbox = match serde_json::from_str(&text) {
            Ok(mailbox) => mailbox,
            // A damaged file is the user's, not ours to delete: it simply
            // never becomes part of the database.
            Err(error) => {
                eprintln!(
                    "kimo: cannot read the old cache {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        for folder in legacy.folders {
            let messages = folder
                .messages
                .into_iter()
                .map(LegacyMessage::into_message)
                .collect::<MailResult<Vec<_>>>()?;
            save_messages_on(
                conn,
                account_id,
                &folder.name,
                folder.uidvalidity,
                &messages,
            )?;
            // The old cache could hold a folder that was never synced: it is
            // only there so the navigation column can show its name.
            if !folder.synced {
                conn.execute(
                    "UPDATE folders SET synced = 0 WHERE account_id = ?1 AND name = ?2",
                    params![account_id, folder.name],
                )?;
            }
        }
        fs::remove_file(&path)?;
        eprintln!(
            "kimo: imported the cached mail of {account_id} from {}",
            path.display()
        );
    }
    Ok(())
}

/// The account list of the old `accounts.json`.
#[derive(Deserialize)]
struct LegacyAccount {
    id: String,
    name: String,
    protocol: String,
    host: String,
    port: u16,
    username: String,
    password: String,
    tls: String,
}

impl LegacyAccount {
    fn to_account(&self) -> MailResult<Account> {
        let protocol = Protocol::parse(&self.protocol)
            .ok_or_else(|| MailError::Storage(format!("account {}: unknown protocol", self.id)))?;
        let tls = TlsMode::parse(&self.tls)
            .ok_or_else(|| MailError::Storage(format!("account {}: unknown TLS mode", self.id)))?;
        Ok(Account {
            id: self.id.clone(),
            name: self.name.clone(),
            protocol,
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password: self.password.clone(),
            tls,
        })
    }
}

/// One `cache/<account-id>.json`. Every field is defaulted, so a file written
/// by an older build still reads.
#[derive(Deserialize)]
struct LegacyMailbox {
    #[serde(default)]
    folders: Vec<LegacyFolder>,
}

#[derive(Deserialize)]
struct LegacyFolder {
    name: String,
    #[serde(default)]
    uidvalidity: Option<u32>,
    #[serde(default)]
    synced: bool,
    #[serde(default)]
    messages: Vec<LegacyMessage>,
}

#[derive(Deserialize)]
struct LegacyMessage {
    uid: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    seen: bool,
    #[serde(default)]
    body_text: String,
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    preview: String,
    #[serde(default)]
    attachment_names: Vec<String>,
    #[serde(default)]
    inline_images: Vec<LegacyInlineImage>,
}

impl LegacyMessage {
    fn into_message(self) -> MailResult<MailMessage> {
        let mut inline_images = Vec::new();
        for image in self.inline_images {
            let data = base64::engine::general_purpose::STANDARD
                .decode(image.data_base64.as_bytes())
                .map_err(|error| {
                    MailError::Storage(format!("inline image {}: {error}", image.cid))
                })?;
            inline_images.push(InlineImage {
                cid: image.cid,
                content_type: image.content_type,
                data,
            });
        }
        Ok(MailMessage {
            uid: self.uid,
            subject: self.subject,
            from: self.from,
            to: self.to,
            date: self.date,
            timestamp: self.timestamp,
            seen: self.seen,
            body_text: self.body_text,
            body_html: self.body_html,
            preview: self.preview,
            attachment_names: self.attachment_names,
            inline_images,
        })
    }
}

/// Inline images were base64 in JSON; in the database they are BLOBs.
#[derive(Deserialize)]
struct LegacyInlineImage {
    cid: String,
    content_type: String,
    data_base64: String,
}

/// Test scaffolding shared with `crate::search`: a throwaway database and the
/// two ways to put mail in it.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::path::PathBuf;

    /// The account every test database gets.
    pub(crate) const ACCOUNT: &str = "work";

    /// A database of its own in the temp directory, schema and all.
    pub(crate) fn temp_db(name: &str) -> (Connection, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kimo-db-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut conn = Connection::open(dir.join("mail.db")).unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn, None).unwrap();
        (conn, dir)
    }

    /// The account `save` stores into.
    fn account(id: &str) -> Account {
        Account {
            id: id.to_string(),
            name: format!("Account {id}"),
            protocol: Protocol::Imap,
            host: "imap.example.com".to_string(),
            port: 993,
            username: format!("{id}@example.com"),
            password: "secret".to_string(),
            tls: TlsMode::Implicit,
        }
    }

    /// Store a batch for `ACCOUNT` — the public `save_messages` wants the
    /// process-wide connection, which tests deliberately do not use.
    pub(crate) fn save(
        conn: &mut Connection,
        folder: &str,
        uidvalidity: Option<u32>,
        messages: &[MailMessage],
    ) -> MailResult<()> {
        save_as(conn, ACCOUNT, folder, uidvalidity, messages)
    }

    /// Store a batch for an account of the caller's choosing, creating it.
    pub(crate) fn save_as(
        conn: &mut Connection,
        account_id: &str,
        folder: &str,
        uidvalidity: Option<u32>,
        messages: &[MailMessage],
    ) -> MailResult<()> {
        save_account_on(conn, &account(account_id))?;
        save_messages_on(conn, account_id, folder, uidvalidity, messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A database of its own in the temp directory, schema and all. Each test
    /// takes a distinct name: they run in parallel inside one process.
    fn test_db(name: &str) -> (Connection, PathBuf) {
        super::test_support::temp_db(name)
    }

    fn account(id: &str) -> Account {
        Account {
            id: id.to_string(),
            name: format!("Account {id}"),
            protocol: Protocol::Imap,
            host: "imap.example.com".to_string(),
            port: 993,
            username: format!("{id}@example.com"),
            password: "secret".to_string(),
            tls: TlsMode::Implicit,
        }
    }

    fn message(uid: &str, subject: &str, body: &str) -> MailMessage {
        MailMessage {
            uid: uid.to_string(),
            subject: subject.to_string(),
            from: "Alice <alice@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "Mon, 1 Jan 2026 00:00:00 +0000".to_string(),
            timestamp: Some(1_767_225_600),
            seen: false,
            body_text: body.to_string(),
            body_html: None,
            preview: body.chars().take(20).collect(),
            attachment_names: Vec::new(),
            inline_images: Vec::new(),
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    /// The folders of one account the server is recorded as listing.
    fn listed_on(conn: &Connection, account_id: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM folders WHERE account_id = ?1 AND listed = 1 ORDER BY name")
            .unwrap();
        let rows = stmt
            .query_map([account_id], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    /// How many messages the FTS index matches for an expression.
    fn hits(conn: &Connection, expression: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            [expression],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn the_schema_is_created_once_and_versioned() {
        let (conn, dir) = test_db("schema");
        assert_eq!(
            count(&conn, "SELECT * FROM pragma_user_version"),
            SCHEMA_VERSION
        );
        for table in [
            "accounts",
            "folders",
            "messages",
            "attachments",
            "inline_images",
            "messages_fts",
        ] {
            assert_eq!(
                count(
                    &conn,
                    &format!("SELECT COUNT(*) FROM sqlite_master WHERE name = '{table}'")
                ),
                1,
                "{table} is missing"
            );
        }

        // Opening the same file again runs neither the DDL nor the import.
        let mut again = Connection::open(dir.join("mail.db")).unwrap();
        configure(&again).unwrap();
        migrate(&mut again, None).unwrap();
        assert_eq!(
            count(&again, "SELECT * FROM pragma_user_version"),
            SCHEMA_VERSION
        );
        integrity_check_on(&again).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn accounts_come_back_in_order_and_edits_keep_their_place() {
        let (conn, dir) = test_db("accounts");
        let mut work = account("work");
        work.protocol = Protocol::Pop3;
        work.tls = TlsMode::StartTls;
        work.port = 110;
        save_account_on(&conn, &work).unwrap();
        save_account_on(&conn, &account("personal")).unwrap();

        let loaded = load_accounts_on(&conn).unwrap();
        assert_eq!(
            loaded.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["work", "personal"]
        );
        assert_eq!(loaded[0], work, "every field round-trips");

        // Renaming and re-typing the password is an update, not a new account:
        // the id and the place in the navigation column stay put.
        let mut edited = account("work");
        edited.name = "Work (renamed)".to_string();
        edited.password = "new secret".to_string();
        save_account_on(&conn, &edited).unwrap();

        let loaded = load_accounts_on(&conn).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "work");
        assert_eq!(loaded[0].name, "Work (renamed)");
        assert_eq!(loaded[0].password, "new secret");
        assert_eq!(loaded[1].id, "personal");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_schema_refuses_a_protocol_it_does_not_know() {
        let (conn, dir) = test_db("check");
        let error = conn
            .execute(
                "INSERT INTO accounts (id, name, protocol, host, port, username, password, tls, position)
                 VALUES ('x', 'X', 'smtp', 'mail.example.com', 25, 'u', 'p', 'implicit', 0)",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().contains("CHECK"), "{error}");
        let error = conn
            .execute(
                "INSERT INTO accounts (id, name, protocol, host, port, username, password, tls, position)
                 VALUES ('x', 'X', 'imap', 'mail.example.com', 25, 'u', 'p', 'starttls', 0)",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().contains("CHECK"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_an_account_takes_its_mail_with_it() {
        let (mut conn, dir) = test_db("cascade");
        save_account_on(&conn, &account("work")).unwrap();
        save_account_on(&conn, &account("other")).unwrap();
        let mut mail = message("1", "Hello", "body of the mail");
        mail.attachment_names = vec!["report.pdf".to_string()];
        mail.inline_images = vec![InlineImage {
            cid: "logo".to_string(),
            content_type: "image/png".to_string(),
            data: b"png bytes".to_vec(),
        }];
        save_messages_on(&mut conn, "work", "INBOX", Some(1), &[mail.clone()]).unwrap();
        save_messages_on(&mut conn, "other", "INBOX", Some(1), &[mail]).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 2);

        delete_account_on(&conn, "work").unwrap();

        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM messages WHERE account_id = 'work'"
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM folders WHERE account_id = 'work'"
            ),
            0
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM attachments"),
            1,
            "the other account's rows stay"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM inline_images"), 1);
        assert_eq!(hits(&conn, "hello"), 1, "the deleted mail left the index");
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_message_takes_its_children_and_its_index_row() {
        let (mut conn, dir) = test_db("delete-message");
        save_account_on(&conn, &account("work")).unwrap();
        let mut mail = message("1", "Hello", "body of the mail");
        mail.attachment_names = vec!["report.pdf".to_string()];
        mail.inline_images = vec![InlineImage {
            cid: "logo".to_string(),
            content_type: "image/png".to_string(),
            data: b"png bytes".to_vec(),
        }];
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(1),
            &[mail, message("2", "Other", "other body")],
        )
        .unwrap();
        assert_eq!(
            folder_sync_on(&conn, "work", "INBOX").unwrap().uid_next,
            Some(3),
            "the batch moves the folder's mark"
        );

        delete_message_on(&conn, "work", "INBOX", "1").unwrap();

        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM attachments"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM inline_images"), 0);
        assert_eq!(hits(&conn, "hello"), 0, "the deleted mail left the index");
        assert_eq!(hits(&conn, "other"), 1);
        assert_eq!(
            folder_sync_on(&conn, "work", "INBOX").unwrap().uid_next,
            Some(3),
            "the mark does not walk back to a message that is gone"
        );
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn purging_an_account_keeps_the_account_but_forgets_its_mail() {
        let (mut conn, dir) = test_db("purge");
        save_account_on(&conn, &account("work")).unwrap();
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(1),
            &[message("1", "Hello", "body")],
        )
        .unwrap();

        purge_mail_on(&mut conn, "work").unwrap();

        assert_eq!(
            load_accounts_on(&conn).unwrap().len(),
            1,
            "the account stays"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM folders"), 0);
        assert!(
            load_mailboxes_on(&conn, &["work".to_string()]).unwrap()["work"]
                .folders
                .is_empty()
        );
        assert_eq!(hits(&conn, "hello"), 0);
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_upserts_by_key_and_never_unreads_a_message() {
        let (mut conn, dir) = test_db("upsert");
        save_account_on(&conn, &account("work")).unwrap();
        let mut first = message("1", "Hello", "first body");
        first.attachment_names = vec!["one.pdf".to_string()];
        save_messages_on(&mut conn, "work", "INBOX", Some(4), &[first]).unwrap();

        // The server reports the same UID as read, with different content.
        let mut second = message("1", "Hello again", "second body");
        second.seen = true;
        save_messages_on(&mut conn, "work", "INBOX", Some(4), &[second]).unwrap();

        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1);
        let (subject, body, seen) = conn
            .query_row(
                "SELECT subject, body_text, seen FROM messages WHERE uid = '1'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (subject.as_str(), body.as_str()),
            ("Hello again", "second body")
        );
        assert_eq!(seen, 1, "`seen` only ever grows locally");
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM attachments"),
            0,
            "the list is replaced"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM folders WHERE account_id = 'work' AND name = 'INBOX' AND synced = 1 AND uidvalidity = 4"),
            1
        );

        // A later re-sync that still reports the mail as unseen must not undo it.
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(4),
            &[message("1", "Hello again", "second body")],
        )
        .unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM messages WHERE seen = 0"),
            0
        );
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_uidvalidity_clears_only_that_folder() {
        let (mut conn, dir) = test_db("uidvalidity");
        save_account_on(&conn, &account("work")).unwrap();
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(1),
            &[message("1", "Old", "old body")],
        )
        .unwrap();
        save_messages_on(
            &mut conn,
            "work",
            "Trash",
            Some(1),
            &[message("9", "Kept", "kept body")],
        )
        .unwrap();

        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(2),
            &[message("2", "New", "new body")],
        )
        .unwrap();

        let inbox = load_mailboxes_on(&conn, &["work".to_string()]).unwrap()["work"]
            .folders
            .iter()
            .find(|folder| folder.name == "INBOX")
            .cloned()
            .unwrap();
        assert_eq!(inbox.uidvalidity, Some(2));
        assert_eq!(
            inbox
                .messages
                .iter()
                .map(|m| m.uid.as_str())
                .collect::<Vec<_>>(),
            ["2"]
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM messages WHERE folder = 'Trash'"
            ),
            1
        );
        assert_eq!(
            hits(&conn, "subject : old"),
            0,
            "the rotated mail left the index"
        );
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_imap_mark_follows_what_is_stored() {
        let (mut conn, dir) = test_db("imap-mark");
        save_account_on(&conn, &account("work")).unwrap();
        assert_eq!(
            folder_sync_on(&conn, "work", "INBOX").unwrap(),
            FolderSync::default(),
            "a folder nobody has fetched has no mark"
        );

        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(7),
            &[message("10", "a", "a"), message("12", "b", "b")],
        )
        .unwrap();
        let mark = folder_sync_on(&conn, "work", "INBOX").unwrap();
        assert_eq!(mark.uidvalidity, Some(7));
        assert_eq!(mark.uid_next, Some(13), "one past the highest UID stored");

        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(7),
            &[message("20", "c", "c")],
        )
        .unwrap();
        assert_eq!(
            folder_sync_on(&conn, "work", "INBOX").unwrap().uid_next,
            Some(21)
        );

        // Storing what is already there moves nothing: the mark is what is
        // stored, not what was in the batch.
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(7),
            &[message("20", "c", "c")],
        )
        .unwrap();
        assert_eq!(
            folder_sync_on(&conn, "work", "INBOX").unwrap().uid_next,
            Some(21)
        );

        // Another UIDVALIDITY generation starts the folder over.
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(8),
            &[message("3", "d", "d")],
        )
        .unwrap();
        let mark = folder_sync_on(&conn, "work", "INBOX").unwrap();
        assert_eq!(mark.uidvalidity, Some(8));
        assert_eq!(mark.uid_next, Some(4));
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pop3_keeps_its_own_mark_and_knows_its_uidls() {
        let (mut conn, dir) = test_db("pop3-mark");
        let mut pop = account("pop");
        pop.protocol = Protocol::Pop3;
        save_account_on(&conn, &pop).unwrap();

        save_messages_on(
            &mut conn,
            "pop",
            "INBOX",
            None,
            &[message("UIDL-1", "a", "a"), message("UIDL-2", "b", "b")],
        )
        .unwrap();
        let mark = folder_sync_on(&conn, "pop", "INBOX").unwrap();
        assert_eq!(mark.uid_next, None, "a UIDL is not a UID");
        assert_eq!(mark.first_num, None, "nothing chose a floor yet");

        assert_eq!(
            stored_uids_on(&conn, "pop", "INBOX").unwrap(),
            HashSet::from(["UIDL-1".to_string(), "UIDL-2".to_string()])
        );

        set_first_num_on(&conn, "pop", "INBOX", 4951).unwrap();
        let mark = folder_sync_on(&conn, "pop", "INBOX").unwrap();
        assert_eq!(mark.first_num, Some(4951));
        assert_eq!(mark.uid_next, None, "a POP3 folder never gets a UID mark");
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_fetched_folders_are_listed_for_a_catch_up() {
        let (mut conn, dir) = test_db("synced-folders");
        save_account_on(&conn, &account("work")).unwrap();
        save_folder_list_on(
            &mut conn,
            "work",
            &["INBOX".to_string(), "Sent".to_string()],
        )
        .unwrap();
        assert!(
            synced_folders_on(&conn, "work").unwrap().is_empty(),
            "a folder the LIST knows is not a folder that was fetched"
        );

        save_messages_on(
            &mut conn,
            "work",
            "Sent",
            Some(3),
            &[message("1", "a", "a")],
        )
        .unwrap();
        save_messages_on(&mut conn, "work", "INBOX", Some(3), &[]).unwrap();
        assert_eq!(
            synced_folders_on(&conn, "work").unwrap(),
            ["INBOX", "Sent"],
            "INBOX counts even when the fetch found nothing"
        );
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// What a `LIST` answer decides: which folders count as ones the server
    /// still has. A folder it leaves out is not deleted — the mail cached for
    /// it is the only copy of it here — but it stops being listed, which is
    /// what the navigation column draws it as. A fetch that reaches one of
    /// them again is proof the server has it after all.
    #[test]
    fn the_list_decides_which_folders_the_server_still_has() {
        let (mut conn, dir) = test_db("listed-folders");
        save_account_on(&conn, &account("work")).unwrap();
        let all = [
            "INBOX".to_string(),
            "Sent".to_string(),
            "Archive".to_string(),
        ];
        save_folder_list_on(&mut conn, "work", &all).unwrap();
        assert_eq!(listed_on(&conn, "work"), ["Archive", "INBOX", "Sent"]);

        // The next answer names only INBOX.
        save_folder_list_on(&mut conn, "work", &["INBOX".to_string()]).unwrap();
        assert_eq!(listed_on(&conn, "work"), ["INBOX"]);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM folders WHERE account_id = 'work'"
            ),
            3,
            "the folders it left out keep their mail, and their rows"
        );
        assert_eq!(
            listed_folders_on(&conn, "work").unwrap(),
            ["INBOX"],
            "so what a failed LIST falls back on is what the last one named"
        );

        // Mail fetched for one of them means the server has it.
        save_messages_on(
            &mut conn,
            "work",
            "Archive",
            None,
            &[message("1", "a", "a")],
        )
        .unwrap();
        assert_eq!(listed_on(&conn, "work"), ["Archive", "INBOX"]);

        // The same for POP3, which has no `LIST` to be listed by: its INBOX is
        // listed by the sync that fetches it.
        let mut pop = account("pop");
        pop.protocol = Protocol::Pop3;
        save_account_on(&conn, &pop).unwrap();
        set_first_num_on(&conn, "pop", "INBOX", 1).unwrap();
        assert_eq!(listed_on(&conn, "pop"), ["INBOX"]);

        // And the loaded mailboxes carry the flag the column starts on.
        let mailboxes = load_mailboxes_on(&conn, &["work".to_string()]).unwrap();
        let listed = |name: &str| {
            mailboxes["work"]
                .folders
                .iter()
                .find(|folder| folder.name == name)
                .unwrap()
                .listed
        };
        assert!(listed("INBOX") && listed("Archive"));
        assert!(!listed("Sent"));
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_1_database_gets_its_imap_mark_derived() {
        let (mut conn, dir) = test_db("migrate-v2");
        save_account_on(&conn, &account("work")).unwrap();
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(4),
            &[message("10", "a", "a"), message("12", "b", "b")],
        )
        .unwrap();
        let mut pop = account("pop");
        pop.protocol = Protocol::Pop3;
        save_account_on(&conn, &pop).unwrap();
        save_messages_on(&mut conn, "pop", "INBOX", None, &[message("1", "a", "a")]).unwrap();

        // Rewind to the shape version 1 shipped: no marks, no listedness, and
        // the version that says so.
        conn.execute_batch(
            "ALTER TABLE folders DROP COLUMN uid_next;
             ALTER TABLE folders DROP COLUMN first_num;
             ALTER TABLE folders DROP COLUMN listed;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        migrate(&mut conn, None).unwrap();
        assert_eq!(
            count(&conn, "SELECT * FROM pragma_user_version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            folder_sync_on(&conn, "work", "INBOX").unwrap().uid_next,
            Some(13),
            "the upgrade does not refetch a batch it already holds"
        );
        assert_eq!(
            folder_sync_on(&conn, "pop", "INBOX").unwrap().uid_next,
            None,
            "a POP3 UIDL is not a UID, however numeric it looks"
        );
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_left_at_version_0_is_finished() {
        // A crash between the DDL and the version write, or an import that
        // failed, leaves a database that has the schema but says 0. The next
        // start has to be able to walk that path again — every statement is
        // `IF NOT EXISTS` — without losing what is already in it.
        let (mut conn, dir) = test_db("half-built");
        conn.pragma_update(None, "user_version", 0).unwrap();
        save_account_on(&conn, &account("work")).unwrap();

        migrate(&mut conn, None).unwrap();
        assert_eq!(
            count(&conn, "SELECT * FROM pragma_user_version"),
            SCHEMA_VERSION
        );
        assert_eq!(load_accounts_on(&conn).unwrap().len(), 1);
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn attachments_and_inline_images_survive_as_bytes() {
        let (mut conn, dir) = test_db("children");
        save_account_on(&conn, &account("work")).unwrap();
        let mut mail = message("1", "With parts", "body");
        mail.attachment_names = vec!["report.pdf".to_string(), "photo.png".to_string()];
        mail.inline_images = vec![
            InlineImage {
                cid: "logo@example.com".to_string(),
                content_type: "image/png".to_string(),
                data: vec![0x89, b'P', b'N', b'G', 0x00, 0xff],
            },
            InlineImage {
                cid: "second".to_string(),
                content_type: "image/gif".to_string(),
                data: b"GIF89a".to_vec(),
            },
        ];
        save_messages_on(&mut conn, "work", "INBOX", Some(1), &[mail]).unwrap();

        let mailboxes = load_mailboxes_on(&conn, &["work".to_string()]).unwrap();
        let loaded = &mailboxes["work"].folders[0].messages[0];
        assert_eq!(loaded.attachment_names, ["report.pdf", "photo.png"]);
        assert_eq!(loaded.inline_images.len(), 2);
        assert_eq!(loaded.inline_images[0].cid, "logo@example.com");
        assert_eq!(loaded.inline_images[0].content_type, "image/png");
        assert_eq!(
            loaded.inline_images[0].data,
            [0x89, b'P', b'N', b'G', 0x00, 0xff]
        );
        assert_eq!(loaded.inline_images[1].data, b"GIF89a");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_saving_the_same_batch_keeps_the_row_ids() {
        let (mut conn, dir) = test_db("rowid");
        save_account_on(&conn, &account("work")).unwrap();
        let mail = message("1", "Hello", "body");
        save_messages_on(
            &mut conn,
            "work",
            "INBOX",
            Some(1),
            std::slice::from_ref(&mail),
        )
        .unwrap();
        let first = count(&conn, "SELECT id FROM messages WHERE uid = '1'");
        save_messages_on(&mut conn, "work", "INBOX", Some(1), &[mail]).unwrap();
        let second = count(&conn, "SELECT id FROM messages WHERE uid = '1'");
        assert_eq!(first, second, "an unchanged re-sync rewrites nothing");
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_search_index_follows_the_body_and_the_cjk_column() {
        let (mut conn, dir) = test_db("fts");
        save_account_on(&conn, &account("work")).unwrap();
        let mut chinese = message("1", "发票号码确认", "本月的发票已经开好，请确认号码");
        chinese.from = "财务 <caiwu@nroad.com.cn>".to_string();
        let english = message("2", "Invoice for August", "Your invoices are ready");
        save_messages_on(&mut conn, "work", "INBOX", Some(1), &[chinese, english]).unwrap();

        // Latin: a subject word, a body word, a sender, and the prefix form the
        // query builder will use.
        assert_eq!(
            hits(&conn, "{subject from_addr to_addr body_text} : \"invoi\" *"),
            1
        );
        assert_eq!(
            hits(
                &conn,
                "{subject from_addr to_addr body_text} : \"invoices\" *"
            ),
            1
        );
        assert_eq!(hits(&conn, "from_addr : caiwu"), 1);
        // CJK: the bigrams make a two-character search exact...
        assert_eq!(hits(&conn, "{cjk} : \"发票\""), 1);
        assert_eq!(hits(&conn, "{cjk} : \"票号\" AND {cjk} : \"号码\""), 1);
        // ...while a pair that never appears adjacently does not match.
        assert_eq!(hits(&conn, "{cjk} : \"票码\""), 0);
        // A single character is deliberately broad: it matches its unigram.
        assert_eq!(hits(&conn, "{cjk} : \"发\""), 1);
        integrity_check_on(&conn).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_legacy_json_is_imported_once_and_deleted() {
        let dir = std::env::temp_dir().join(format!("kimo-db-{}-legacy", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = dir.join("config");
        let cache = dir.join("cache");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&cache).unwrap();
        fs::write(
            config.join("accounts.json"),
            r#"[{"id":"work","name":"Work","protocol":"pop3","host":"pop.example.com","port":995,"username":"u","password":"p","tls":"implicit"}]"#,
        )
        .unwrap();
        fs::write(
            cache.join("work.json"),
            r#"{"folders":[{"name":"INBOX","uidvalidity":7,"synced":true,"messages":[{"uid":"1","subject":"发票号码","from":"财务 <caiwu@nroad.com.cn>","to":"me@example.com","date":"","timestamp":10,"seen":false,"body_text":"please confirm 发票号码","body_html":null,"preview":"please","attachment_names":["a.pdf"],"inline_images":[{"cid":"logo","content_type":"image/png","data_base64":"aGVsbG8="}]}]}]}"#,
        )
        .unwrap();
        fs::write(cache.join("ghost.json"), r#"{"folders":[]}"#).unwrap();

        let mut conn = Connection::open(dir.join("mail.db")).unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn, Some((config.as_path(), cache.as_path()))).unwrap();

        assert_eq!(
            count(&conn, "SELECT * FROM pragma_user_version"),
            SCHEMA_VERSION
        );
        let accounts = load_accounts_on(&conn).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, "work");
        assert_eq!(accounts[0].protocol, Protocol::Pop3);
        assert!(
            !config.join("accounts.json").exists(),
            "the account list moved into the database"
        );
        assert!(
            !cache.join("work.json").exists(),
            "the imported cache is deleted"
        );
        assert!(
            cache.join("ghost.json").exists(),
            "a cache for an unknown account is left alone"
        );

        let mailboxes = load_mailboxes_on(&conn, &["work".to_string()]).unwrap();
        let folder = &mailboxes["work"].folders[0];
        assert_eq!(folder.name, "INBOX");
        assert_eq!(folder.uidvalidity, Some(7));
        assert!(folder.synced);
        let migrated = &folder.messages[0];
        assert_eq!(migrated.subject, "发票号码");
        assert_eq!(migrated.attachment_names, ["a.pdf"]);
        assert_eq!(migrated.inline_images[0].data, b"hello");
        assert_eq!(
            hits(&conn, "{cjk} : \"发票\""),
            1,
            "the index was built during the import"
        );
        integrity_check_on(&conn).unwrap();

        // Opening again imports nothing twice.
        migrate(&mut conn, Some((config.as_path(), cache.as_path()))).unwrap();
        assert_eq!(load_accounts_on(&conn).unwrap().len(), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_legacy_cache_is_left_alone() {
        let (mut conn, dir) = test_db("damaged");
        let config = dir.join("config");
        let cache = dir.join("cache");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&cache).unwrap();
        save_account_on(&conn, &account("work")).unwrap();
        fs::write(cache.join("work.json"), "{ not json at all").unwrap();

        import_legacy_json(&mut conn, config.as_path(), cache.as_path()).unwrap();

        assert!(
            cache.join("work.json").exists(),
            "a damaged file is the user's to keep"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The upgrade check for a real installation: import a pre-database layout
    /// (an `accounts.json` and a `cache/` of `<account-id>.json` files) into a
    /// throwaway database and report what landed. Run it on a *copy* — the
    /// import deletes the files it reads, as it does for a real upgrade:
    ///
    ///     KIMO_LEGACY_DIR=/tmp/old-kimo \
    ///         cargo test -- --ignored --nocapture import_a_real_legacy_layout
    ///
    /// Drop this with the rest of the legacy import code.
    #[test]
    #[ignore]
    fn import_a_real_legacy_layout() {
        let Some(root) = std::env::var_os("KIMO_LEGACY_DIR") else {
            println!("set KIMO_LEGACY_DIR to the old config/data directory");
            return;
        };
        let root = PathBuf::from(root);
        let dir = std::env::temp_dir().join(format!("kimo-db-{}-imported", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut conn = Connection::open(dir.join("mail.db")).unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn, Some((root.as_path(), &root.join("cache")))).unwrap();

        let accounts = load_accounts_on(&conn).unwrap();
        println!("{} account(s)", accounts.len());
        for account in &accounts {
            println!(
                "  {}  {}  {} {:?}://{}:{}  tls={:?}",
                account.id,
                account.name,
                account.protocol.label(),
                account.protocol,
                account.host,
                account.port,
                account.tls
            );
        }
        let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
        let mailboxes = load_mailboxes_on(&conn, &ids).unwrap();
        for (id, mailbox) in &mailboxes {
            let messages: usize = mailbox.folders.iter().map(|f| f.messages.len()).sum();
            let unread: usize = mailbox
                .folders
                .iter()
                .flat_map(|f| f.messages.iter())
                .filter(|m| !m.seen)
                .count();
            let inline: usize = mailbox
                .folders
                .iter()
                .flat_map(|f| f.messages.iter())
                .map(|m| m.inline_images.len())
                .sum();
            println!(
                "  {id}: {} folders, {messages} messages ({unread} unread, {inline} inline images)",
                mailbox.folders.len()
            );
            // What each folder will resume from: the upgrade derives an IMAP
            // mark from the imported UIDs, so the first sync after it fetches
            // only what arrived since the cache was written.
            for folder in &mailbox.folders {
                let mark = folder_sync_on(&conn, id, &folder.name).unwrap();
                println!(
                    "    {:<34} {:>4} msgs  uidvalidity={:?} uid_next={:?} first_num={:?}",
                    folder.name,
                    folder.messages.len(),
                    mark.uidvalidity,
                    mark.uid_next,
                    mark.first_num
                );
            }
        }
        println!(
            "database: {} ({} bytes)",
            dir.join("mail.db").display(),
            fs::metadata(dir.join("mail.db")).unwrap().len()
        );
        integrity_check_on(&conn).unwrap();
        println!("integrity check: ok");
    }
}
