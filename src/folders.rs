//! Folder kinds: which special mailbox a name refers to, and the icon to draw.
//!
//! IMAP servers may label special mailboxes with the SPECIAL-USE attributes
//! (\Sent, \Drafts, \Trash, \Junk, \Archive, \All, \Flagged), but a plain
//! LIST often carries none of them and this client never asks for the
//! capability that would let it request them, so the kind is derived from the
//! mailbox name. That is the *decoded* name — the string the folder list
//! shows, not the modified UTF-7 wire form.
//!
//! The keyword table is data, not UI copy. Mailbox names arrive in whatever
//! language the server's users speak, so the names the big providers use
//! (QQ Mail, 163, Outlook-CN, Gmail-CN) are matched exactly like `Sent
//! Items` is. That is not an i18n framework: the UI strings stay English
//! (hard rule 6).

/// What a folder is, for icon purposes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FolderKind {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    AllMail,
    Starred,
    /// Anything [`kind_of`] does not recognize.
    Folder,
}

impl FolderKind {
    /// The embedded row icons, as (kind, open, PNG). Only a generic folder
    /// has a second, open glyph; the special mailboxes keep one.
    ///
    /// The PNGs are white on transparency: egui tints by multiplying, so a
    /// black bake could never take the row's text color.
    pub const ICONS: &[(FolderKind, bool, &[u8])] = &[
        (
            FolderKind::Inbox,
            false,
            include_bytes!("../assets/folder-icons/inbox.png"),
        ),
        (
            FolderKind::Sent,
            false,
            include_bytes!("../assets/folder-icons/sent.png"),
        ),
        (
            FolderKind::Drafts,
            false,
            include_bytes!("../assets/folder-icons/drafts.png"),
        ),
        (
            FolderKind::Trash,
            false,
            include_bytes!("../assets/folder-icons/trash.png"),
        ),
        (
            FolderKind::Junk,
            false,
            include_bytes!("../assets/folder-icons/junk.png"),
        ),
        (
            FolderKind::Archive,
            false,
            include_bytes!("../assets/folder-icons/archive.png"),
        ),
        (
            FolderKind::AllMail,
            false,
            include_bytes!("../assets/folder-icons/all-mail.png"),
        ),
        (
            FolderKind::Starred,
            false,
            include_bytes!("../assets/folder-icons/starred.png"),
        ),
        (
            FolderKind::Folder,
            false,
            include_bytes!("../assets/folder-icons/folder.png"),
        ),
        (
            FolderKind::Folder,
            true,
            include_bytes!("../assets/folder-icons/folder-open.png"),
        ),
    ];
}

/// Gmail's LIST names its system mailboxes under a container called
/// `[Gmail]`. The container itself is `\Noselect` — it is a namespace, not
/// a mailbox, and SELECT on it is refused — so it is never a row.
pub const GMAIL_CONTAINER: &str = "[Gmail]";

/// Whether `name` is Gmail's `[Gmail]` namespace container rather than a
/// mailbox.
pub fn is_gmail_container(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(GMAIL_CONTAINER)
}

/// The display name of a Gmail system mailbox: `[Gmail]/All Mail` is
/// `All Mail`. `None` for anything not nested under the container, so a
/// user label called `Projects/2026` keeps its own name.
pub fn gmail_folder(name: &str) -> Option<&str> {
    let name = name.trim();
    let head = name.get(..GMAIL_CONTAINER.len())?;
    if !head.eq_ignore_ascii_case(GMAIL_CONTAINER) {
        return None;
    }
    let rest = name.get(GMAIL_CONTAINER.len()..)?.strip_prefix('/')?;
    (!rest.is_empty()).then_some(rest)
}

