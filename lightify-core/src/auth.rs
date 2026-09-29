//! Spotify sign-in (OAuth 2.0 authorization code + PKCE), ported from the shipped
//! host's `auth.rs` + `start_auth` so both apps write the same config and token cache.
//!
//! Before this existed the native shell could only *reuse* a login the shipped app
//! had cached — a fresh install with no `.lightify_cache` just said "Open Lightify to
//! sign in" and stopped, with nothing to click.
//!
//! Flow: the user supplies their own Client ID (Lightify runs as each user's own
//! Spotify developer app) → [`bind_callback`] claims the redirect port → the caller
//! opens [`PkceAuth::auth_url`] in the browser → [`PkceAuth::finish`] waits for the
//! redirect, exchanges the code, checks the granted scopes, resolves the account's
//! identity and saves config + token.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::config::{self, TokenInfo};

/// Must match the redirect URI registered on the user's Spotify developer app —
/// the same one the shipped app (and the setup notes) tell them to register.
pub const REDIRECT_URI: &str = "http://127.0.0.1:8901/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:8901";
const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
pub const DASHBOARD_URL: &str = "https://developer.spotify.com/dashboard";

/// The scopes the shipped app requests (identical list, so a token from either app
/// serves both).
pub const SCOPES: &str = "\
user-read-playback-state \
user-modify-playback-state \
user-read-currently-playing \
user-read-recently-played \
user-library-read \
user-library-modify \
user-read-private \
user-read-email \
playlist-read-private \
playlist-read-collaborative \
playlist-modify-public \
playlist-modify-private \
streaming";

/// A Spotify Client ID is 32 hex characters. Checked up front so a paste with a
/// stray space or a truncated value fails here, not as an opaque browser error.
pub fn normalize_client_id(raw: &str) -> Result<String, String> {
    let id = raw.trim().to_ascii_lowercase();
    if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err("That doesn't look like a Client ID — it's the 32-character code on your Spotify app's settings page.".into())
    }
}

/// The saved Client ID, if any, to pre-fill the sign-in field.
pub fn saved_client_id() -> String {
    config::load_config(&config::data_dir()).client_id
}

pub struct PkceAuth {
    client_id: String,
    verifier: String,
    challenge: String,
}

/// The claimed redirect port. Bound *before* the browser opens, so a fast redirect
/// can never arrive ahead of the listener.
pub struct Callback(TcpListener);

pub async fn bind_callback() -> Result<Callback, String> {
    TcpListener::bind(CALLBACK_ADDR).await.map(Callback).map_err(|e| {
        format!("Couldn't open the sign-in port ({CALLBACK_ADDR}): {e}. Is another Lightify signing in right now?")
    })
}

impl PkceAuth {
    pub fn new(client_id: &str) -> Self {
        let bytes: Vec<u8> = (0..64).map(|_| rand::thread_rng().gen::<u8>()).collect();
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { client_id: client_id.to_string(), verifier, challenge }
    }

    pub fn auth_url(&self) -> String {
        format!(
            "{AUTH_URL}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge_method=S256&code_challenge={}",
            enc(&self.client_id),
            enc(REDIRECT_URI),
            enc(SCOPES),
            enc(&self.challenge),
        )
    }

