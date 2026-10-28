//! The Microsoft Entra ID leg of OAuth2: the authorize page the user's
//! browser is sent to, PKCE, the token exchange and the refresh — a public
//! client with no secret, which is what a desktop application is.
//!
//! One thing is deliberately *not* here: a client id. Entra issues those to
//! the deployment (an app registration with a "Mobile and desktop
//! applications" redirect URI of `http://localhost`), so the id comes from
//! the account's config (`client_id`) and `auth.begin` refuses to invent
//! one.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::abi;

/// `common` serves both personal Outlook.com accounts and Entra tenants;
/// a deployment that wants a single tenant replaces it with the tenant id.
pub const AUTH_ENDPOINT: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
pub const SCOPE: &str = "offline_access Mail.ReadWrite Mail.Send";

/// A sign-in the plugin has started and the host is half-way through: what
/// `auth.begin` minted and `auth.continue` needs to finish it.
pub struct Pending {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: String,
    pub verifier: String,
    pub login_hint: Option<String>,
}

/// Everything about a sign-in that is random, in one place: the state the
/// redirect must echo and the PKCE verifier that makes the code useless to
/// anyone watching the browser.
pub fn pending(client_id: &str, port: u16, login_hint: Option<String>) -> Result<Pending, String> {
    let mut seed = [0u8; 48];
    if !abi::fill_random(&mut seed) {
        return Err("the host refused the random bytes a sign-in needs".to_string());
    }
    Ok(Pending {
        client_id: client_id.to_string(),
        redirect_uri: format!("http://localhost:{port}/"),
        state: hex(&seed[..16]),
        verifier: URL_SAFE_NO_PAD.encode(&seed[16..]),
        login_hint,
    })
}

/// The page the browser is opened on: authorization code + PKCE, offline
/// access for a refresh token, and the loopback redirect the host is
/// holding a socket for. `response_mode=query` keeps the answer on the
/// loopback request the host parses.
pub fn authorize_url(pending: &Pending) -> String {
    let mut url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&response_mode=query&scope={}&\
         state={}&code_challenge={}&code_challenge_method=S256",
        AUTH_ENDPOINT,
        percent_encoded(&pending.client_id),
        percent_encoded(&pending.redirect_uri),
        percent_encoded(SCOPE),
        percent_encoded(&pending.state),
        challenge(&pending.verifier),
    );
    if let Some(hint) = &pending.login_hint {
        url.push_str(&format!("&login_hint={}", percent_encoded(hint)));
        // A stale session is the wrong account more often than it is a
        // convenience; `prompt=select_account` makes the chooser appear.
        url.push_str("&prompt=select_account");
    }
    url
}

/// The S256 challenge of a verifier: BASE64URL(SHA-256(verifier)) with the
/// padding stripped, exactly as RFC 7636 §4.2 spells it.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The POST the code is traded for tokens with: form-encoded, no secret —
/// a public client has none, which is what PKCE is for.
pub fn exchange_request(pending: &Pending, code: &str) -> Value {
    json!({
        "method": "POST",
        "url": TOKEN_ENDPOINT,
        "headers": [{ "name": "Content-Type", "value": "application/x-www-form-urlencoded" }],
        "body": base64_std(form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", &pending.verifier),
            ("redirect_uri", &pending.redirect_uri),
            ("client_id", &pending.client_id),
            ("scope", SCOPE),
        ]).as_bytes()),
    })
}

/// The POST a refresh token is traded for a fresh access token with. The
/// scope is named again: Entra only returns the scopes the refresh carries.
pub fn refresh_request(client_id: &str, refresh_token: &str) -> Value {
    json!({
        "method": "POST",
        "url": TOKEN_ENDPOINT,
        "headers": [{ "name": "Content-Type", "value": "application/x-www-form-urlencoded" }],
        "body": base64_std(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("scope", SCOPE),
        ]).as_bytes()),
    })
}

/// The tokens a grant or a refresh produced, with `expires_in` seconds
/// already turned into the absolute `expires_at` the host stores.
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
/// joined with `&`.
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
            client_id: "cid-1".to_string(),
            redirect_uri: "http://localhost:8914/".to_string(),
            state: "s".to_string(),
            verifier: "v".to_string(),
            login_hint: Some("user@contoso.com".to_string()),
        };
        let url = authorize_url(&pending);
        assert!(url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8914%2F"));
        assert!(url.contains("response_mode=query"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope=offline_access%20Mail.ReadWrite%20Mail.Send"));
        assert!(url.contains("login_hint=user%40contoso.com"));
        assert!(url.contains("prompt=select_account"));
    }

    #[test]
    fn the_exchange_body_is_form_encoded_with_the_verifier() {
        let pending = Pending {
            client_id: "cid".to_string(),
            redirect_uri: "http://localhost:1/".to_string(),
            state: String::new(),
            verifier: "v-v".to_string(),
            login_hint: None,
        };
        let request = exchange_request(&pending, "c/de");
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(request["body"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(decoded.starts_with("grant_type=authorization_code&code=c%2Fde&"));
        assert!(decoded.contains("code_verifier=v-v"));
        assert!(decoded.contains("scope=offline_access%20Mail.ReadWrite%20Mail.Send"));
    }

    #[test]
    fn a_refused_grand_gives_its_own_words_back() {
        let error = parse_tokens(&json!({
            "error": "invalid_grant",
            "error_description": "AADSTS700082: the refresh token has expired"
        }))
        .unwrap_err();
        assert!(error.contains("AADSTS700082"));
    }
}