/// Whether a mailbox of a Gmail account is a label rather than a folder.
/// Gmail lists its labels beside INBOX and nests only its system mailboxes
/// under `[Gmail]`, so the name is enough *when the account's list is
/// Gmail's*. A plain IMAP server's folders are not labels, and callers only
/// ask this after the list showed a `[Gmail]/` mailbox.
pub fn is_gmail_label(name: &str) -> bool {
    let name = name.trim();
    !name.eq_ignore_ascii_case("INBOX") && !is_gmail_container(name) && gmail_folder(name).is_none()
}

/// The navigation column's order for a Gmail account: INBOX, then the system
/// mailboxes Gmail nests under `[Gmail]`, then the labels. Each group is in
/// name order, the order `imap::order_mailboxes` gives a mailbox list, so a
/// sync does not shuffle the column. Gmail's labels are the account's own
/// list; nothing here invents one.
pub fn order_gmail(folders: &mut Vec<String>) {
    let mut inbox = Vec::new();
    let mut rest = Vec::new();
    for name in folders.drain(..) {
        if name.trim().eq_ignore_ascii_case("INBOX") {
            inbox.push(name);
        } else {
            rest.push(name);
        }
    }
    let (mut system, mut labels): (Vec<String>, Vec<String>) = rest
        .into_iter()
        .partition(|name| gmail_folder(name).is_some());
    system.sort();
    labels.sort();
    folders.extend(inbox);
    folders.extend(system);
    folders.extend(labels);
}

/// Classify a folder by its decoded, display-ready name.
pub fn kind_of(name: &str) -> FolderKind {
    let name = name.trim();
    // INBOX is the one mailbox name the protocol fixes, in any case.
    if name.eq_ignore_ascii_case("INBOX") {
        return FolderKind::Inbox;
    }
    // Names nest ([Gmail]/已发送邮件, 项目/已发送/2026) and the hierarchy
    // delimiter is the server's choice, so walk the segments from the right:
    // the closest recognizable ancestor names the folder.
    for segment in name.rsplit(['/', '.', '\\']) {
        if let Some(kind) = classify(segment.trim()) {
            return kind;
        }
    }
    FolderKind::Folder
}

/// One path segment: an exact keyword, or a keyword plus one of the standard
/// endings — 已删除邮件 is 已删除 + 邮件, Sent Items is Sent + Items.
fn classify(segment: &str) -> Option<FolderKind> {
    if segment.is_empty() {
        return None;
    }
    if let Some((_, kind)) = KEYWORDS.iter().find(|(word, _)| eq_ci(segment, word)) {
        return Some(*kind);
    }
    KEYWORDS.iter().find_map(|(word, kind)| {
        let rest = strip_prefix_ci(segment, word)?.trim();
        SUFFIXES
            .iter()
            .any(|suffix| eq_ci(rest, suffix))
            .then_some(*kind)
    })
}

