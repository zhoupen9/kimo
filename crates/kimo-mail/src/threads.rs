//! Mail grouped into conversations: the message list's model of a folder.
//!
//! A thread is what the headers say it is — `References` read from the end, the
//! last id this folder holds is the parent, and `In-Reply-To` for a client that
//! wrote no chain — and, when they say nothing at all, what the subject says: a
//! subject that carries a reply or forward prefix joins the thread of the same
//! subject without one. That second rule is the only reason mail cached before
//! schema v7 threads (its ids were never stored, and a warm sync never
//! refetches a mail the database has), and it is deliberately narrower than the
//! classic subject merge: two mails that happen to share a subject stay apart
//! unless one of them looks like an answer.
//!
//! Nothing here touches the database or the UI: `group` is a pure function of a
//! folder's messages, which is what makes the rules testable and what lets the
//! list cache its answer instead of grouping on every frame.

use std::collections::HashMap;

use kimo_core::models::MailMessage;

/// One conversation: its mails, oldest first, and what a list row says about
/// them.
#[derive(Clone, Debug)]
pub struct Thread {
    /// Indices into the folder's message list, oldest first — the order a
    /// conversation reads in, which is the reverse of the folder's own.
    pub messages: Vec<usize>,
    /// The oldest mail's uid: unique in a folder and stable as long as the
    /// thread's oldest mail is. What the open/closed set is keyed on.
    pub key: String,
    /// The thread's subject: the oldest mail's, with the reply and forward
    /// prefixes taken off. What the row shows.
    pub subject: String,
    /// The mail the row speaks for: the newest one, which is also the one the
    /// list puts the thread's date and preview from.
    pub newest: usize,
    /// How many of its mails are unread.
    pub unread: usize,
    /// Whether any of them carries an attachment.
    pub has_attachments: bool,
    /// Whether any of them is flagged: a conversation's row stands for its
    /// mails, so it carries their warning tint the way it carries their
    /// paperclip.
    pub flagged: bool,
}

impl Thread {
    /// Whether the mail with this uid is one of the thread's. What the list
    /// asks to select a conversation's row: the pane shows a mail, and the row
    /// stands for it when that mail is one of its own.
    pub fn holds(&self, messages: &[MailMessage], uid: &str) -> bool {
        self.messages
            .iter()
            .any(|&index| messages[index].uid == uid)
    }
}

/// A folder's mail as conversations, newest thread first — which is the order
/// the folder itself is in, so threading a list never reorders it.
pub fn group(messages: &[MailMessage]) -> Vec<Thread> {
    let mut sets = Sets::new(messages.len());

    // One index per message id. A duplicate id (the same mail filed twice, or a
    // resend that kept its id) names the first of them, which is enough: they
    // are one conversation either way.
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if !message.message_id.is_empty() {
            by_id.entry(message.message_id.as_str()).or_insert(index);
        }
    }

    for (index, message) in messages.iter().enumerate() {
        if let Some(parent) = parent_of(message, &by_id, index) {
            sets.union(index, parent);
        }
    }

    merge_by_subject(messages, &mut sets);

    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for index in 0..messages.len() {
        let root = sets.find(index);
        members.entry(root).or_default().push(index);
    }

    let mut threads: Vec<Thread> = members
        .into_values()
        .map(|mut indices| {
            // The folder is newest first, so ascending indices are: the first
            // is the mail the row speaks for and the last is the oldest.
            indices.sort_unstable();
            let newest = indices[0];
            let oldest = indices[indices.len() - 1];
            let thread = Thread {
                key: messages[oldest].uid.clone(),
                subject: base_subject(&messages[oldest].subject).to_string(),
                newest,
                unread: indices
                    .iter()
                    .filter(|&&index| !messages[index].seen)
                    .count(),
                has_attachments: indices
                    .iter()
                    .any(|&index| !messages[index].attachments.is_empty()),
                flagged: indices.iter().any(|&index| messages[index].flagged),
                messages: Vec::new(),
            };
            indices.reverse(); // oldest first: the order a conversation reads in
            Thread {
                messages: indices,
                ..thread
            }
        })
        .collect();
    threads.sort_by_key(|thread| thread.newest);
    threads
}

