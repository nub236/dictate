//! ChatGPT OAuth: sign in, persist, refresh.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The RFC 7009 revocation endpoint on the same issuer as the other two.
const REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPES: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";

/// How long a sign-in waits for its callback before giving up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// The message a cancelled attempt returns, so the UI can stay quiet after a
/// sign-out instead of reporting the cancellation as a failure.
pub const CANCELLED_MESSAGE: &str = "Sign-in was cancelled.";

#[cfg(target_os = "windows")]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(target_os = "windows"))]
const NULL_DEVICE: &str = "/dev/null";

/// Bumped on every sign-out. A sign-in that was already under way compares this
/// before it saves, so approving an old browser tab cannot sign you back in.
static ATTEMPT: AtomicU64 = AtomicU64::new(0);

/// Set by sign-out to end a waiting attempt at once, freeing the callback port
/// instead of holding it until the timeout.
static CANCELLED: AtomicBool = AtomicBool::new(false);

fn current_attempt() -> u64 {
    ATTEMPT.load(Ordering::SeqCst)
}

/// Ends any sign-in that is waiting for its callback.
pub fn cancel_attempt() {
    ATTEMPT.fetch_add(1, Ordering::SeqCst);
    CANCELLED.store(true, Ordering::SeqCst);
    clear_pending_url();
}

/// The authorize URL of the sign-in that is waiting for its callback.
///
/// Closing the browser tab is a normal thing to do and nothing tells the app it
/// happened, so the URL is kept for the length of the attempt and can be opened
/// again. Clearing it is what makes the screen stop offering that.
static PENDING_URL: Mutex<Option<String>> = Mutex::new(None);

pub fn pending_url() -> Option<String> {
    PENDING_URL.lock().ok().and_then(|url| url.clone())
}

pub fn clear_pending_url() {
    if let Ok(mut url) = PENDING_URL.lock() {
        *url = None;
    }
}

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
    let mut out = String::with_capacity((data.len() * 4).div_ceil(3));
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

/// Random bytes from the operating system, hex-encoded.
///
/// This must not fall back to a fixed value when entropy is unavailable: the
/// PKCE verifier and the OAuth state both come from here, so a constant would
/// make the challenge guessable and the state check vacuous.
fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| format!("could not generate a secure random value: {error}"))?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
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
    // Write beside the file and rename over it. Writing in place truncates
    // first, and a crash between those two steps leaves an auth.json that no
    // longer parses — which reads to the app as "signed out".
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, json).map_err(|e| format!("could not save the login: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temporary, path).map_err(|e| format!("could not save the login: {e}"))?;
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
    // curl only fails for transport problems. Without this, a machine with no
    // network turned into "ChatGPT rejected the sign-in:" with nothing after it.
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "Could not reach ChatGPT. Check your connection and try again.".to_string()
        } else {
            format!("Could not reach ChatGPT: {detail}")
        });
    }
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
                .map(str::trim)
                .filter(|detail| !detail.is_empty())
                .unwrap_or_else(|| text.trim());
            if detail.is_empty() {
                "ChatGPT rejected the sign-in, without saying why.".to_string()
            } else {
                format!("ChatGPT rejected the sign-in: {detail}")
            }
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
    CANCELLED.store(false, Ordering::SeqCst);
    let attempt = current_attempt();

    let verifier = random_hex(48)?;
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = random_hex(16)?;

    let listeners = bind_listeners()?;

    let query = format!(
        "response_type=code&client_id={CLIENT_ID}&\
         redirect_uri=http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}&scope={scope}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}\
         &id_token_add_organizations=true&codex_cli_simplified_flow=true",
        scope = SCOPES.replace(' ', "%20"),
    );
    let url = format!("{AUTH_URL}?{query}");
    if let Ok(mut pending) = PENDING_URL.lock() {
        *pending = Some(url.clone());
    }
    let _ = open::that(&url);

    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let mut code: Option<String> = None;
    let mut failure: Option<String> = None;

    'wait: while Instant::now() < deadline {
        // Signing out ends the wait immediately and releases the port, rather
        // than leaving a dead attempt holding it for the rest of the timeout.
        if CANCELLED.swap(false, Ordering::SeqCst) {
            clear_pending_url();
            return Err(CANCELLED_MESSAGE.into());
        }
        let mut activity = false;
        for listener in &listeners {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => {
                    activity = true;
                    stream
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(format!("sign-in listener failed: {error}")),
            };
            match read_callback(&mut stream, &state) {
                Callback::Ignored => {}
                Callback::Arrived {
                    code: found,
                    failure: reason,
                } => {
                    code = found;
                    failure = reason;
                    break 'wait;
                }
            }
        }
        if !activity {
            std::thread::sleep(Duration::from_millis(120));
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
    // Signing out while the exchange was in flight wins: the tokens are dropped
    // instead of being written back after the user asked to be signed out.
    if current_attempt() != attempt {
        return Err(CANCELLED_MESSAGE.into());
    }
    save(path, &credentials)?;
    Ok(credentials)
}

