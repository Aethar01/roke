use crate::config::{default_state_dir, normalize_device_tag, PeerConfig, PeerKind};
use crate::model::{civil_from_days, days_from_civil, now_millis};
use crate::sync::http::{base64_encode, build_agent, read_response_string};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_TASKS_SCOPE: &str = "https://www.googleapis.com/auth/tasks";

const MS_TASKS_SCOPE: &str = "Tasks.ReadWrite offline_access";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Epoch milliseconds when `access_token` expires.
    #[serde(default)]
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

pub fn token_file_path(state_dir: &Path, peer: &PeerConfig) -> PathBuf {
    let slug = normalize_device_tag(&peer.name);
    let safe = if slug.is_empty() {
        peer.kind.label()
    } else {
        &slug
    };
    state_dir.join(format!("oauth-{}-{safe}.json", peer.kind.label()))
}

pub fn has_saved_tokens(state_dir: &Path, peer: &PeerConfig) -> bool {
    let path = token_file_path(state_dir, peer);
    load_tokens(&path).is_some()
}

fn load_tokens(path: &Path) -> Option<OAuthTokens> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_tokens(path: &Path, tokens: &OAuthTokens) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed creating OAuth state dir: {e}"))?;
        }
    }
    let json = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, json).map_err(|e| format!("failed writing OAuth token file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|e| format!("failed saving OAuth token file: {e}"))
}

fn ms_endpoint(peer: &PeerConfig, suffix: &str) -> String {
    let tenant = peer
        .tenant
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("common");
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/{suffix}")
}

fn require_client_id(peer: &PeerConfig) -> Result<&str, String> {
    peer.client_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "missing 'client_id' for {} peer '{}'",
                peer.kind.label(),
                peer.name
            )
        })
}

/// Resolve a valid OAuth 2.0 Bearer access token for `peer`, automatically refreshing
/// expired tokens using the stored `refresh_token`.
pub fn resolve_access_token(peer: &PeerConfig, state_dir: Option<&Path>) -> Result<String, String> {
    if let Some(tok) = peer.resolve_token_cmd()? {
        return Ok(tok);
    }

    let Some(dir) = state_dir else {
        return Err(format!(
            "cannot locate OAuth token store for peer '{}' (provide token_cmd or a storage path)",
            peer.name
        ));
    };
    let primary_path = token_file_path(dir, peer);
    let global_dir = default_state_dir();
    let global_path = token_file_path(&global_dir, peer);
    let (path, tokens) = if let Some(toks) = load_tokens(&primary_path) {
        (primary_path, toks)
    } else if dir.starts_with(&global_dir) {
        if let Some(toks) = load_tokens(&global_path) {
            (global_path, toks)
        } else {
            return Err(format!("not authenticated — run `roke auth {}`", peer.name));
        }
    } else {
        return Err(format!("not authenticated — run `roke auth {}`", peer.name));
    };

    // Reuse access token if it has at least 60 seconds of lifetime remaining.
    let now = now_millis();
    if !tokens.access_token.is_empty()
        && (tokens.expires_at_ms == 0 || now.saturating_add(60_000) < tokens.expires_at_ms)
    {
        return Ok(tokens.access_token);
    }

    let Some(refresh) = tokens.refresh_token.as_deref().filter(|s| !s.is_empty()) else {
        return Err(format!(
            "OAuth token expired and no refresh_token stored — re-run `roke auth {}`",
            peer.name
        ));
    };

    let refreshed = refresh_access_token(peer, refresh)?;
    let merged = OAuthTokens {
        access_token: refreshed.access_token.clone(),
        refresh_token: refreshed.refresh_token.or(tokens.refresh_token),
        expires_at_ms: refreshed.expires_at_ms,
        scope: refreshed.scope.or(tokens.scope),
    };
    save_tokens(&path, &merged)?;
    Ok(merged.access_token)
}

fn refresh_access_token(peer: &PeerConfig, refresh_token: &str) -> Result<OAuthTokens, String> {
    let client_id = require_client_id(peer)?;
    let client_secret = peer.resolve_client_secret()?;
    let agent = build_agent();

    let (token_url, scope) = match peer.kind {
        PeerKind::GoogleTasks => (GOOGLE_TOKEN_URL.to_string(), None),
        PeerKind::MsTodo => (ms_endpoint(peer, "token"), Some(MS_TASKS_SCOPE)),
        other => return Err(format!("peer kind '{}' does not use OAuth", other.label())),
    };

    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];
    if let Some(sc) = scope {
        form.push(("scope", sc));
    }
    if let Some(ref secret) = client_secret {
        form.push(("client_secret", secret.as_str()));
    }

    let resp = agent
        .post(&token_url)
        .send_form(&form)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let body = read_response_string(r).unwrap_or_default();
                format!("OAuth token refresh failed (HTTP {code}): {body}")
            }
            ureq::Error::Transport(t) => format!("OAuth token refresh failed: {t}"),
        })?;
    let body = read_response_string(resp)?;
    parse_token_response(&body)
}

