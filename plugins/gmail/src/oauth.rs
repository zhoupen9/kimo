//! The Google leg of OAuth2: the authorization URL the user's browser is
//! sent to, PKCE, the token exchange, and the XOAUTH2 strings IMAP and SMTP
//! authenticate with.
//!
//! One thing is deliberately *not* here: a client id. Google issues those to
//! the deployment, not to the software, so the id comes from the account's
//! config (`client_id`) and `auth.begin` refuses to invent one. The scope is
//! `https://mail.google.com/` — the one scope that covers IMAP and SMTP
//! access and nothing else.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::abi;

pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const SCOPE: &str = "https://mail.google.com/";

/// A sign-in the plugin has started and the host is half-way through: what
/// `auth.begin` minted and `auth.continue` needs to finish it.
pub struct Pending {
    pub client_id: String,
    /// The secret that goes with the id, when the deployment's client has
    /// one. Google issues desktop clients a secret and refuses the exchange
    /// without it, which is why the form asks for it; Entra's public clients
    /// have none, which is why it is optional.
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub state: String,
    pub verifier: String,
    pub login_hint: Option<String>,
}

/// Everything about a sign-in that is random, in one place: the state the
/// redirect must echo and the PKCE verifier that makes the code useless to
/// anyone watching the browser.
pub fn pending(
    client_id: &str,
    client_secret: Option<String>,
    port: u16,
    login_hint: Option<String>,
) -> Result<Pending, String> {
    let mut seed = [0u8; 48];
    if !abi::fill_random(&mut seed) {
        return Err("the host refused the random bytes a sign-in needs".to_string());
    }
    Ok(Pending {
        client_id: client_id.to_string(),
        client_secret,
        redirect_uri: format!("http://localhost:{port}/"),
        state: hex(&seed[..16]),
        verifier: URL_SAFE_NO_PAD.encode(&seed[16..]),
        login_hint,
    })
}

/// The page the browser is opened on: authorization code + PKCE, offline
/// access so a refresh token comes back, and the loopback redirect the host
/// is holding a socket for.
pub fn authorize_url(pending: &Pending) -> String {
    let mut url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&\
         code_challenge={}&code_challenge_method=S256&access_type=offline",
        AUTH_ENDPOINT,
        percent_encoded(&pending.client_id),
        percent_encoded(&pending.redirect_uri),
        percent_encoded(SCOPE),
        percent_encoded(&pending.state),
        challenge(&pending.verifier),
    );
    if let Some(hint) = &pending.login_hint {
        url.push_str(&format!("&login_hint={}", percent_encoded(hint)));
    }
    url
}

/// The S256 challenge of a verifier: BASE64URL(SHA-256(verifier)) with the
/// padding stripped, exactly as RFC 7636 §4.2 spells it.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The POST the code is traded for tokens with: form-encoded, PKCE, and the
/// client's secret when the deployment has one. Google's desktop clients do,
/// and their token endpoint refuses the exchange without it.
pub fn exchange_request(pending: &Pending, code: &str) -> Value {
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", pending.verifier.as_str()),
        ("redirect_uri", pending.redirect_uri.as_str()),
        ("client_id", pending.client_id.as_str()),
    ];
    if let Some(secret) = &pending.client_secret {
        pairs.push(("client_secret", secret.as_str()));
    }
    json!({
        "method": "POST",
        "url": TOKEN_ENDPOINT,
        "headers": [{ "name": "Content-Type", "value": "application/x-www-form-urlencoded" }],
        "body": base64_std(form(&pairs).as_bytes()),
    })
}

/// The POST a refresh token is traded for a fresh access token with. The
/// secret rides along for the same reason it does the exchange: Google's
/// desktop clients have one and the endpoint wants it again.
pub fn refresh_request(client_id: &str, client_secret: Option<&str>, refresh_token: &str) -> Value {
    let mut pairs = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        pairs.push(("client_secret", secret));
    }
    json!({
        "method": "POST",
        "url": TOKEN_ENDPOINT,
        "headers": [{ "name": "Content-Type", "value": "application/x-www-form-urlencoded" }],
        "body": base64_std(form(&pairs).as_bytes()),
    })
}