/// Case-insensitive for ASCII, byte-exact for everything else (CJK has no
/// case, and mixed names compare as written).
fn eq_ci(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

/// Like str::strip_prefix, ASCII-case-insensitively, and safe on non-ASCII
/// boundaries (a keyword that would cut a character short never matches).
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Canonical names of the special mailboxes. The ASCII ones cover what
/// Gmail, Dovecot, Exchange and Courier hand out; the Chinese ones are what
/// QQ Mail, 163 and Outlook-CN call the same folders.
const KEYWORDS: &[(&str, FolderKind)] = &[
    ("INBOX", FolderKind::Inbox),
    ("收件箱", FolderKind::Inbox),
    ("收件匣", FolderKind::Inbox),
    ("收信箱", FolderKind::Inbox),
    ("Draft", FolderKind::Drafts),
    ("Drafts", FolderKind::Drafts),
    ("草稿", FolderKind::Drafts),
    ("草稿箱", FolderKind::Drafts),
    ("Sent", FolderKind::Sent),
    ("已发送", FolderKind::Sent),
    ("发件箱", FolderKind::Sent),
    ("寄件箱", FolderKind::Sent),
    ("Trash", FolderKind::Trash),
    ("Bin", FolderKind::Trash),
    ("Deleted", FolderKind::Trash),
    ("已删除", FolderKind::Trash),
    ("废纸篓", FolderKind::Trash),
    ("回收站", FolderKind::Trash),
    ("Junk", FolderKind::Junk),
    ("Spam", FolderKind::Junk),
    ("Bulk Mail", FolderKind::Junk),
    ("垃圾邮件", FolderKind::Junk),
    ("垃圾箱", FolderKind::Junk),
    ("垃圾筒", FolderKind::Junk),
    ("广告邮件", FolderKind::Junk),
    ("不明邮件", FolderKind::Junk),
    ("Archive", FolderKind::Archive),
    ("Archives", FolderKind::Archive),
    ("归档", FolderKind::Archive),
    ("存档", FolderKind::Archive),
    ("All Mail", FolderKind::AllMail),
    ("所有邮件", FolderKind::AllMail),
    ("全部邮件", FolderKind::AllMail),
    ("Starred", FolderKind::Starred),
    ("Flagged", FolderKind::Starred),
    ("星标", FolderKind::Starred),
];

/// Endings a keyword may carry: Sent Items, Deleted Messages, 已删除邮件,
/// 已发送的邮件, 垃圾邮件箱. Only these, so that a user folder which merely
/// starts with a keyword (已发送备份, Sentinel) stays an ordinary folder.
const SUFFIXES: &[&str] = &[
    "邮件",
    "邮件箱",
    "邮件夹",
    "的邮件",
    "箱",
    "夹",
    "文件夹",
    "Message",
    "Messages",
    "Item",
    "Items",
    "Mail",
    "Mails",
    "E-mail",
    "Email",
    "Objects",
    "Folder",
    "Archive",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_names_map_to_their_kinds() {
        assert_eq!(kind_of("草稿箱"), FolderKind::Drafts);
        assert_eq!(kind_of("已发送"), FolderKind::Sent);
        assert_eq!(kind_of("已删除"), FolderKind::Trash);
        assert_eq!(kind_of("垃圾邮件"), FolderKind::Junk);
    }

    #[test]
    fn names_the_big_providers_use_are_recognized() {
        // The development account's own folders, decoded from modified UTF-7.
        assert_eq!(kind_of("已删除邮件"), FolderKind::Trash);
        assert_eq!(kind_of("已发送邮件"), FolderKind::Sent);
        assert_eq!(kind_of("垃圾邮件箱"), FolderKind::Junk);
        assert_eq!(kind_of("收件箱"), FolderKind::Inbox);
        assert_eq!(kind_of("所有邮件"), FolderKind::AllMail);
        assert_eq!(kind_of("草稿邮件"), FolderKind::Drafts);
        assert_eq!(kind_of("Sent Items"), FolderKind::Sent);
        assert_eq!(kind_of("Deleted Messages"), FolderKind::Trash);
        assert_eq!(kind_of("Junk E-mail"), FolderKind::Junk);
        assert_eq!(kind_of("Bulk Mail"), FolderKind::Junk);
        assert_eq!(kind_of("Archives"), FolderKind::Archive);
    }

    #[test]
    fn nested_names_use_their_closest_recognizable_segment() {
        assert_eq!(kind_of("[Gmail]/已发送邮件"), FolderKind::Sent);
        assert_eq!(kind_of("[Gmail]/All Mail"), FolderKind::AllMail);
        assert_eq!(kind_of("项目/已发送/2026"), FolderKind::Sent);
        assert_eq!(kind_of("收件箱/客户"), FolderKind::Inbox);
    }

    #[test]
    fn inbox_is_case_insensitive() {
        assert_eq!(kind_of("inbox"), FolderKind::Inbox);
        assert_eq!(kind_of("  Inbox  "), FolderKind::Inbox);
    }

    #[test]
    fn lookalike_names_stay_generic_folders() {
        assert_eq!(kind_of("已发送的报表"), FolderKind::Folder);
        assert_eq!(kind_of("2024/已发送备份"), FolderKind::Folder);
        assert_eq!(kind_of("垃圾邮件备份"), FolderKind::Folder);
        assert_eq!(kind_of("存档备份"), FolderKind::Folder);
        assert_eq!(kind_of("Sentinel"), FolderKind::Folder);
        assert_eq!(kind_of("Drafts2024"), FolderKind::Folder);
        assert_eq!(kind_of(""), FolderKind::Folder);
        // The wire form is not classified: callers decode first.
        assert_eq!(kind_of("&XfJT0ZAB-"), FolderKind::Folder);
    }

    #[test]
    fn gmail_system_folders_lose_their_container_prefix() {
        assert_eq!(gmail_folder("[Gmail]/All Mail"), Some("All Mail"));
        assert_eq!(gmail_folder("[Gmail]/Sent Mail"), Some("Sent Mail"));
        assert_eq!(gmail_folder("[Gmail]/已发送邮件"), Some("已发送邮件"));
        // The container itself, a label, and a name that merely starts the
        // same way are not system folders.
        assert_eq!(gmail_folder("[Gmail]"), None);
        assert_eq!(gmail_folder("[Gmail]x"), None);
        assert_eq!(gmail_folder("Projects/2026"), None);
        assert_eq!(gmail_folder("[Gmail]/"), None);
        // Trimmed like kind_of, and the container is case-insensitive.
        assert_eq!(gmail_folder("  [gmail]/Trash  "), Some("Trash"));
    }

    #[test]
    fn the_container_is_not_a_mailbox_and_labels_are_the_rest() {
        assert!(is_gmail_container("[Gmail]"));
        assert!(is_gmail_container(" [gmail] "));
        assert!(!is_gmail_container("[Gmail]/All Mail"));
        // On a Gmail account, INBOX and the system mailboxes are folders;
        // everything else the list names is a label.
        assert!(!is_gmail_label("INBOX"));
        assert!(!is_gmail_label("inbox"));
        assert!(!is_gmail_label("[Gmail]"));
        assert!(!is_gmail_label("[Gmail]/All Mail"));
        assert!(is_gmail_label("Receipts"));
        assert!(is_gmail_label("Projects/2026"));
        assert!(is_gmail_label("Sent")); // a label may be named like a folder
    }

    #[test]
    fn gmail_order_is_inbox_then_system_folders_then_labels() {
        let mut folders = vec![
            "Receipts".to_string(),
            "[Gmail]/Trash".to_string(),
            "Archive".to_string(),
            "INBOX".to_string(),
            "[Gmail]/All Mail".to_string(),
            "Projects/2026".to_string(),
            "[Gmail]/Drafts".to_string(),
        ];
        order_gmail(&mut folders);
        assert_eq!(
            folders,
            [
                "INBOX",
                "[Gmail]/All Mail",
                "[Gmail]/Drafts",
                "[Gmail]/Trash",
                "Archive",
                "Projects/2026",
                "Receipts",
            ]
        );
    }

    #[test]
    fn every_kind_has_a_white_icon() {
        for (kind, open, png) in FolderKind::ICONS {
            let decoded = image::load_from_memory(png)
                .unwrap_or_else(|error| panic!("{kind:?} open={open}: {error}"))
                .to_rgba8();
            assert_eq!(decoded.dimensions(), (48, 48), "{kind:?} open={open}");
            // White (egui tints by multiplying) with transparency around it.
            assert!(
                decoded
                    .pixels()
                    .any(|pixel| pixel.0 == [255, 255, 255, 255]),
                "{kind:?} open={open} has no white pixels"
            );
            assert!(
                decoded.pixels().any(|pixel| pixel.0[3] == 0),
                "{kind:?} open={open} has no transparent pixels"
            );
        }
    }
}
