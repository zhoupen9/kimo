//! Small shared helpers: name/date formatting, slugs, the system dark-mode
//! probe, sync/search timing predicates and other leaf utilities.

use super::*;

/// Compare two display names the way a reader would: case-insensitively, so
/// "ada" and "Ada" sort together rather than one running before the other on a
/// byte value.
pub(super) fn compare_names(one: &str, two: &str) -> std::cmp::Ordering {
    one.to_lowercase().cmp(&two.to_lowercase())
}
/// Whether a `To` header names one of the addresses: a substring test, the way
/// the search popup's own `To` filter matches (`LIKE %address%`), because what
/// is stored is the display form rather than a parsed list.
pub(super) fn addressed_to(to: &str, me: &[String]) -> bool {
    let to = to.to_lowercase();
    me.iter()
        .any(|address| to.contains(&address.to_lowercase()))
}
/// Best-effort OS dark-mode detection without spawning processes or talking
/// to D-Bus: read the common GTK3/GTK4 and KDE configuration files.
pub(super) fn system_prefers_dark() -> bool {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    let Some(config_home) = config_home else {
        return false;
    };
    for rel in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
        if let Ok(text) = std::fs::read_to_string(config_home.join(rel)) {
            if ini_flag_is_true(&text, "gtk-application-prefer-dark-theme") {
                return true;
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(config_home.join("kdeglobals")) {
        if kde_scheme_is_dark(&text) {
            return true;
        }
    }
    false
}
/// `key = true` (or 1/yes) in an INI-style file.
pub(super) fn ini_flag_is_true(text: &str, key: &str) -> bool {
    text.lines().any(|line| match line.trim().split_once('=') {
        Some((k, v)) => {
            let v = v.trim();
            k.trim() == key
                && (v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        }
        None => false,
    })
}
/// KDE `ColorScheme=…` containing "dark" (e.g. BreezeDark).
pub(super) fn kde_scheme_is_dark(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.strip_prefix("colorscheme=")
            .is_some_and(|scheme| scheme.contains("dark"))
    })
}
pub(super) fn sort_messages(messages: &mut [MailMessage]) {
    messages.sort_by_key(|m| std::cmp::Reverse(m.timestamp.unwrap_or(0)));
}
pub(super) fn slug(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = s
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "account".to_string()
    } else {
        collapsed
    }
}
pub(super) fn date_label(timestamp: Option<i64>, date: &str) -> String {
    timestamp
        .map(format_timestamp)
        .unwrap_or_else(|| date.to_string())
}
pub(super) fn format_timestamp(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = crate::mail::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}
