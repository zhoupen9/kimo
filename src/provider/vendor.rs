//! The vendor layer: what the big providers need beyond "IMAP and SMTP".
//!
//! The protocol adapters speak the standard; a vendor profile says *where* to
//! speak it and what its sign-in needs. It is data, not a type per vendor, for
//! the same reason `crate::folders` keeps its names in a table: Gmail, Outlook,
//! iCloud, Yahoo, QQ and NetEase differ in endpoints, in whether the password is
//! the account's own or an app password, and in which mailbox a sent copy
//! belongs in — none of them needs different wire behaviour (that is the IMAP
//! adapter's job). A vendor that ever does gets a function here; the account
//! form and `sync::append_to_sent` are its two callers.
//!
//! **Vendor identity is inferred, not stored.** It is a pure function of the
//! account's endpoints, every preset fills one of the hosts below, and a custom
//! host correctly falls back to the existing name heuristics
//! (`crate::folders::kind_of`) — so nothing breaks if it is forgotten. The
//! alternative, a `vendor` column on `accounts`, would mean a schema migration
//! and a new field on every `Account` literal for a value that is already
//! derivable; `VendorKey::of` is the seam if a vendor ever needs an override
//! that inference cannot express.

use std::collections::BTreeSet;

use crate::folders::{self, FolderKind};
use crate::models::{Account, TlsMode};
use crate::protocol::imap::decode_mailbox_name;

/// Which vendor an account talks to. `Custom` is every server the table does
/// not recognize, including POP3 drops and local relays.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum VendorKey {
    Custom,
    Gmail,
    Outlook,
    ICloud,
    Yahoo,
    QQ,
    NetEase,
}

impl VendorKey {
    /// The picker's order: the fallback first, then the providers whose
    /// presets exist.
    pub const ALL: &'static [VendorKey] = &[
        VendorKey::Custom,
        VendorKey::Gmail,
        VendorKey::Outlook,
        VendorKey::ICloud,
        VendorKey::Yahoo,
        VendorKey::QQ,
        VendorKey::NetEase,
    ];

    pub fn label(self) -> &'static str {
        self.profile().name
    }

    /// The value `settings.json`'s `disabled_vendors` stores. Custom is never
    /// written: plain IMAP, POP3 and SMTP are not a preset and cannot be
    /// switched off.
    pub fn as_str(self) -> &'static str {
        match self {
            VendorKey::Custom => "custom",
            VendorKey::Gmail => "gmail",
            VendorKey::Outlook => "outlook",
            VendorKey::ICloud => "icloud",
            VendorKey::Yahoo => "yahoo",
            VendorKey::QQ => "qq",
            VendorKey::NetEase => "netease",
        }
    }

    /// The inverse of [`VendorKey::as_str`]; `None` for a key this build does
    /// not know, so a settings file from a newer version keeps the rest of its
    /// preferences.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "custom" => Some(VendorKey::Custom),
            "gmail" => Some(VendorKey::Gmail),
            "outlook" => Some(VendorKey::Outlook),
            "icloud" => Some(VendorKey::ICloud),
            "yahoo" => Some(VendorKey::Yahoo),
            "qq" => Some(VendorKey::QQ),
            "netease" => Some(VendorKey::NetEase),
            _ => None,
        }
    }

    pub fn profile(self) -> &'static VendorProfile {
        match self {
            VendorKey::Custom => &CUSTOM,
            VendorKey::Gmail => &GMAIL,
            VendorKey::Outlook => &OUTLOOK,
            VendorKey::ICloud => &ICLOUD,
            VendorKey::Yahoo => &YAHOO,
            VendorKey::QQ => &QQ,
            VendorKey::NetEase => &NETEASE,
        }
    }

    /// Infer the vendor from a server host. Trimmed, ASCII-lowercased and with
    /// a trailing dot ignored, matched on whole domain labels (so
    /// `notgmail.com` is Custom, not Gmail).
    pub fn infer(host: &str) -> VendorKey {
        let host = normalize(host);
        if host.is_empty() {
            return VendorKey::Custom;
        }
        if under(&host, "gmail.com") || under(&host, "googlemail.com") {
            return VendorKey::Gmail;
        }
        if under(&host, "outlook.com")
            || under(&host, "office365.com")
            || under(&host, "office.com")
        {
            return VendorKey::Outlook;
        }
        if under(&host, "mail.me.com") || under(&host, "icloud.com") {
            return VendorKey::ICloud;
        }
        if under(&host, "yahoo.com") || under(&host, "yahoo.co.jp") {
            return VendorKey::Yahoo;
        }
        if under(&host, "qq.com") || under(&host, "foxmail.com") {
            return VendorKey::QQ;
        }
        if under(&host, "163.com") || under(&host, "126.com") || under(&host, "yeah.net") {
            return VendorKey::NetEase;
        }
        VendorKey::Custom
    }

    /// The vendor of a configured account: its incoming host, or — when that
    /// one is a custom relay — its outgoing host.
    pub fn of(account: &Account) -> VendorKey {
        let incoming = VendorKey::infer(&account.host);
        if incoming != VendorKey::Custom {
            return incoming;
        }
        account
            .smtp
            .as_ref()
            .map_or(VendorKey::Custom, |smtp| VendorKey::infer(&smtp.host))
    }
}

