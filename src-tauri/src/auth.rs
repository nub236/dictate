//! ChatGPT OAuth: sign in, persist, refresh.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPES: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Credentials {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub expires_at: i64,
}

impl Credentials {
    pub fn email(&self) -> Option<String> {
        let claims = jwt_claims(&self.id_token);
        claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.expires_at - 60 <= now
    }
}

// --------------------------------------------------------------------------- //
// small encoding helpers (no extra crates)
// --------------------------------------------------------------------------- //

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 4 + 2) / 3);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64[n as usize & 63] as char);
        }
    }
    out
}

fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut count = 0;
    let mut out = Vec::new();
    for ch in text.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => continue,
        } as u32;
        bits = (bits << 6) | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

fn jwt_claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap_or("");
    b64url_decode(payload)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn account_id_from(id_token: &str, access_token: &str) -> Option<String> {
    for token in [id_token, access_token] {
        let claims = jwt_claims(token);
        if let Some(id) = claims
            .get("https://api.openai.com/auth")
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(Value::as_str)
        {
            return Some(id.to_string());
        }
        if let Some(id) = claims.get("chatgpt_account_id").and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    None
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut buffer);
    }
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// --------------------------------------------------------------------------- //
// storage
// --------------------------------------------------------------------------- //

pub fn load(path: &Path) -> Option<Credentials> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save(path: &Path, credentials: &Credentials) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(credentials).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| format!("could not save the login: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// token endpoint
// --------------------------------------------------------------------------- //

fn token_request(body: Value) -> Result<Credentials, String> {
    let payload = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let mut curl = Command::new("curl");
    curl.args([
        "-sS",
        "--max-time",
        "45",
        "--http1.1",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-H",
        "Accept: application/json",
        "-d",
        "@-",
        TOKEN_URL,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    crate::engine::hide_console(&mut curl);
    let mut child = curl
        .spawn()
        .map_err(|e| format!("could not reach the network: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("sign-in failed: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            let detail = value
                .get("error_description")
                .and_then(Value::as_str)
                .or_else(|| value.get("error").and_then(Value::as_str))
                .unwrap_or_else(|| text.trim());
            format!("ChatGPT rejected the sign-in: {detail}")
        })?;
    let id_token = value
        .get("id_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok(Credentials {
        account_id: account_id_from(&id_token, access),
        access_token: access.to_string(),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        id_token,
        expires_at: now + expires_in,
    })
}

pub fn refresh(path: &Path, current: &Credentials) -> Result<Credentials, String> {
    if current.refresh_token.is_empty() {
        return Err("Your ChatGPT session expired. Sign in again.".into());
    }
    let mut next = token_request(serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": current.refresh_token,
        "client_id": CLIENT_ID,
        "scope": SCOPES,
    }))?;
    if next.refresh_token.is_empty() {
        next.refresh_token = current.refresh_token.clone();
    }
    if next.id_token.is_empty() {
        next.id_token = current.id_token.clone();
    }
    if next.account_id.is_none() {
        next.account_id = current.account_id.clone();
    }
    save(path, &next)?;
    Ok(next)
}

/// Returns credentials, refreshing them first when needed.
pub fn valid(path: &Path) -> Result<Credentials, String> {
    let credentials = load(path).ok_or("Sign in with ChatGPT to start dictating.")?;
    if credentials.expired() {
        refresh(path, &credentials)
    } else {
        Ok(credentials)
    }
}

// --------------------------------------------------------------------------- //
// interactive login
// --------------------------------------------------------------------------- //

pub fn login(path: &Path) -> Result<Credentials, String> {
    let verifier = random_hex(48);
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = random_hex(16);

    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).map_err(|_| {
        format!("Port {CALLBACK_PORT} is busy. Close any other sign-in window and try again.")
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;

    let query = format!(
        "response_type=code&client_id={CLIENT_ID}&redirect_uri={redirect}&scope={scope}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}\
         &id_token_add_organizations=true&codex_cli_simplified_flow=true",
        redirect = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"),
        scope = SCOPES.replace(' ', "%20"),
    );
    let url = format!("{AUTH_URL}?{query}");
    let _ = open::that(&url);

    let deadline = Instant::now() + Duration::from_secs(300);
    let mut code: Option<String> = None;
    let mut failure: Option<String> = None;

    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let first = request.lines().next().unwrap_or("");

                if first.contains(CALLBACK_PATH) {
                    let target = first.split(' ').nth(1).unwrap_or("");
                    if let Some((_, query)) = target.split_once('?') {
                        for pair in query.split('&') {
                            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                            match key {
                                "code" => code = Some(percent_decode(value)),
                                "error" => failure = Some(percent_decode(value)),
                                "state" if percent_decode(value) != state => {
                                    failure = Some("state mismatch".into())
                                }
                                _ => {}
                            }
                        }
                    }
                }

                let body = "<html><body style=\"font-family:-apple-system,system-ui;padding:16vh 8vw;text-align:center;background:#0b0b0d;color:#f2f2f5\"><h2>Signed in to Dictate</h2><p style=\"color:#8d8d99\">You can close this tab and go back to the app.</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                break;
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(error) => return Err(format!("sign-in listener failed: {error}")),
        }
    }

    if let Some(message) = failure {
        return Err(format!("Sign-in failed: {message}"));
    }
    let code = code.ok_or("Sign-in timed out. Try again.")?;

    let credentials = token_request(serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"),
        "client_id": CLIENT_ID,
        "code_verifier": verifier,
    }))?;
    save(path, &credentials)?;
    Ok(credentials)
}