/// Asks the issuer to drop the refresh token.
///
/// Best effort on purpose: the tokens are already gone from disk, and a server
/// that does not implement revocation is not something the user could act on.
pub fn revoke(credentials: Option<&Credentials>) {
    let Some(credentials) = credentials else {
        return;
    };
    let token = if credentials.refresh_token.is_empty() {
        &credentials.access_token
    } else {
        &credentials.refresh_token
    };
    if token.is_empty() {
        return;
    }
    let body = serde_json::json!({
        "token": token,
        "token_type_hint": "refresh_token",
        "client_id": CLIENT_ID,
    })
    .to_string();
    let mut curl = Command::new("curl");
    curl.args([
        "-sS",
        "--max-time",
        "10",
        "--http1.1",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-o",
        NULL_DEVICE,
        "-w",
        "%{http_code}",
        "-d",
        "@-",
        REVOKE_URL,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    crate::engine::hide_console(&mut curl);
    let Ok(mut child) = curl.spawn() else {
        crate::debug_log("sign-out: could not start the revocation request");
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
    }
    match child.wait_with_output() {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
            crate::debug_log(&format!("sign-out: revocation answered {status}"));
        }
        Err(error) => crate::debug_log(&format!("sign-out: revocation failed ({error})")),
    }
}

/// Listens on both loopback families.
///
/// The redirect asks for `http://localhost:...`. On a machine where that name
/// resolves to IPv6 first the browser connects to ::1 and does not fall back,
/// so listening only on 127.0.0.1 leaves the tab on a dead page and the sign-in
/// waiting out the whole timeout.
fn bind_listeners() -> Result<Vec<TcpListener>, String> {
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).map_err(|_| {
        format!("Port {CALLBACK_PORT} is busy. Close any other sign-in window and try again.")
    })?;
    let mut listeners = vec![listener];
    // No IPv6 is not an error: the IPv4 listener still serves localhost there.
    if let Ok(listener) = TcpListener::bind(("::1", CALLBACK_PORT)) {
        listeners.push(listener);
    }
    for listener in &listeners {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    }
    Ok(listeners)
}

/// What one request to the callback port means.
enum Callback {
    /// Carries no result: a favicon fetch, a prefetch, or another app probing
    /// the port. Answering it must not end the wait.
    Ignored,
    Arrived {
        code: Option<String>,
        failure: Option<String>,
    },
}

/// Splits a request target into either a result or something to ignore.
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Some((path, query)) = target.split_once('?') else {
        return Callback::Ignored;
    };
    if path != CALLBACK_PATH {
        return Callback::Ignored;
    }
    let (mut code, mut error, mut state) = (None, None, None);
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" => code = Some(percent_decode(value)),
            "error" => error = Some(percent_decode(value)),
            "state" => state = Some(percent_decode(value)),
            _ => {}
        }
    }
    if code.is_none() && error.is_none() {
        return Callback::Ignored;
    }
    // A result that echoes a different state did not come from the approval
    // this app started. A missing one is left alone: the PKCE verifier already
    // ties the exchange to this process, and refusing a callback the server
    // chose not to label would break sign-in over nothing.
    if state.is_some_and(|state| state != expected_state) {
        return Callback::Arrived {
            code: None,
            failure: Some("state mismatch".into()),
        };
    }
    Callback::Arrived { code, failure: error }
}