/// One endpoint a preset fills in: host, port and the TLS the port implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: &'static str,
    pub port: u16,
    pub tls: TlsMode,
}

/// What a provider's sign-in needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    /// The account's own password (Custom servers, local relays).
    Password,
    /// An app password or authorization code issued by the provider.
    AppPassword,
    /// OAuth2 only. Kimo does not implement OAuth2 (AGENTS.md, hard rule 4),
    /// so the form says the account cannot sign in rather than letting the
    /// server refuse it cryptically.
    OAuthOnly,
}

/// Everything the interface and the account form need to know about a vendor.
#[derive(Clone, Copy, Debug)]
pub struct VendorProfile {
    pub name: &'static str,
    /// The IMAP endpoint a preset fills in; `None` for Custom.
    pub imap: Option<Endpoint>,
    /// The submission endpoint a preset fills in; `None` for Custom.
    pub smtp: Option<Endpoint>,
    pub auth: AuthKind,
    /// One sentence for the form: what the sign-in needs, or why it cannot
    /// work. `None` for a custom server whose requirements are the user's.
    pub note: Option<&'static str>,
    /// Mailbox names (decoded, case-insensitive) a sent copy prefers before
    /// the name classification runs, so a user folder called "Sent" cannot win
    /// over the provider's own Sent mailbox.
    pub preferred_sent: &'static [&'static str],
}

static CUSTOM: VendorProfile = VendorProfile {
    name: "Custom (IMAP/POP3)",
    imap: None,
    smtp: None,
    auth: AuthKind::Password,
    note: None,
    preferred_sent: &[],
};

static GMAIL: VendorProfile = VendorProfile {
    name: "Gmail",
    imap: Some(Endpoint {
        host: "imap.gmail.com",
        port: 993,
        tls: TlsMode::Implicit,
    }),
    smtp: Some(Endpoint {
        host: "smtp.gmail.com",
        port: 465,
        tls: TlsMode::Implicit,
    }),
    auth: AuthKind::AppPassword,
    note: Some(
        "Gmail refuses the account password: create an app password (Google Account → \
         Security → 2-Step Verification → App passwords). OAuth2 is the Gmail plugin's \
         path, not this built-in one.",
    ),
    preferred_sent: &["[Gmail]/Sent Mail"],
};

static OUTLOOK: VendorProfile = VendorProfile {
    name: "Outlook / Microsoft 365",
    imap: Some(Endpoint {
        host: "outlook.office365.com",
        port: 993,
        tls: TlsMode::Implicit,
    }),
    smtp: Some(Endpoint {
        host: "smtp.office365.com",
        port: 587,
        tls: TlsMode::StartTls,
    }),
    auth: AuthKind::OAuthOnly,
    note: Some(
        "Microsoft requires OAuth2 for Outlook and Microsoft 365. Kimo does not support \
         OAuth2, so this account cannot sign in and the server will refuse the password.",
    ),
    preferred_sent: &["Sent Items"],
};