fn parse_token_response(body: &str) -> Result<OAuthTokens, String> {
    let val: Value =
        serde_json::from_str(body).map_err(|e| format!("invalid OAuth token JSON: {e}"))?;
    let access_token = val
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("OAuth response missing access_token: {body}"))?
        .to_string();
    let refresh_token = val
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let expires_in = val
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    let expires_at_ms = now_millis().saturating_add(expires_in.saturating_mul(1000));
    let scope = val.get("scope").and_then(Value::as_str).map(str::to_string);
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at_ms,
        scope,
    })
}

/// Interactive OAuth 2.0 login for `roke auth <peer>`.
/// - `ms-todo`: OAuth 2.0 Device Code flow (works locally and over headless SSH).
/// - `google-tasks`: OAuth 2.0 Loopback PKCE (`S256`) flow on `127.0.0.1`.
pub fn authenticate_peer(peer: &PeerConfig, state_dir: &Path) -> Result<PathBuf, String> {
    let tokens = match peer.kind {
        PeerKind::MsTodo => authenticate_ms_device_code(peer)?,
        PeerKind::GoogleTasks => authenticate_google_pkce(peer)?,
        other => {
            return Err(format!(
                "peer '{}' has kind '{}' which does not use OAuth login",
                peer.name,
                other.label()
            ));
        }
    };

    let path = token_file_path(state_dir, peer);
    save_tokens(&path, &tokens)?;
    Ok(path)
}

fn try_open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn authenticate_ms_device_code(peer: &PeerConfig) -> Result<OAuthTokens, String> {
    let client_id = require_client_id(peer)?;
    let agent = build_agent();

    let dc_url = ms_endpoint(peer, "devicecode");
    let resp = agent
        .post(&dc_url)
        .send_form(&[("client_id", client_id), ("scope", MS_TASKS_SCOPE)])
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let body = read_response_string(r).unwrap_or_default();
                format!("Microsoft device code request failed (HTTP {code}): {body}")
            }
            ureq::Error::Transport(t) => format!("Microsoft device code request failed: {t}"),
        })?;
    let body = read_response_string(resp)?;
    let val: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid device code JSON: {e}"))?;

    let device_code = val
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing device_code in response: {body}"))?;
    let user_code = val
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or("(see browser)");
    let verification_uri = val
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://microsoft.com/devicelogin");
    let interval_secs = val
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(2);
    let expires_in_secs = val.get("expires_in").and_then(Value::as_u64).unwrap_or(900);

    if let Some(msg) = val.get("message").and_then(Value::as_str) {
        eprintln!("{msg}");
    } else {
        eprintln!("Open {verification_uri} and enter code: {user_code}");
    }
    try_open_browser(verification_uri);

    let token_url = ms_endpoint(peer, "token");
    let deadline = now_millis().saturating_add(expires_in_secs.saturating_mul(1000));
    let mut poll_secs = interval_secs;

    while now_millis() < deadline {
        thread::sleep(Duration::from_secs(poll_secs));
        let poll_res = agent.post(&token_url).send_form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", client_id),
            ("device_code", device_code),
        ]);
        match poll_res {
            Ok(ok_resp) => {
                let tok_body = read_response_string(ok_resp)?;
                return parse_token_response(&tok_body);
            }
            Err(ureq::Error::Status(400, err_resp)) => {
                let err_body = read_response_string(err_resp).unwrap_or_default();
                let err_code = serde_json::from_str::<Value>(&err_body)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                match err_code.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        poll_secs = poll_secs.saturating_add(5);
                        continue;
                    }
                    "expired_token" => {
                        return Err("device code expired — run `roke auth` again".into())
                    }
                    "authorization_declined" => return Err("authorization declined by user".into()),
                    _ => return Err(format!("Microsoft OAuth error: {err_body}")),
                }
            }
            Err(ureq::Error::Status(code, err_resp)) => {
                let err_body = read_response_string(err_resp).unwrap_or_default();
                return Err(format!("Microsoft OAuth failed (HTTP {code}): {err_body}"));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(format!("Microsoft OAuth request failed: {t}"));
            }
        }
    }

    Err("timed out waiting for Microsoft authorization".into())
}