/// The mail this one answers, as far as this folder can tell: the last id in
/// `References` that names a mail here, or — when the client wrote no chain —
/// the last `In-Reply-To` that does. A mail answering something this folder
/// does not have is a root: its parent is somewhere else, and the subject merge
/// (`merge_by_subject`) is what may still put it with the conversation it
/// answers.
fn parent_of(message: &MailMessage, by_id: &HashMap<&str, usize>, index: usize) -> Option<usize> {
    let here = |id: &String| {
        by_id
            .get(id.as_str())
            .copied()
            .filter(|&found| found != index)
    };
    message
        .references
        .iter()
        .rev()
        .find_map(here)
        .or_else(|| message.in_reply_to.iter().rev().find_map(here))
}

/// Join the threads the headers left apart but whose subjects say they belong
/// together. A root joins another root when their normalized subjects are equal
/// and *one of them looks like an answer*: that is what keeps two mails that
/// merely share a subject in threads of their own, while still joining a reply
/// whose headers were lost — every mail cached before schema v7.
fn merge_by_subject(messages: &[MailMessage], sets: &mut Sets) {
    // subject → (root of the thread it belongs to, whether that thread holds a
    // mail whose subject is an answer already).
    let mut subjects: HashMap<String, (usize, bool)> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if sets.find(index) != index {
            continue;
        }
        let subject = normalized_subject(&message.subject);
        if subject.is_empty() {
            continue;
        }
        let answer = strip_subject_prefix(message.subject.trim()).is_some();
        match subjects.get(&subject).copied() {
            Some((root, root_answer)) => {
                if answer || root_answer {
                    sets.union(index, root);
                    subjects.insert(subject, (root, root_answer || answer));
                }
            }
            None => {
                subjects.insert(subject, (index, answer));
            }
        }
    }
}