/// Reads one request, answers it, and reports what it meant.
fn read_callback(stream: &mut TcpStream, expected_state: &str) -> Callback {
    // A browser can open the connection before it sends the request (a
    // preconnect), and a connection that never speaks must not hold up the
    // wait. Blocking with a bound is what makes the read return either the
    // request or nothing at all.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buffer = [0u8; 4096];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .unwrap_or("")
        .split(' ')
        .nth(1)
        .unwrap_or("");
    let outcome = parse_callback(target, expected_state);

    let (status, body) = match &outcome {
        Callback::Ignored => ("404 Not Found", ""),
        Callback::Arrived { failure: Some(_), .. } => (
            "400 Bad Request",
            "<html><body style=\"font-family:-apple-system,system-ui;padding:16vh 8vw;text-align:center;background:#0b0b0d;color:#f2f2f5\"><h2>Sign-in failed</h2><p style=\"color:#8d8d99\">Go back to Dictate and try again.</p></body></html>",
        ),
        Callback::Arrived { .. } => (
            "200 OK",
            "<html><body style=\"font-family:-apple-system,system-ui;padding:16vh 8vw;text-align:center;background:#0b0b0d;color:#f2f2f5\"><h2>Signed in to Dictate</h2><p style=\"color:#8d8d99\">You can close this tab and go back to the app.</p></body></html>",
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Answers one request the way the accept loop does, on a real socket.
    fn answer_one(request: &'static [u8], state: &'static str) -> (String, Callback) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
        let address = listener.local_addr().expect("an address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("a connection");
            read_callback(&mut stream, state)
        });
        let mut client = TcpStream::connect(address).expect("connect");
        client.write_all(request).expect("write");
        // read_callback answers and returns, which closes the stream; reading to
        // the end is what waits for that answer to arrive.
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read");
        let outcome = server.join().expect("the server thread");
        (response, outcome)
    }

    fn token(claims: &str) -> String {
        format!("{}.{}.signature", b64url(b"{\"alg\":\"none\"}"), b64url(claims.as_bytes()))
    }

    #[test]
    fn a_callback_is_answered_and_reported_off_a_real_socket() {
        let (response, outcome) = answer_one(
            b"GET /auth/callback?code=abc123&state=s1 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n",
            "s1",
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Signed in to Dictate"), "{response}");
        assert!(matches!(
            outcome,
            Callback::Arrived { code: Some(code), failure: None } if code == "abc123"
        ));
    }

    #[test]
    fn a_probe_gets_a_404_and_does_not_end_the_wait() {
        // This is the request that used to break the sign-in before the code had
        // arrived: the browser asks for a favicon on the callback page.
        let (response, outcome) = answer_one(b"GET /favicon.ico HTTP/1.1\r\n\r\n", "s1");
        assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
        assert!(matches!(outcome, Callback::Ignored));
    }

    #[test]
    fn a_mismatched_state_is_answered_with_a_failure() {
        let (response, outcome) = answer_one(
            b"GET /auth/callback?code=abc&state=someone-else HTTP/1.1\r\n\r\n",
            "s1",
        );
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"), "{response}");
        assert!(matches!(outcome, Callback::Arrived { failure: Some(_), .. }));
    }

    #[test]
    fn the_ipv6_loopback_can_accept_a_callback() {
        // The redirect names `localhost`, and where that resolves to ::1 first
        // the browser never tries 127.0.0.1. This checks the second listener is
        // possible on this machine; the code binds it whenever it is.
        let Ok(listener) = TcpListener::bind(("::1", 0)) else {
            return;
        };
        let address = listener.local_addr().expect("an address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("a connection");
            read_callback(&mut stream, "s1")
        });
        let mut client = TcpStream::connect(address).expect("connect over ::1");
        client
            .write_all(b"GET /auth/callback?code=abc&state=s1 HTTP/1.1\r\n\r\n")
            .expect("write");
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(matches!(
            server.join().expect("the server thread"),
            Callback::Arrived { code: Some(_), .. }
        ));
    }

    #[test]
    fn a_callback_carries_the_code() {
        assert!(matches!(
            parse_callback("/auth/callback?code=abc123&state=s1", "s1"),
            Callback::Arrived { code: Some(code), failure: None } if code == "abc123"
        ));
    }

    #[test]
    fn a_request_without_a_result_does_not_end_the_wait() {
        // The browser asks for a favicon, and prefetchers fetch the redirect.
        // Under the old code both of those ended the sign-in before the code
        // had arrived.
        assert!(matches!(parse_callback("/favicon.ico", "s1"), Callback::Ignored));
        assert!(matches!(
            parse_callback("/auth/callback", "s1"),
            Callback::Ignored
        ));
        assert!(matches!(
            parse_callback("/auth/callback?state=s1", "s1"),
            Callback::Ignored
        ));
    }

    #[test]
    fn a_callback_from_another_attempt_fails() {
        assert!(matches!(
            parse_callback("/auth/callback?code=abc&state=other", "s1"),
            Callback::Arrived { failure: Some(_), .. }
        ));
        // A callback that does not carry a state at all is still exchanged: the
        // PKCE verifier is what actually binds the code to this process.
        assert!(matches!(
            parse_callback("/auth/callback?code=abc", "s1"),
            Callback::Arrived { code: Some(code), failure: None } if code == "abc"
        ));
    }

    #[test]
    fn a_refusal_is_reported_with_its_state_intact() {
        match parse_callback("/auth/callback?error=access_denied&state=s1", "s1") {
            Callback::Arrived { code: None, failure: Some(reason) } => {
                assert_eq!(reason, "access_denied")
            }
            _ => panic!("expected a failure"),
        }
    }

    #[test]
    fn state_is_percent_decoded_before_comparison() {
        assert!(matches!(
            parse_callback("/auth/callback?code=a%2Bb&state=s%31", "s1"),
            Callback::Arrived { code: Some(code), failure: None } if code == "a+b"
        ));
    }

    #[test]
    fn claims_are_read_from_an_id_token() {
        let token = token(r#"{"email":"someone@example.com"}"#);
        assert_eq!(
            jwt_claims(&token).get("email").and_then(Value::as_str),
            Some("someone@example.com")
        );
    }

    #[test]
    fn the_account_id_comes_from_the_auth_namespace() {
        let id = token(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_1"}}"#);
        assert_eq!(account_id_from(&id, ""), Some("acct_1".to_string()));
        // The access token is consulted when the id token has nothing to say.
        assert_eq!(
            account_id_from("", &token(r#"{"chatgpt_account_id":"acct_2"}"#)),
            Some("acct_2".to_string())
        );
        assert_eq!(account_id_from("", ""), None);
    }

    #[test]
    fn a_token_without_claims_does_not_panic() {
        assert!(jwt_claims("not-a-jwt").is_null());
        assert!(jwt_claims("").is_null());
    }
}
