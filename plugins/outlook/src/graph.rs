//! The Microsoft Graph client: everything the provider operations need over
//! the host's HTTPS import, with the REST shapes `docs/mail-spec.md` §6.5
//! maps.
//!
//! Identities are Graph's own: a message id is its `id` (with
//! `Prefer: IdType="ImmutableId"` asked on every request, so an id survives
//! a move), and a folder is resolved by display name per call because the
//! host's folder rows are keyed by names. Folder *display* names are not
//! unique or stable; first match wins, and that is the module's honest
//! limitation until the store grows a folder-id column.

use serde_json::{json, Value};

use crate::abi;
use crate::oauth;

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// A raw message may be anything a mailbox holds; this is the cap the
/// module will copy in one piece, and a larger one is an error rather than
/// a truncation.
pub const RAW_CAP: usize = 12 * 1024 * 1024;
/// Folder listings and id pages: small, and many of them.
pub const JSON_CAP: usize = 4 * 1024 * 1024;
/// How deep a folder listing walks: the top level plus one nested level,
/// which is every folder Outlook's own UI nests.
pub const FOLDER_DEPTH: u32 = 2;

/// The failure shapes the operations turn into error codes: a refused
/// token is `auth_failed`, a throttled request is `rate_limited` with the
/// server's own wait, and everything else is Graph's words.
pub enum Failed {
    Auth(String),
    Rate {
        message: String,
        retry_after_ms: Option<u64>,
    },
    Protocol(String),
}

pub struct Graph {
    token: String,
}

/// One folder as the listing carries it.
pub struct Folder {
    pub id: String,
    pub display: String,
}

/// One message summary from an id page.
pub struct Summary {
    pub id: String,
    pub read: bool,
    pub flagged: bool,
}

/// One folder entry of a listing page, as the collect walk consumes it.
pub struct FolderEntry {
    pub id: String,
    pub display: String,
    pub children: u64,
}