fn authenticate_google_pkce(peer: &PeerConfig) -> Result<OAuthTokens, String> {
    let client_id = require_client_id(peer)?;
    let client_secret = peer.resolve_client_secret()?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("failed binding loopback OAuth listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed reading loopback port: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let verifier = base64url_no_pad(&random_bytes(32));
    let challenge = base64url_no_pad(&sha256(verifier.as_bytes()));
    let state_token = base64url_no_pad(&random_bytes(16));

    let auth_url = format!(
        "{GOOGLE_AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256&state={}",
        url_encode(client_id),
        url_encode(&redirect_uri),
        url_encode(GOOGLE_TASKS_SCOPE),
        url_encode(&challenge),
        url_encode(&state_token),
    );

    eprintln!("Opening browser for Google Tasks login...");
    eprintln!("If it does not open automatically, visit:\n  {auth_url}");
    try_open_browser(&auth_url);

    let (mut stream, _) = listener
        .accept()
        .map_err(|e| format!("failed receiving OAuth callback: {e}"))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("failed cloning callback stream: {e}"))?,
    );
    let mut req_line = String::new();
    reader
        .read_line(&mut req_line)
        .map_err(|e| format!("failed reading OAuth callback: {e}"))?;

    let req_target = req_line.split_whitespace().nth(1).unwrap_or("");
    let query = req_target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query_params(query);

    let html_ok = "<html><body><h3>roke authenticated with Google Tasks</h3><p>You can close this tab and return to your terminal.</p></body></html>";
    let html_err = "<html><body><h3>roke authentication failed</h3><p>Check your terminal for details.</p></body></html>";

    if let Some(err) = params.get("error") {
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html_err.len(),
            html_err
        );
        return Err(format!("Google OAuth error: {err}"));
    }

    if params.get("state").map(String::as_str) != Some(state_token.as_str()) {
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html_err.len(),
            html_err
        );
        return Err("Google OAuth state mismatch".into());
    }

    let Some(code) = params.get("code").filter(|s| !s.is_empty()) else {
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html_err.len(),
            html_err
        );
        return Err("Google OAuth callback missing authorization code".into());
    };

    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html_ok.len(),
        html_ok
    );

    let agent = build_agent();
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id),
        ("redirect_uri", &redirect_uri),
        ("code_verifier", &verifier),
    ];
    if let Some(ref secret) = client_secret {
        form.push(("client_secret", secret.as_str()));
    }

    let resp = agent
        .post(GOOGLE_TOKEN_URL)
        .send_form(&form)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let body = read_response_string(r).unwrap_or_default();
                format!("Google OAuth token exchange failed (HTTP {code}): {body}")
            }
            ureq::Error::Transport(t) => format!("Google OAuth token exchange failed: {t}"),
        })?;
    let body = read_response_string(resp)?;
    parse_token_response(&body)
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    #[cfg(unix)]
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // Fallback mixing OS-seeded RandomState + timestamp + pid
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let state = RandomState::new();
        let mut h = state.build_hasher();
        h.write_u64(now_millis());
        h.write_u32(std::process::id());
        h.write_usize(i);
        let v = h.finish().to_le_bytes();
        for (dst, src) in chunk.iter_mut().zip(v.iter()) {
            *dst = *src;
        }
    }
    buf
}

pub fn base64url_no_pad(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

/// Pure-Rust RFC 6234 SHA-256 (used for OAuth 2.0 PKCE `S256` code challenge).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    #[allow(clippy::chunks_exact_to_as_chunks)]
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (dst, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *dst = dst.wrapping_add(add);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub fn url_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn parse_query_params(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(percent_decode_query(k), percent_decode_query(v));
        }
    }
    map
}

fn percent_decode_query(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Format epoch ms as RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS.sssZ`).
pub fn format_rfc3339_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let rem_ms = ms % 1000;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let h = sod / 3600;
    let min = (sod % 3600) / 60;
    let s = sod % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{rem_ms:03}Z")
}

/// Parse an RFC 3339 / ISO-8601 timestamp (`2026-11-27T10:15:30.123Z`, `2026-11-27T10:15:30+02:00`,
/// or `2026-11-27T10:15:30.0000000`) into epoch milliseconds.
pub fn parse_rfc3339_ms(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.len() < 10 {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let days = days_from_civil(y, m, d);

    if s.len() < 19 {
        return u64::try_from(days * 86_400).ok().map(|v| v * 1000);
    }
    let h: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;

    let mut rest = &s[19..];
    let mut millis: i64 = 0;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits_len = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        let frac = &after_dot[..digits_len];
        if !frac.is_empty() {
            let mut padded = frac.chars().take(3).collect::<String>();
            while padded.len() < 3 {
                padded.push('0');
            }
            millis = padded.parse().unwrap_or(0);
        }
        rest = &after_dot[digits_len..];
    }

    let mut tz_offset_secs: i64 = 0;
    if let Some(sign) = rest.bytes().next().filter(|b| matches!(b, b'+' | b'-')) {
        if rest.len() >= 6 {
            if let (Ok(oh), Ok(om)) = (rest[1..3].parse::<i64>(), rest[4..6].parse::<i64>()) {
                let mag = oh * 3600 + om * 60;
                tz_offset_secs = if sign == b'+' { mag } else { -mag };
            }
        }
    }

    let total_secs = days * 86_400 + h * 3600 + min * 60 + sec - tz_offset_secs;
    let total_ms = total_secs * 1000 + millis;
    u64::try_from(total_ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_rfc_6234_and_pkce_vector() {
        let digest = sha256(b"abc");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // RFC 7636 Appendix B PKCE S256 test vector
        let verifier = b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url_no_pad(&sha256(verifier)),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn rfc3339_round_trips_and_parses_offsets() {
        let ms = 1_764_238_530_456u64;
        let formatted = format_rfc3339_ms(ms);
        assert_eq!(parse_rfc3339_ms(&formatted), Some(ms));
        assert_eq!(
            parse_rfc3339_ms("2026-11-27T12:00:00+02:00"),
            parse_rfc3339_ms("2026-11-27T10:00:00Z")
        );
    }
}