    /// Wait for the browser redirect, then exchange, verify and save. Returns the
    /// account's display name.
    pub async fn finish(&self, callback: Callback) -> Result<String, String> {
        let code = wait_for_code(callback.0).await?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        let resp = http
            .post(TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(|e| format!("Couldn't reach Spotify to finish signing in: {e}"))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Spotify refused the sign-in: {body}"));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("Sign-in response: {e}"))?;
        let mut token = parse_token(&body)?;
        if !has_required_scopes(&token.scope) {
            return Err("Spotify didn't grant every permission Lightify needs. Sign in again and approve all of them.".into());
        }

        // Identity: the shell shows "Connected as …", and creating playlists needs
        // the user id. A 403 here is the classic dev-mode trap — the account isn't
        // on the app's User Management list — so say that plainly.
        let me = http
            .get("https://api.spotify.com/v1/me")
            .bearer_auth(&token.access_token)
            .send()
            .await
            .map_err(|e| format!("Couldn't read your Spotify profile: {e}"))?;
        if me.status().as_u16() == 403 {
            return Err("Spotify blocked this account for your app. On developer.spotify.com, open your app → User Management and add the email of the Spotify account you're signing in with.".into());
        }
        if !me.status().is_success() {
            return Err(format!("Couldn't read your Spotify profile ({})", me.status()));
        }
        let me: serde_json::Value = me.json().await.map_err(|e| format!("Profile response: {e}"))?;
        token.cached_user_id = me["id"].as_str().unwrap_or_default().to_string();
        token.cached_display_name = me["display_name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| me["id"].as_str())
            .unwrap_or("Spotify")
            .to_string();

        let dir = config::data_dir();
        let cid = self.client_id.clone();
        config::update_config(&dir, |c| c.client_id = cid)?;
        config::save_token(&dir, &token);
        Ok(token.cached_display_name.clone())
    }
}

fn has_required_scopes(granted: &str) -> bool {
    let granted: std::collections::HashSet<&str> = granted.split_whitespace().collect();
    SCOPES.split_whitespace().all(|s| granted.contains(s))
}

fn parse_token(body: &serde_json::Value) -> Result<TokenInfo, String> {
    let access_token = body["access_token"].as_str().ok_or("Sign-in response had no access token")?.to_string();
    let refresh_token = body["refresh_token"].as_str().ok_or("Sign-in response had no refresh token")?.to_string();
    let expires_in = body["expires_in"].as_i64().unwrap_or(3600);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(TokenInfo {
        access_token,
        token_type: body["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_in,
        refresh_token,
        scope: body["scope"].as_str().unwrap_or_default().to_string(),
        expires_at: now + expires_in,
        cached_user_id: String::new(),
        cached_display_name: String::new(),
    })
}

/// Serve the redirect until it carries a `code` (or an `error`). Other requests —
/// a browser's favicon probe, say — get a 404 and the wait continues. No timeout
/// here: the caller races this against the user cancelling.
async fn wait_for_code(listener: TcpListener) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| format!("Sign-in callback: {e}"))?;
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            continue;
        }
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => {}
            }
        }
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != "/callback" {
            let _ = respond(&mut stream, "404 Not Found", "Not found").await;
            continue;
        }
        if let Some(code) = query_param(query, "code") {
            let _ = respond(&mut stream, "200 OK", &page("You're signed in", "You can close this tab and go back to Lightify.")).await;
            return Ok(code);
        }
        let error = query_param(query, "error").unwrap_or_else(|| "no code returned".into());
        let _ = respond(&mut stream, "400 Bad Request", &page("Sign-in didn't finish", &format!("Spotify said: {error}. Go back to Lightify and try again."))).await;
        return Err(if error == "access_denied" {
            "Sign-in was cancelled in the browser.".into()
        } else {
            format!("Spotify sign-in failed: {error}")
        });
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await
}

fn page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Lightify</title>\
         <body style=\"margin:0;min-height:100vh;display:grid;place-items:center;background:#111;color:#e2e2e2;font:15px 'Segoe UI',system-ui,sans-serif\">\
         <div style=\"text-align:center\"><p style=\"letter-spacing:4px;color:#888;font-size:12px\">L I G H T I F Y</p>\
         <h1 style=\"font-weight:500;font-size:22px;margin:8px 0\">{title}</h1><p style=\"color:#a0a0a0\">{message}</p></div>"
    )
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| decode(v))
    })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn enc(s: &str) -> String {
    crate::spotify::urlencode(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_is_validated() {
        assert!(normalize_client_id(" 0123456789ABCDEF0123456789abcdef ").is_ok());
        assert!(normalize_client_id("abc").is_err());
        assert!(normalize_client_id("0123456789abcdef0123456789abcdeg").is_err());
    }

    #[test]
    fn callback_query_is_parsed() {
        assert_eq!(query_param("code=AQB-x_1&state=z", "code").as_deref(), Some("AQB-x_1"));
        assert_eq!(query_param("error=access_denied", "error").as_deref(), Some("access_denied"));
        assert_eq!(decode("a%20b+c%2"), "a b c%2");
    }

    /// The browser coming back with `error=access_denied` (the user clicked Cancel on
    /// Spotify's consent page) must end the wait with a clear message, after a stray
    /// request (a favicon probe) was answered and ignored.
    #[tokio::test]
    async fn callback_listener_handles_probe_then_denial() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(wait_for_code(listener));
        for (req, want) in [
            ("GET /favicon.ico HTTP/1.1
Host: x

", "404"),
            ("GET /callback?error=access_denied HTTP/1.1
Host: x

", "400"),
        ] {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            assert!(out.starts_with(&format!("HTTP/1.1 {want}")), "{out}");
        }
        let err = waiter.await.unwrap().unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
    }

    #[tokio::test]
    async fn callback_listener_returns_the_code() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(wait_for_code(listener));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /callback?code=AQB%2Dx HTTP/1.1
Host: x

").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("signed in"), "{out}");
        assert_eq!(waiter.await.unwrap().unwrap(), "AQB-x");
    }

    #[test]
    fn pkce_challenge_matches_verifier() {
        let p = PkceAuth::new("0123456789abcdef0123456789abcdef");
        assert_eq!(p.challenge, URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes())));
        assert!(p.auth_url().contains("code_challenge_method=S256"));
        assert!(p.auth_url().contains(&enc(REDIRECT_URI)));
    }
}