pub fn parse_folder_entries(page: &Value) -> Vec<FolderEntry> {
    let mut entries = Vec::new();
    let Some(list) = page.get("value").and_then(Value::as_array) else {
        return entries;
    };
    for entry in list {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        entries.push(FolderEntry {
            id: id.to_string(),
            display: entry
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            children: entry
                .get("childFolderCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    entries
}

/// The summaries of one messages page, in the page's own order.
pub fn parse_summaries(page: &Value) -> Vec<Summary> {
    let mut summaries = Vec::new();
    let Some(list) = page.get("value").and_then(Value::as_array) else {
        return summaries;
    };
    for entry in list {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        summaries.push(Summary {
            id: id.to_string(),
            read: entry
                .get("isRead")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            flagged: entry
                .get("flag")
                .and_then(|flag| flag.get("flagStatus"))
                .and_then(Value::as_str)
                .is_some_and(|status| status == "flagged"),
        });
    }
    summaries
}

impl Graph {
    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
        }
    }

    // ----- request plumbing --------------------------------------------------

    /// One JSON call. The response body is decoded whether the status is a
    /// 2xx or not: Graph's errors are JSON too, and they are the best words
    /// a failure has.
    fn call(&self, method: &str, url: &str, body: Option<&Value>) -> Result<Value, Failed> {
        let mut request = json!({
            "method": method,
            "url": url,
            "headers": [
                { "name": "Authorization", "value": format!("Bearer {}", self.token) },
                { "name": "Prefer", "value": "IdType=\"ImmutableId\"" },
                { "name": "Accept", "value": "application/json" },
            ],
        });
        if let Some(body) = body {
            request["headers"]
                .as_array_mut()
                .expect("built above")
                .push(json!({ "name": "Content-Type", "value": "application/json" }));
            request["body"] = Value::String(oauth::base64_std(
                serde_json::to_vec(body)
                    .map_err(|error| {
                        Failed::Protocol(format!("the request cannot be serialised: {error}"))
                    })?
                    .as_slice(),
            ));
        }
        let envelope = abi::http_envelope(&request, JSON_CAP).map_err(Failed::Protocol)?;
        let status = envelope.get("status").and_then(Value::as_u64).unwrap_or(0);
        let body = abi::decoded_body(&envelope)
            .map_err(Failed::Protocol)
            .and_then(|bytes| {
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|error| Failed::Protocol(format!("response: {error}")))
            })?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        Err(failed_status(status, &body, &envelope))
    }

    fn get(&self, url: &str) -> Result<Value, Failed> {
        self.call("GET", url, None)
    }

    fn patch(&self, url: &str, body: Value) -> Result<(), Failed> {
        self.call("PATCH", url, Some(&body)).map(|_| ())
    }

    fn post(&self, url: &str, body: Value) -> Result<(), Failed> {
        self.call("POST", url, Some(&body)).map(|_| ())
    }

    fn delete(&self, url: &str) -> Result<(), Failed> {
        self.call("DELETE", url, None).map(|_| ())
    }

    /// The raw RFC 5322 bytes of one message: `GET …/$value`, which answers
    /// a MIME stream rather than JSON.
    pub fn raw(&self, id: &str) -> Result<Vec<u8>, Failed> {
        let request = json!({
            "method": "GET",
            "url": format!("{GRAPH}/me/messages/{id}/$value"),
            "headers": [
                { "name": "Authorization", "value": format!("Bearer {}", self.token) },
                { "name": "Prefer", "value": "IdType=\"ImmutableId\"" },
            ],
        });
        let envelope = abi::http_envelope(&request, RAW_CAP).map_err(Failed::Protocol)?;
        let status = envelope.get("status").and_then(Value::as_u64).unwrap_or(0);
        let body = abi::decoded_body(&envelope).map_err(Failed::Protocol)?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        Err(failed_status(status, &parsed, &envelope))
    }

    // ----- folders -----------------------------------------------------------

    /// The mailboxes: top level plus one nested level, following
    /// `@odata.nextLink` because a folder set is paged too.
    pub fn folders(&self) -> Result<Vec<Folder>, Failed> {
        let mut folders = Vec::new();
        self.collect_folders(
            &format!("{GRAPH}/me/mailFolders"),
            &mut folders,
            FOLDER_DEPTH,
        )?;
        Ok(folders)
    }

    fn collect_folders(
        &self,
        start: &str,
        out: &mut Vec<Folder>,
        depth: u32,
    ) -> Result<(), Failed> {
        let mut next = Some(start.to_string());
        while let Some(url) = next {
            let page = self.get(&url)?;
            for entry in parse_folder_entries(&page) {
                if depth > 1 && entry.children > 0 {
                    self.collect_folders(
                        &format!("{GRAPH}/me/mailFolders/{}/childFolders", entry.id),
                        out,
                        depth - 1,
                    )?;
                }
                out.push(Folder {
                    id: entry.id,
                    display: entry.display,
                });
            }
            next = page
                .get("@odata.nextLink")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Ok(())
    }

    /// The folder id a display name names. Names are what the host stores;
    /// ids are what Graph wants, and the listing is the bridge between them.
    pub fn folder_id(&self, display: &str) -> Result<String, Failed> {
        self.folders()?
            .into_iter()
            .find(|folder| folder.display == display)
            .map(|folder| folder.id)
            .ok_or_else(|| Failed::Protocol(format!("the mailbox has no folder named {display:?}")))
    }

    // ----- messages ----------------------------------------------------------

    /// One page of message summaries, newest first: `$skip` pages down,
    /// which is the older-mail walk in Graph's own arithmetic.
    pub fn summaries(&self, folder_id: &str, top: u32, skip: u32) -> Result<Vec<Summary>, Failed> {
        let url = format!(
            "{GRAPH}/me/mailFolders/{folder_id}/messages\
             ?$select=id,isRead,flag&$orderby=receivedDateTime%20desc&$top={top}&$skip={skip}"
        );
        let page = self.get(&url)?;
        Ok(parse_summaries(&page))
    }

    pub fn set_read(&self, id: &str, read: bool) -> Result<(), Failed> {
        self.patch(
            &format!("{GRAPH}/me/messages/{id}"),
            json!({ "isRead": read }),
        )
    }

    pub fn set_flagged(&self, id: &str, flagged: bool) -> Result<(), Failed> {
        let status = if flagged { "flagged" } else { "notFlagged" };
        self.patch(
            &format!("{GRAPH}/me/messages/{id}"),
            json!({ "flag": { "flagStatus": status } }),
        )
    }

    /// Graph's soft delete: `DELETE` moves the message to Deleted Items,
    /// which is exactly the soft-delete semantics the spec asks Outlook for.
    pub fn delete_message(&self, id: &str) -> Result<(), Failed> {
        self.delete(&format!("{GRAPH}/me/messages/{id}"))
    }

    /// A KQL search across the mailbox: `$search` takes the expression in
    /// double quotes, escaped; the expression is the user's, not Graph's to
    /// re-parse.
    pub fn search(&self, expression: &str) -> Result<Vec<String>, Failed> {
        let quoted = format!(
            "\"{}\"",
            expression.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let url = format!(
            "{GRAPH}/me/messages?$search={}&$select=id&$top=50",
            oauth::percent_encoded(&quoted)
        );
        let page = self.get(&url)?;
        let mut ids = Vec::new();
        if let Some(list) = page.get("value").and_then(Value::as_array) {
            for entry in list {
                if let Some(id) = entry.get("id").and_then(Value::as_str) {
                    ids.push(id.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Submit through `sendMail`, with the copy filed by Graph itself
    /// (`saveToSentItems`): the module declares `send.save_copy`, and the
    /// host then knows not to append a second one.
    pub fn send(&self, message: Value) -> Result<(), Failed> {
        self.post(
            &format!("{GRAPH}/me/sendMail"),
            json!({
                "message": message,
                "saveToSentItems": true,
            }),
        )
    }

    /// The signed-in mailbox answers any small GET: this is the health
    /// check, and the effective-capability probe of a live token.
    pub fn alive(&self) -> Result<(), Failed> {
        self.get(&format!("{GRAPH}/me/mailFolders?$top=1&$select=id"))
            .map(|_| ())
    }
}

/// A non-2xx as the module's failure shape, with Graph's own words.
fn failed_status(status: u64, body: &Value, envelope: &Value) -> Failed {
    let message = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("the request was refused")
        .to_string();
    match status {
        401 | 403 => Failed::Auth(format!("the token was refused — sign in again ({message})")),
        429 => {
            let retry_after_ms =
                envelope
                    .get("headers")
                    .and_then(Value::as_array)
                    .and_then(|headers| {
                        headers.iter().find_map(|header| {
                            (header
                                .get("name")
                                .and_then(Value::as_str)?
                                .eq_ignore_ascii_case("retry-after"))
                            .then(|| {
                                header
                                    .get("value")
                                    .and_then(Value::as_str)
                                    .and_then(|seconds| seconds.trim().parse::<u64>().ok())
                                    .map(|seconds| seconds * 1000)
                            })?
                        })
                    });
            Failed::Rate {
                message: format!("Graph asked us to slow down ({message})"),
                retry_after_ms,
            }
        }
        other => Failed::Protocol(format!("Graph answered {other}: {message}")),
    }
}

/// The Graph message object `sendMail` takes, built from the host's
/// rendered RFC 5322 bytes and the envelope that names the recipients. The
/// raw mail is parsed rather than forwarded because Graph takes a message
/// object, not MIME; attachments ride along as `fileAttachment`s.
pub fn message_for_send(recipients: &[String], raw: &[u8]) -> Result<Value, String> {
    use mail_parser::{MessageParser, MimeHeaders};
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or("the rendered message cannot be parsed")?;

    // Text first: Kimo composes plain text (hard rule 4), and mail-parser's
    // `body_html` would *convert* text to HTML — a transformation a sent
    // mail should not silently get. HTML rides along only when the message
    // actually has an HTML part.
    let body = if let Some(text) = parsed.body_text(0) {
        json!({ "contentType": "text", "content": text.into_owned() })
    } else if let Some(html) = parsed.body_html(0) {
        json!({ "contentType": "html", "content": html.into_owned() })
    } else {
        json!({ "contentType": "text", "content": "" })
    };

    let mut attachments = Vec::new();
    for part in parsed.attachments() {
        let Some(name) = part.attachment_name() else {
            continue;
        };
        attachments.push(json!({
            "@odata.type": "#microsoft.graph.fileAttachment",
            "name": name,
            "contentBytes": oauth::base64_std(part.contents()),
        }));
    }

    Ok(json!({
        "subject": parsed.subject().unwrap_or_default(),
        "body": body,
        "toRecipients": recipients
            .iter()
            .map(|address| json!({ "emailAddress": { "address": address } }))
            .collect::<Vec<_>>(),
        "attachments": attachments,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_page_parses_with_its_names_and_children() {
        let page = json!({
            "value": [
                { "id": "aaa=", "displayName": "Inbox", "childFolderCount": 1 },
                { "id": "bbb=", "displayName": "Sent Items", "childFolderCount": 0 },
                { "displayName": "an entry with no id is skipped" }
            ]
        });
        let entries = parse_folder_entries(&page);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].display, "Inbox");
        assert_eq!(entries[0].children, 1);
        // An entry with no display name keeps its id, which is still a name
        // the caller can resolve back.
        let nameless = json!({ "value": [{ "id": "ccc=" }] });
        assert_eq!(parse_folder_entries(&nameless)[0].display, "ccc=");
    }

    #[test]
    fn a_messages_page_parses_its_summaries() {
        let page = json!({
            "value": [
                {
                    "id": "m1=",
                    "isRead": true,
                    "flag": { "flagStatus": "flagged" }
                },
                { "id": "m2=", "isRead": false, "flag": { "flagStatus": "notFlagged" } }
            ]
        });
        let summaries = parse_summaries(&page);
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].read && summaries[0].flagged);
        assert!(!summaries[1].read && !summaries[1].flagged);
    }

    #[test]
    fn a_search_expression_is_quoted_and_escaped() {
        assert_eq!(
            format!(
                "\"{}\"",
                "from:ada \"important\""
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            ),
            "\"from:ada \\\"important\\\"\""
        );
    }

    #[test]
    fn a_sent_message_carries_its_body_recipients_and_attachments() {
        let raw = concat!(
            "From: me@contoso.com\r\n",
            "To: ada@example.com\r\n",
            "Subject: Report\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Please pay it.\r\n",
            "--b\r\n",
            "Content-Type: application/pdf; name=\"report.pdf\"\r\n",
            "Content-Disposition: attachment; filename=\"report.pdf\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--b--\r\n"
        );
        let message = message_for_send(&["ada@example.com".to_string()], raw.as_bytes()).unwrap();
        assert_eq!(message["subject"], "Report");
        assert_eq!(message["body"]["contentType"], "text");
        assert_eq!(message["body"]["content"], "Please pay it.");
        assert_eq!(
            message["toRecipients"][0]["emailAddress"]["address"],
            "ada@example.com"
        );
        assert_eq!(message["attachments"][0]["name"], "report.pdf");
        // "aGVsbG8=" is "hello".
        assert_eq!(message["attachments"][0]["contentBytes"], "aGVsbG8=");
    }

    #[test]
    fn a_refused_token_is_an_auth_failure_with_graphs_words() {
        let body = json!({ "error": { "message": "Access token is empty" } });
        match failed_status(401, &body, &Value::Null) {
            Failed::Auth(message) => assert!(message.contains("sign in again")),
            _ => panic!("401 is an auth failure"),
        }
        let body = json!({ "error": { "message": "too many requests" } });
        match failed_status(429, &body, &json!({})) {
            Failed::Rate { retry_after_ms, .. } => assert_eq!(retry_after_ms, None),
            _ => panic!("429 is a rate failure"),
        }
        let envelope = json!({
            "headers": [{ "name": "Retry-After", "value": "5" }]
        });
        match failed_status(429, &json!({}), &envelope) {
            Failed::Rate { retry_after_ms, .. } => assert_eq!(retry_after_ms, Some(5000)),
            _ => panic!("429 is a rate failure"),
        }
    }
}