static ICLOUD: VendorProfile = VendorProfile {
    name: "iCloud Mail",
    imap: Some(Endpoint {
        host: "imap.mail.me.com",
        port: 993,
        tls: TlsMode::Implicit,
    }),
    smtp: Some(Endpoint {
        host: "smtp.mail.me.com",
        port: 587,
        tls: TlsMode::StartTls,
    }),
    auth: AuthKind::AppPassword,
    note: Some(
        "Use an app-specific password created at appleid.apple.com; two-factor \
         authentication needs one and the account password is refused.",
    ),
    preferred_sent: &["Sent Messages"],
};

static YAHOO: VendorProfile = VendorProfile {
    name: "Yahoo Mail",
    imap: Some(Endpoint {
        host: "imap.mail.yahoo.com",
        port: 993,
        tls: TlsMode::Implicit,
    }),
    smtp: Some(Endpoint {
        host: "smtp.mail.yahoo.com",
        port: 465,
        tls: TlsMode::Implicit,
    }),
    auth: AuthKind::AppPassword,
    note: Some("Use an app password generated in Yahoo Account Security."),
    preferred_sent: &["Sent"],
};

static QQ: VendorProfile = VendorProfile {
    name: "QQ Mail",
    imap: Some(Endpoint {
        host: "imap.qq.com",
        port: 993,
        tls: TlsMode::Implicit,
    }),
    smtp: Some(Endpoint {
        host: "smtp.qq.com",
        port: 465,
        tls: TlsMode::Implicit,
    }),
    auth: AuthKind::AppPassword,
    note: Some(
        "Use the authorization code from QQ Mail's settings (设置 → 账户 → 开启 IMAP 服务), \
         not the account password.",
    ),
    preferred_sent: &["Sent Messages", "已发送"],
};

static NETEASE: VendorProfile = VendorProfile {
    name: "NetEase Mail (163/126/yeah)",
    imap: Some(Endpoint {
        host: "imap.163.com",
        port: 993,
        tls: TlsMode::Implicit,
    }),
    smtp: Some(Endpoint {
        host: "smtp.163.com",
        port: 465,
        tls: TlsMode::Implicit,
    }),
    auth: AuthKind::AppPassword,
    note: Some(
        "Use the client authorization code (客户端授权码) from the mailbox settings, \
         not the account password.",
    ),
    preferred_sent: &["已发送", "Sent"],
};

/// Whether a provider preset is offered, given the vendors switched off in
/// Preferences → Accounts. Custom is not a vendor: plain IMAP, POP3 and SMTP
/// are always available and cannot be switched off.
pub fn enabled(key: VendorKey, disabled: &BTreeSet<VendorKey>) -> bool {
    key == VendorKey::Custom || !disabled.contains(&key)
}

/// The vendor whose policy applies to an account: Custom when the inferred one
/// is switched off, so a disabled preset behaves exactly like an ordinary
/// server — no preset in the form, no Sent-mailbox preference.
pub fn effective_key(key: VendorKey, disabled: &BTreeSet<VendorKey>) -> VendorKey {
    if enabled(key, disabled) {
        key
    } else {
        VendorKey::Custom
    }
}

/// The mailbox a sent copy belongs in, from the folders the last `LIST` named.
///
/// The profile's own names are checked first — decoded and case-insensitively —
/// so `[Gmail]/Sent Mail` wins over a user folder called "Sent". The fallback
/// is the rule Kimo always had: the first mailbox whose decoded name classifies
/// as `FolderKind::Sent`. The answer is the wire form, which is what `APPEND`
/// has to be given.
pub fn sent_mailbox(profile: &VendorProfile, listed: &[String]) -> Option<String> {
    for preferred in profile.preferred_sent {
        if let Some(found) = listed
            .iter()
            .find(|name| decode_mailbox_name(name).eq_ignore_ascii_case(preferred))
        {
            return Some(found.clone());
        }
    }
    listed
        .iter()
        .find(|name| folders::kind_of(&decode_mailbox_name(name)) == FolderKind::Sent)
        .cloned()
}

