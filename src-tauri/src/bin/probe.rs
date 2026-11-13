//! Asks one question of the signed-in account: can this plan use the
//! enhancement route, and with which models?
//!
//! Build and run with:
//!
//! ```text
//! cd src-tauri
//! cargo run --features probe --bin probe -- [model-slug]
//! ```
//!
//! It reads the same `auth.json` the app writes. If the stored token has
//! expired it refreshes it and **saves the new one**, because the token endpoint
//! rotates refresh tokens: discarding the new one leaves the app unable to sign
//! in without a fresh browser approval.
//!
//! It sends one short transcript and prints the model list, the status, the time
//! taken and the rewritten text.

#[path = "../console.rs"]
mod console;
#[path = "../enhance.rs"]
mod enhance;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The same token endpoint and client the app signs in with, so an expired
/// token can be renewed here instead of requiring a dictation first.
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPES: &str = "openid profile email offline_access";

/// A sample that a cleanup pass has something to do with: fillers, a
/// self-correction and no punctuation.
const SAMPLE: &str = "so um i think we should uh move the meeting to thursday no wait friday at two";

fn instructions() -> String {
    enhance::compose_instructions(&enhance::Prompt {
        mode: enhance::MODE_MESSAGING,
        tone: enhance::TONE_CASUAL,
        level: enhance::LEVEL_BALANCED,
        context: Some("Slack"),
        vocabulary: &["Freebuff".to_string()],
        custom: "",
    })
}

/// Where the app keeps its files, mirroring `data_dir` in `main.rs`.
fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"))
        .unwrap_or_else(|_| PathBuf::from("."));
    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("dev.dictate")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Exchanges the stored refresh token for a fresh one.
///
/// The whole token response is returned because the endpoint rotates the refresh
/// token: keeping only the access token would strand the saved credentials.
fn refresh(stored: &str) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": stored,
        "client_id": CLIENT_ID,
        "scope": SCOPES,
    })
    .to_string();

    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "--http1.1",
        "--max-time",
        "45",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-H",
        "Accept: application/json",
        "-d",
        "@-",
        TOKEN_URL,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    console::hide_console(&mut command);

    let mut child = command.spawn().map_err(|error| error.to_string())?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(body.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
        format!(
            "the token endpoint did not answer with JSON: {}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    if value.get("access_token").and_then(|t| t.as_str()).is_none() {
        let detail = value
            .get("error_description")
            .and_then(|detail| detail.as_str())
            .or_else(|| value.get("error").and_then(|error| error.as_str()))
            .unwrap_or("no access token in the response");
        return Err(detail.to_string());
    }
    Ok(value)
}

/// Writes the credentials back the way the app does: beside the file, then
/// renamed over it, with owner-only permissions.
fn persist(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn main() {
    let wanted = std::env::args().nth(1);

    let path = data_dir().join("auth.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("Could not read {}: {error}", path.display());
            return;
        }
    };
    let mut stored: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("auth.json did not parse: {error}");
            return;
        }
    };
    let Some(mut access_token) = stored
        .get("access_token")
        .and_then(|token| token.as_str())
        .map(str::to_string)
    else {
        eprintln!("auth.json carries no access_token; sign in from the app first.");
        return;
    };
    let account_id = stored
        .get("account_id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let expires_at = stored
        .get("expires_at")
        .and_then(|at| at.as_i64())
        .unwrap_or(0);

    println!("auth.json: {}", path.display());
    println!(
        "token: {} chars, account id: {}, {}",
        access_token.chars().count(),
        account_id.as_deref().unwrap_or("(none)"),
        if expires_at > now() + 60 {
            "not expired".to_string()
        } else {
            format!("EXPIRED {} min ago", (now() - expires_at).max(0) / 60)
        }
    );

    if expires_at <= now() + 60 {
        let refresh_token = stored
            .get("refresh_token")
            .and_then(|token| token.as_str())
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        match refresh_token.map(|token| refresh(&token)) {
            Some(Ok(fresh)) => {
                access_token = fresh
                    .get("access_token")
                    .and_then(|token| token.as_str())
                    .unwrap_or("")
                    .to_string();
                // Save before anything else can fail, so the rotated refresh
                // token is never the one that gets thrown away.
                if let Some(map) = stored.as_object_mut() {
                    for field in ["access_token", "refresh_token", "id_token"] {
                        if let Some(value) = fresh.get(field).and_then(|value| value.as_str()) {
                            map.insert(field.to_string(), value.into());
                        }
                    }
                    let expires_in = fresh
                        .get("expires_in")
                        .and_then(|value| value.as_i64())
                        .unwrap_or(3600);
                    map.insert("expires_at".into(), (now() + expires_in).into());
                }
                match persist(&path, &stored) {
                    Ok(()) => println!("refreshed and saved"),
                    Err(error) => println!("refreshed for this run, but could not save: {error}"),
                }
            }
            Some(Err(error)) => {
                println!("refresh failed: {error}");
                println!("Sign out and back in from Dictate, then run this again.\n");
            }
            None => println!("auth.json carries no refresh token; sign in from the app first.\n"),
        }
    }

    let account = enhance::Account {
        access_token: &access_token,
        account_id: account_id.as_deref(),
    };

    println!("\n== models ==");
    let models = match enhance::list_models(&account) {
        Ok(models) => {
            for model in &models {
                println!("  {} — {}", model.slug, model.display_name);
            }
            models
        }
        Err(failure) => {
            println!("  failed ({}): {}", failure.status, failure.message);
            Vec::new()
        }
    };

    // Prefer the cheapest model the account offers, then whatever came first.
    let model = wanted
        .clone()
        .or_else(|| {
            models
                .iter()
                .find(|model| model.slug.contains("luna"))
                .map(|model| model.slug.clone())
        })
        .or_else(|| models.first().map(|model| model.slug.clone()))
        .unwrap_or_else(|| enhance::DEFAULT_MODEL.to_string());

    println!("\n== rewrite ==");
    println!("model: {model}");
    println!("instructions: {}", instructions());
    println!("input: {SAMPLE}");

    let started = std::time::Instant::now();
    match enhance::enhance(&account, &model, &instructions(), SAMPLE) {
        Ok(text) => {
            println!("\nok in {:.2}s", started.elapsed().as_secs_f64());
            println!("output: {text}");
        }
        Err(failure) => {
            println!(
                "\nfailed in {:.2}s ({}): {}",
                started.elapsed().as_secs_f64(),
                failure.status,
                failure.message
            );
            println!(
                "\nIf the model was refused, pass another slug from the list above:\n\
                 cargo run --features probe --bin probe -- <slug>"
            );
        }
    }
}