/// The tokens a grant or a refresh produced, with `expires_in` seconds
/// already turned into the absolute `expires_at` the host stores. The debug
/// print is lengths only: a token in a log line is a leaked token.
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub scope: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field(
                "access_token",
                &format!("{} bytes", self.access_token.len()),
            )
            .field(
                "refresh_token",
                &self
                    .refresh_token
                    .as_ref()
                    .map(|token| format!("{} bytes", token.len())),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

pub fn parse_tokens(body: &Value) -> Result<Tokens, String> {
    let error = || {
        body.get("error_description")
            .and_then(Value::as_str)
            .or_else(|| body.get("error").and_then(Value::as_str))
            .unwrap_or("the token endpoint answered with nothing usable")
            .to_string()
    };
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(error)?;
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires_at = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| (abi::now_ms().max(0) as u64) + seconds * 1000);
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Tokens {
        access_token: access_token.to_string(),
        refresh_token,
        expires_at,
        scope,
    })
}

/// The SASL XOAUTH2 initial response for Gmail's IMAP and SMTP: the fixed
/// `user`/`auth` pair, each field followed by 0x01, closed by an empty one.
/// A rejected token comes back as a base64 JSON status on the continuation
/// line, never inside this string.
pub fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\u{1}auth=Bearer {token}\u{1}\u{1}")
}

pub fn base64_std(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Percent-encode everything outside RFC 3986's unreserved set, the encoding
/// a query component (not a whole URL) needs.
pub fn percent_encoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `application/x-www-form-urlencoded` body: every value escaped, pairs
/// joined with `&`. The values here are tokens and client ids — nothing a
/// form would treat specially survives unescaped.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", key, percent_encoded(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pkce_challenge_is_the_rfc_7636_vector() {
        // Appendix B of RFC 7636: this verifier must produce this challenge.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn the_authorization_url_names_every_part() {
        let pending = Pending {
            client_id: "client-1".to_string(),
            client_secret: None,
            redirect_uri: "http://localhost:8914/".to_string(),
            state: "s".to_string(),
            verifier: "v".to_string(),
            login_hint: Some("user@gmail.com".to_string()),
        };
        let url = authorize_url(&pending);
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8914%2F"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("scope=https%3A%2F%2Fmail.google.com%2F"));
        assert!(url.contains("login_hint=user%40gmail.com"));
    }

    #[test]
    fn the_xoauth2_string_is_the_fixed_shape() {
        assert_eq!(
            xoauth2("user@gmail.com", "tok"),
            "user=user@gmail.com\u{1}auth=Bearer tok\u{1}\u{1}"
        );
    }

    #[test]
    fn tokens_parse_with_an_absolute_expiry() {
        let tokens = parse_tokens(&json!({
            "access_token": "a",
            "refresh_token": "r",
            "expires_in": 3600,
            "scope": SCOPE
        }))
        .unwrap();
        assert_eq!(tokens.access_token, "a");
        assert_eq!(tokens.refresh_token.as_deref(), Some("r"));
        assert_eq!(tokens.expires_at, Some(1_700_000_000_000u64 + 3_600_000));
    }

    #[test]
    fn a_refused_grant_gives_its_own_words_back() {
        let error = parse_tokens(&json!({
            "error": "invalid_grant",
            "error_description": "Bad Request"
        }))
        .unwrap_err();
        assert_eq!(error, "Bad Request");
    }

    #[test]
    fn the_exchange_body_is_form_encoded() {
        let pending = Pending {
            client_id: "cid".to_string(),
            client_secret: Some("secret-1".to_string()),
            redirect_uri: "http://localhost:1/".to_string(),
            state: String::new(),
            verifier: "v-v".to_string(),
            login_hint: None,
        };
        let request = exchange_request(&pending, "c/de");
        let body = request["headers"][0]["value"].as_str().unwrap();
        assert_eq!(body, "application/x-www-form-urlencoded");
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(request["body"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(decoded.starts_with("grant_type=authorization_code&code=c%2Fde&"));
        assert!(decoded.contains("code_verifier=v-v"));
        // The secret rides last: Google will not trade the code without it.
        assert!(
            decoded.contains("&client_id=cid&client_secret=secret-1"),
            "{decoded}"
        );
    }
}