fn normalize(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `host` is `domain` or a subdomain of it, whole labels only.
fn under(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::models::{SmtpSettings, TlsMode};

    fn listed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn hosts_are_recognized_by_their_vendor() {
        assert_eq!(VendorKey::infer("imap.gmail.com"), VendorKey::Gmail);
        assert_eq!(VendorKey::infer("smtp.googlemail.com"), VendorKey::Gmail);
        assert_eq!(
            VendorKey::infer("outlook.office365.com"),
            VendorKey::Outlook
        );
        assert_eq!(
            VendorKey::infer("smtp-mail.outlook.com"),
            VendorKey::Outlook
        );
        assert_eq!(VendorKey::infer("imap.mail.me.com"), VendorKey::ICloud);
        assert_eq!(VendorKey::infer("imap.mail.yahoo.com"), VendorKey::Yahoo);
        assert_eq!(VendorKey::infer("imap.qq.com"), VendorKey::QQ);
        assert_eq!(VendorKey::infer("imap.foxmail.com"), VendorKey::QQ);
        assert_eq!(VendorKey::infer("imap.163.com"), VendorKey::NetEase);
        assert_eq!(VendorKey::infer("imap.yeah.net"), VendorKey::NetEase);
        // Case, whitespace and a trailing dot are not a different server.
        assert_eq!(VendorKey::infer("  IMAP.GMAIL.COM.  "), VendorKey::Gmail);
    }

    #[test]
    fn an_unknown_or_empty_host_is_custom() {
        assert_eq!(VendorKey::infer("imap.example.com"), VendorKey::Custom);
        assert_eq!(VendorKey::infer(""), VendorKey::Custom);
        // A domain that merely ends in a vendor's name is not that vendor.
        assert_eq!(VendorKey::infer("notgmail.com"), VendorKey::Custom);
        assert_eq!(
            VendorKey::infer("mail.me.com.evil.example"),
            VendorKey::Custom
        );
    }

    /// Presets have to agree with the ports the security choice implies: a
    /// profile that says Implicit TLS on 587 would try to start a clear-text
    /// session on the submission port.
    #[test]
    fn presets_use_the_ports_their_security_implies() {
        for key in VendorKey::ALL {
            let profile = key.profile();
            if let Some(imap) = profile.imap {
                match imap.tls {
                    TlsMode::Implicit => assert_eq!(imap.port, 993, "{key:?} IMAP"),
                    TlsMode::StartTls => assert_eq!(imap.port, 143, "{key:?} IMAP"),
                    TlsMode::Plain => panic!("{key:?} preset is not encrypted"),
                }
            }
            if let Some(smtp) = profile.smtp {
                match smtp.tls {
                    TlsMode::Implicit => assert_eq!(smtp.port, 465, "{key:?} SMTP"),
                    TlsMode::StartTls => assert_eq!(smtp.port, 587, "{key:?} SMTP"),
                    TlsMode::Plain => panic!("{key:?} preset is not encrypted"),
                }
            }
        }
        assert!(VendorKey::Custom.profile().imap.is_none());
        assert!(VendorKey::Custom.profile().smtp.is_none());
    }

    /// Every mailbox name a profile prefers has to be one the classifier
    /// actually calls Sent, or the "prefer the provider's own" rule would
    /// silently do nothing.
    #[test]
    fn every_preferred_sent_name_classifies_as_sent() {
        for key in VendorKey::ALL {
            for name in key.profile().preferred_sent {
                assert_eq!(
                    folders::kind_of(name),
                    FolderKind::Sent,
                    "{key:?} prefers {name}, which is not Sent"
                );
            }
        }
    }

    /// A `LIST` answer names the mailbox a copy of a sent mail belongs in: the
    /// provider's own when the profile names one, the first Sent-classified
    /// mailbox otherwise — and it answers the *wire* form.
    #[test]
    fn a_sent_copy_prefers_the_providers_own_mailbox() {
        let custom = VendorKey::Custom.profile();
        assert_eq!(
            sent_mailbox(custom, &listed(&["INBOX", "Sent", "Trash"])).as_deref(),
            Some("Sent")
        );
        assert_eq!(
            sent_mailbox(custom, &listed(&["INBOX", "Sent Items"])).as_deref(),
            Some("Sent Items")
        );
        // Modified UTF-7 on the wire, 已发送 on screen.
        assert_eq!(
            sent_mailbox(custom, &listed(&["INBOX", "&XfJT0ZAB-"])).as_deref(),
            Some("&XfJT0ZAB-")
        );
        // Nothing that classifies as Sent: nowhere to put a copy.
        assert_eq!(sent_mailbox(custom, &listed(&["INBOX", "Archive"])), None);
        assert_eq!(sent_mailbox(custom, &[]), None);

        // Gmail files the copy in its own hierarchy even with a user folder
        // called "Sent" listed before it.
        let gmail = VendorKey::Gmail.profile();
        assert_eq!(
            sent_mailbox(gmail, &listed(&["INBOX", "Sent", "[Gmail]/Sent Mail"])).as_deref(),
            Some("[Gmail]/Sent Mail")
        );
        assert_eq!(
            sent_mailbox(gmail, &listed(&["INBOX", "[Gmail]/Sent Mail"])).as_deref(),
            Some("[Gmail]/Sent Mail")
        );
        // And still finds nothing when the server does not list it.
        assert_eq!(
            sent_mailbox(gmail, &listed(&["INBOX", "Sent"])).as_deref(),
            Some("Sent")
        );
    }

    #[test]
    fn outlook_says_its_sign_in_needs_oauth() {
        let outlook = VendorKey::Outlook.profile();
        assert_eq!(outlook.auth, AuthKind::OAuthOnly);
        assert!(outlook.note.unwrap_or_default().contains("OAuth2"));
        assert_eq!(VendorKey::Gmail.profile().auth, AuthKind::AppPassword);
        assert_eq!(VendorKey::Custom.profile().auth, AuthKind::Password);
    }

    /// The outgoing host names the vendor when the incoming one is a relay:
    /// a submission server is as much a provider's as a mailbox is.
    #[test]
    fn the_submission_host_names_the_vendor_when_the_incoming_one_does_not() {
        let account = Account {
            id: "work".to_string(),
            name: "Work".to_string(),
            protocol: crate::models::Protocol::Imap,
            host: "mail.relay.example".to_string(),
            port: 993,
            username: "me@example.com".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Implicit,
            smtp: Some(SmtpSettings {
                host: "smtp.gmail.com".to_string(),
                port: 465,
                tls: TlsMode::Implicit,
                login: None,
            }),
        };
        assert_eq!(VendorKey::of(&account), VendorKey::Gmail);
        assert_eq!(VendorKey::of(&account).profile().name, "Gmail");
    }

    /// A vendor's settings key is stable and round-trips; a key from a newer
    /// build is simply not this one, and the rest of settings.json stands.
    #[test]
    fn vendor_keys_round_trip_through_their_settings_strings() {
        for key in VendorKey::ALL {
            assert_eq!(VendorKey::parse(key.as_str()), Some(*key));
        }
        assert_eq!(VendorKey::parse("gmail"), Some(VendorKey::Gmail));
        assert_eq!(VendorKey::parse("Gmail"), None, "the file's keys are exact");
        assert_eq!(VendorKey::parse("proton"), None);
    }

    /// Switching a preset off is not switching off its server: the vendor
    /// becomes Custom, so the Sent copy falls back to the name classification
    /// and the form offers no preset — the standard protocol behaviour.
    #[test]
    fn a_disabled_vendor_behaves_like_a_custom_server() {
        let mut disabled = BTreeSet::new();
        assert!(enabled(VendorKey::Gmail, &disabled));
        disabled.insert(VendorKey::Gmail);
        assert!(!enabled(VendorKey::Gmail, &disabled));
        assert_eq!(
            effective_key(VendorKey::Gmail, &disabled),
            VendorKey::Custom
        );
        // The standard path is never a vendor and cannot be switched off.
        assert!(enabled(VendorKey::Custom, &disabled));
        assert_eq!(
            effective_key(VendorKey::Custom, &disabled),
            VendorKey::Custom
        );
        // Another preset is unaffected.
        assert!(enabled(VendorKey::Outlook, &disabled));
        // And a disabled vendor's policy is the Custom one: no preferred Sent
        // name, so the generic classifier decides.
        assert!(VendorKey::Custom.profile().preferred_sent.is_empty());
    }
}