/// The subject a thread is shown under, and the one it is matched by: the reply
/// and forward prefixes off the front, whitespace collapsed, case folded.
/// `Re: Fwd: Hello` is `hello`.
fn normalized_subject(subject: &str) -> String {
    base_subject(subject)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A subject without the reply and forward prefixes in front of it, in the
/// sender's own case and spacing.
fn base_subject(subject: &str) -> &str {
    let mut text = subject.trim();
    while let Some(rest) = strip_subject_prefix(text) {
        text = rest.trim_start();
    }
    text
}

/// The prefixes clients put in front of a subject that answers another mail.
/// Data, not UI copy: subjects arrive in the sender's language, and the CJK
/// ones are here for the same reason the folder kinds' are (rule 6).
const SUBJECT_PREFIXES: [&str; 11] = [
    "re", "fwd", "fw", "aw", "sv", "答复", "回复", "回覆", "转发", "轉寄", "回信",
];

/// One prefix off the front of `text`, if one is there: the word, an optional
/// counter (`re[2]:`, `re(2):`), and the colon. A colon further in than a
/// prefix can reach (`Note: the plan`) is not a prefix at all.
pub fn strip_subject_prefix(text: &str) -> Option<&str> {
    // The full-width colon too: a CJK client writes 答复： as often as 答复: —
    // and it is three bytes, so where the rest of the subject starts is the
    // colon plus its own width, not plus one.
    let (colon, separator) = text.char_indices().find(|(_, c)| matches!(c, ':' | '：'))?;
    if colon > 8 {
        return None;
    }
    let word = text[..colon].trim();
    let word = match word.find(['[', '(']) {
        Some(open) if word.ends_with(']') || word.ends_with(')') => word[..open].trim(),
        _ => word,
    };
    SUBJECT_PREFIXES
        .iter()
        .any(|prefix| word.eq_ignore_ascii_case(prefix))
        .then(|| &text[colon + separator.len_utf8()..])
}

/// Union-find over message indices: a thread is a set and a header link is a
/// union. Path compression and union by size — a folder is grouped on every
/// change, and the tree has to stay flat enough that it costs nothing.
struct Sets {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Sets {
    fn new(count: usize) -> Self {
        Self {
            parent: (0..count).collect(),
            size: vec![1; count],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let mut root = index;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut walk = index;
        while self.parent[walk] != root {
            let next = self.parent[walk];
            self.parent[walk] = root;
            walk = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (mut a, mut b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        if self.size[a] < self.size[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        self.size[a] += self.size[b];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimo_core::models::Attachment;

    /// One mail of a folder, newest first: the headers a conversation is
    /// stitched from are what the test is about, so they are arguments and
    /// everything else is ordinary mail.
    fn mail(uid: &str, subject: &str, id: &str, references: &[&str]) -> MailMessage {
        MailMessage {
            uid: uid.to_string(),
            subject: subject.to_string(),
            from: "Ada <ada@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "Mon, 1 Jan 2026 00:00:00 +0000".to_string(),
            timestamp: Some(1_767_225_600),
            seen: false,
            flagged: false,
            message_id: id.to_string(),
            in_reply_to: references
                .iter()
                .rev()
                .take(1)
                .map(|id| id.to_string())
                .collect(),
            references: references.iter().map(|id| id.to_string()).collect(),
            body_text: "body".to_string(),
            body_html: None,
            preview: "body".to_string(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
            labels: Default::default(),
        }
    }

    /// A reply and the mail it answers are one thread, in reading order: the
    /// oldest first, and the row speaks for the newest.
    #[test]
    fn a_reply_joins_the_mail_it_answers() {
        let messages = vec![
            mail(
                "3",
                "Re: Hello",
                "c@example.com",
                &["a@example.com", "b@example.com"],
            ),
            mail("2", "Re: Hello", "b@example.com", &["a@example.com"]),
            mail("1", "Hello", "a@example.com", &[]),
        ];
        let threads = group(&messages);
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0].messages,
            [2, 1, 0],
            "oldest first, which is the order the folder holds reversed"
        );
        assert_eq!(threads[0].newest, 0, "the row speaks for the newest mail");
        assert_eq!(threads[0].subject, "Hello", "with the prefixes off");
        assert_eq!(
            threads[0].key, "1",
            "the thread is keyed on its oldest mail"
        );
        assert!(threads[0].holds(&messages, "2") && !threads[0].holds(&messages, "9"));
    }

    /// The parent is the last id in the chain this folder actually has — not
    /// the first, and not a guess by subject: a mail answering something filed
    /// elsewhere is a thread of its own.
    #[test]
    fn the_parent_is_the_nearest_id_this_folder_has() {
        let messages = vec![
            // Answers b, which is a thread of its own, through a chain that
            // also names a.
            mail(
                "3",
                "Re: Something",
                "c@example.com",
                &["a@example.com", "b@example.com"],
            ),
            mail("2", "Another matter", "b@example.com", &[]),
            mail("1", "Hello", "a@example.com", &[]),
        ];
        let threads = group(&messages);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].messages, [1, 0], "the reply went to its parent");

        // The parent is not here at all, and the subject is what is left: the
        // mail joins the thread whose subject it answers, the way a mail with
        // no headers at all does.
        let orphan = vec![
            mail("2", "Re: Hello", "b@example.com", &["ghost@example.com"]),
            mail("1", "Hello", "a@example.com", &[]),
        ];
        let threads = group(&orphan);
        assert_eq!(threads.len(), 1, "the subject is all the mail has left");
        assert_eq!(threads[0].messages, [1, 0]);

        // Nothing links it and nothing matches it: a thread of its own, shown
        // under its own subject.
        let stranger = vec![
            mail(
                "2",
                "Re: Elsewhere",
                "b@example.com",
                &["ghost@example.com"],
            ),
            mail("1", "Hello", "a@example.com", &[]),
        ];
        let threads = group(&stranger);
        assert_eq!(threads.len(), 2);
        assert_eq!(
            threads[0].subject, "Elsewhere",
            "the prefixes come off its own subject"
        );
    }

    /// Mail whose headers were lost — everything cached before schema v7 — is
    /// grouped by subject, but only where one of the subjects looks like an
    /// answer. Two mails that merely share a subject stay apart.
    #[test]
    fn an_answer_without_headers_joins_the_subject_it_answers() {
        let answered = vec![mail("2", "Re: Hello", "", &[]), mail("1", "Hello", "", &[])];
        let threads = group(&answered);
        assert_eq!(threads.len(), 1, "an answer joins the plain subject");
        assert_eq!(threads[0].subject, "Hello");

        let twice = vec![
            mail("2", "Re: Hello", "", &[]),
            mail("1", "Re: Hello", "", &[]),
        ];
        assert_eq!(group(&twice).len(), 1, "two answers are one conversation");

        let unrelated = vec![
            mail("2", "Invoice 42", "", &[]),
            mail("1", "Invoice 42", "", &[]),
        ];
        assert_eq!(
            group(&unrelated).len(),
            2,
            "a shared subject alone is not a thread"
        );
    }

    /// A thread counts what it holds: the unread mail in it, and whether any of
    /// them came with a file. Threads keep the list's own order — the newest
    /// mail in the thread is where the thread sits.
    #[test]
    fn a_thread_counts_its_own_and_keeps_the_lists_order() {
        let mut reply = mail("2", "Re: Hello", "b@example.com", &["a@example.com"]);
        reply.attachments = vec![Attachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 1024,
        }];
        let mut root = mail("1", "Hello", "a@example.com", &[]);
        root.seen = true;
        let unread_alone = mail("3", "Something else", "c@example.com", &[]);

        let threads = group(&[reply, root, unread_alone]);
        assert_eq!(threads.len(), 2);
        assert_eq!(
            threads[0].newest, 0,
            "the thread whose newest mail is at the top comes first"
        );
        assert_eq!(threads[0].unread, 1);
        assert!(threads[0].has_attachments);
        assert_eq!(threads[1].newest, 2);
        assert_eq!(threads[1].unread, 1);
        assert!(!threads[1].has_attachments);
    }

    /// The prefixes a subject is stripped of, in the case and the punctuation
    /// the senders use — and nothing else: a colon halfway into a sentence is
    /// part of the subject.
    #[test]
    fn subject_prefixes_come_off_the_front_only() {
        for subject in [
            "Re: Hello",
            "RE: Hello",
            "re:Hello",
            "Re[2]: Hello",
            "Re(3): Hello",
            "Fwd: Hello",
            "FW: Hello",
            "Re: Fwd: Hello",
        ] {
            assert_eq!(base_subject(subject), "Hello", "{subject}");
        }
        // The CJK prefixes, with either colon.
        for subject in ["答复: 你好", "回复：你好", "转发: 你好", "回覆：你好"] {
            assert_eq!(base_subject(subject), "你好", "{subject}");
        }
        for subject in [
            "Hello",
            "Regarding: the plan",
            "Note: the plan",
            "Redis: a database",
            "",
        ] {
            assert_eq!(base_subject(subject), subject, "{subject}");
        }

        // Whitespace and case are what the match folds away, not the display.
        assert_eq!(normalized_subject("  Re:   Hello  "), "hello");
        assert_eq!(base_subject("Re:   Hello  "), "Hello");
    }

    /// A chain of a thousand replies is one thread: the answer is what the
    /// folder's own order was, and grouping it is a walk.
    #[test]
    fn a_long_chain_is_one_thread() {
        let mut messages = vec![mail("1000", "Re: Hello", "id-1000", &["id-999"])];
        for index in (0..999).rev() {
            let chain: Vec<String> = (0..=index).map(|n| format!("id-{n}")).collect();
            let chain: Vec<&str> = chain.iter().map(String::as_str).collect();
            messages.push(mail(
                &format!("{}", index + 1),
                "Re: Hello",
                &format!("id-{}", index + 1),
                &chain,
            ));
        }
        let threads = group(&messages);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].messages.len(), 1000);
        assert_eq!(threads[0].newest, 0);
    }
}
