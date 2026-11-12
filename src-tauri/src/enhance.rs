//! Rewrites a finished transcript with the user's ChatGPT account.
//!
//! The request goes to the Codex responses route, the same one the dictation
//! path already borrows, so it works on the plan the user is signed in with
//! (including Free) and needs no second sign-in. `store` must be `false` and
//! `stream` must be `true`: the route refuses anything else, and `store: false`
//! is also what keeps the request out of the user's chat history.
//!
//! Nothing here may cost the user their dictation. Every failure is reported as
//! a `Failure` and the caller pastes the raw transcript instead.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The Codex inference route. `store: false` means no conversation is created.
pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// The cleanup pass a fresh install starts with.
pub const DEFAULT_INSTRUCTIONS: &str = "You clean up dictated text. Remove filler words and \
resolve self-corrections. Keep the speaker's words and meaning. Fix punctuation and \
capitalization. Reply with the cleaned text only, with no commentary or quotation marks.";

/// The model to rewrite with when discovery returns nothing.
///
/// A free plan's model list comes back empty even though this model answers, so
/// the working default cannot depend on discovery.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// The models this account may use, so no model name has to be hardcoded.
///
/// The route refuses a request that does not name the client version, so the
/// version the request already announces is repeated as a query parameter.
pub fn models_url() -> String {
    format!("https://chatgpt.com/backend-api/codex/models?client_version={AGENT_VERSION}")
}

/// This route is Codex's, so the request announces the Codex client. A borrowed
/// identity that does not match the traffic it claims to be is what the edge
/// quietly rejects.
const ORIGINATOR: &str = "codex_cli_rs";
const AGENT_VERSION: &str = "0.0.1";

/// Longest a single rewrite may run. Generous for a paragraph, short enough
/// that a stalled connection cannot hold the paste hostage.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Who is asking, as the stored ChatGPT credentials describe them.
pub struct Account<'a> {
    pub access_token: &'a str,
    pub account_id: Option<&'a str>,
}

/// Why a rewrite did not produce text.
#[derive(Debug)]
pub struct Failure {
    /// The HTTP status, or 0 when no response was received.
    pub status: u16,
    pub message: String,
}

/// One model the account can use.
#[derive(Debug)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
}

fn user_agent() -> String {
    let os = match std::env::consts::OS {
        "macos" => "Mac OS",
        "windows" => "Windows",
        _ => "Linux",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => std::env::consts::ARCH,
    };
    format!("{ORIGINATOR}/{AGENT_VERSION} ({os}; {arch})")
}

/// The instructions sent with a dictation.
///
/// `global` always applies; the matched mode's prompt is appended to it, and the
/// app name is context the model may use to judge format (a chat message and a
/// document want different shapes). The window title is deliberately never
/// included: titles carry document names and email subjects.
pub fn compose_instructions(global: &str, mode: Option<&str>, app: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in [Some(global), mode].into_iter().flatten() {
        let part = part.trim();
        if !part.is_empty() {
            parts.push(part);
        }
    }
    let mut composed = parts.join("\n\n");
    if let Some(app) = app.map(str::trim).filter(|app| !app.is_empty()) {
        if !composed.is_empty() {
            composed.push_str("\n\n");
        }
        composed.push_str(&format!("The text was dictated into {app}."));
    }
    composed
}

/// Collects one streamed response.
///
/// Server-sent events arrive as `event:`/`data:` pairs; only the `data:` lines
/// carry JSON, and the text is built from `response.output_text.delta`. Success
/// means `response.completed` was seen: a stream that ends without it is a
/// failure even when some text arrived, because the rest is missing.
#[derive(Default)]
pub struct Stream {
    text: String,
    done: bool,
    failure: Option<String>,
    /// Everything that was not an event, kept so an error body can be reported
    /// when the route answers with JSON instead of a stream.
    body: String,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// True once `response.completed` has been seen.
    pub fn is_complete(&self) -> bool {
        self.done
    }

    /// Feeds one line of the response.
    pub fn absorb(&mut self, line: &str) {
        let line = line.trim_end_matches('\r');
        let Some(payload) = line.strip_prefix("data:") else {
            if !line.trim().is_empty() {
                self.body.push_str(line);
                self.body.push(' ');
            }
            return;
        };
        let payload = payload.trim();
        // Some clients send a terminator that is not JSON.
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        match value.get("type").and_then(|kind| kind.as_str()).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(|delta| delta.as_str()) {
                    self.text.push_str(delta);
                }
            }
            "response.completed" => self.done = true,
            "response.incomplete" => {
                self.failure = Some("the response stopped before it finished".to_string());
            }
            "response.failed" | "error" => {
                let detail = value
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .or_else(|| value.get("error"))
                    .or_else(|| Some(&value));
                let code = detail
                    .and_then(|detail| detail.get("code"))
                    .and_then(|code| code.as_str())
                    .map(str::to_string);
                let message = detail
                    .and_then(|detail| detail.get("message"))
                    .and_then(|message| message.as_str())
                    .map(str::to_string)
                    .or(code);
                self.failure = Some(message.unwrap_or_else(|| "the request failed".to_string()));
            }
            _ => {}
        }
    }

    /// The failure to report, if this stream did not complete cleanly.
    pub fn into_result(self, status: u16) -> Result<String, Failure> {
        if self.done {
            let text = self.text.trim().to_string();
            if text.is_empty() {
                return Err(Failure {
                    status,
                    message: "the rewrite came back empty".to_string(),
                });
            }
            return Ok(text);
        }
        if let Some(message) = self.failure {
            return Err(Failure { status, message });
        }
        let snippet: String = self.body.split_whitespace().collect::<Vec<_>>().join(" ");
        let snippet: String = snippet.chars().take(200).collect();
        Err(Failure {
            status,
            message: if snippet.is_empty() {
                "ChatGPT returned no rewrite".to_string()
            } else {
                format!("ChatGPT could not rewrite that ({status}): {snippet}")
            },
        })
    }
}

/// Splits curl's trailing `-w` status off the end of the body.
///
/// `-w "\n%{http_code}"` appends the status to stdout so a refusal can be told
/// apart from a transport failure. The stream is consumed line by line, so the
/// status is recognised as the last line that is exactly a number.
pub fn take_status(lines: &[String]) -> (Vec<String>, u16) {
    let mut body = lines.to_vec();
    let mut status = 0;
    while let Some(last) = body.pop() {
        let trimmed = last.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(code) = trimmed.parse::<u16>() {
            status = code;
        } else {
            body.push(last);
            break;
        }
    }
    (body, status)
}

fn curl(account: &Account, url: &str, extra: &[&str]) -> Command {
    let mut command = Command::new("curl");
    command.args(["-sS", "--http1.1", url]);
    command.args([
        "-H",
        &format!("Authorization: Bearer {}", account.access_token),
    ]);
    if let Some(account_id) = account
        .account_id
        .filter(|id| !id.is_empty())
    {
        command.args(["-H", &format!("chatgpt-account-id: {account_id}")]);
    }
    command.args(["-H", &format!("originator: {ORIGINATOR}")]);
    command.args(["-H", &format!("User-Agent: {}", user_agent())]);
    command.args(["-H", "Accept: application/json"]);
    command.args(extra);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    crate::console::hide_console(&mut command);
    command
}

/// Reads a model list. A body without one is a failure, not an empty account:
/// an expired token answers with a JSON error that parses perfectly well, and
/// reporting that as "no models" hides the real problem.
pub fn parse_models(body: &str) -> Result<Vec<Model>, Failure> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| Failure {
        status: 0,
        message: format!(
            "ChatGPT did not return a model list: {}",
            body.chars().take(200).collect::<String>()
        ),
    })?;
    let Some(entries) = value.get("models").and_then(|models| models.as_array()) else {
        let snippet: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        return Err(Failure {
            status: 0,
            message: format!(
                "ChatGPT did not return a model list: {}",
                snippet.chars().take(200).collect::<String>()
            ),
        });
    };
    let mut models = Vec::new();
    {
        for entry in entries {
            let slug = entry.get("slug").and_then(|slug| slug.as_str());
            let Some(slug) = slug else { continue };
            // Models not meant for a picker are skipped, as the documented
            // example does.
            if entry.get("visibility").and_then(|v| v.as_str()) == Some("hidden") {
                continue;
            }
            let display_name = entry
                .get("display_name")
                .and_then(|name| name.as_str())
                .unwrap_or(slug)
                .to_string();
            models.push(Model {
                slug: slug.to_string(),
                display_name,
            });
        }
    }
    Ok(models)
}

/// The models this account may use, in the server's order.
pub fn list_models(account: &Account) -> Result<Vec<Model>, Failure> {
    let url = models_url();
    let child = curl(account, &url, &["--max-time", "30"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Failure {
            status: 0,
            message: format!("could not start the request: {error}"),
        })?;
    let output = child.wait_with_output().map_err(|error| Failure {
        status: 0,
        message: format!("the request failed: {error}"),
    })?;
    parse_models(&String::from_utf8_lossy(&output.stdout))
}

/// Rewrites `transcript` with `instructions`, returning only finished text.
pub fn enhance(
    account: &Account,
    model: &str,
    instructions: &str,
    transcript: &str,
) -> Result<String, Failure> {
    let body = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": [{ "role": "user", "content": transcript }],
        // Both are refused otherwise, and this pair is what leaves no
        // conversation behind.
        "store": false,
        "stream": true,
    })
    .to_string();

    let mut child = curl(
        account,
        RESPONSES_URL,
        &[
            "--no-buffer",
            "--max-time",
            &REQUEST_TIMEOUT.as_secs().to_string(),
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: text/event-stream",
            "-H",
            "OpenAI-Beta: responses=experimental",
            "-d",
            "@-",
            // The status is appended after the stream so a refusal can be told
            // apart from a transport failure.
            "-w",
            "\\n%{http_code}",
        ],
    )
    .spawn()
    .map_err(|error| Failure {
        status: 0,
        message: format!("could not start the request: {error}"),
    })?;

    if let Some(stdin) = child.stdin.as_mut() {
        // A closed pipe means curl gave up; the stream read below reports why.
        let _ = stdin.write_all(body.as_bytes());
    }
    // The body is already written, so this end can close and curl can finish.
    drop(child.stdin.take());

    // curl only writes to stderr on transport errors, but a full pipe would
    // block it, so it is drained on its own thread.
    let drain = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });

    let stdout = child.stdout.take().ok_or_else(|| Failure {
        status: 0,
        message: "the request produced no stream".to_string(),
    })?;

    let mut stream = Stream::new();
    let mut lines: Vec<String> = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        stream.absorb(&line);
        // Every line is kept, including the trailing status curl appends after
        // the stream has finished.
        lines.push(line);
    }
    let _ = child.wait();
    let (_, status) = take_status(&lines);
    let stderr = drain
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    match stream.into_result(status) {
        Ok(text) => Ok(text),
        Err(mut failure) => {
            if failure.status == 0 && !stderr.trim().is_empty() {
                // No response at all: curl's own message says why.
                failure.message = format!("could not reach ChatGPT: {}", stderr.trim());
            }
            Err(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(stream: &mut Stream, lines: &[&str]) {
        for line in lines {
            stream.absorb(line);
        }
    }

    #[test]
    fn deltas_are_joined_and_a_completed_stream_succeeds() {
        let mut stream = Stream::new();
        feed(
            &mut stream,
            &[
                "event: response.output_text.delta",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\", world\"}",
                "data: {\"type\":\"response.completed\",\"response\":{}}",
            ],
        );
        assert!(stream.is_complete());
        assert_eq!(stream.into_result(200).unwrap(), "Hello, world");
    }

    #[test]
    fn a_stream_without_completed_is_a_failure_not_a_partial_paste() {
        let mut stream = Stream::new();
        feed(
            &mut stream,
            &["data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}"],
        );
        assert!(!stream.is_complete());
        let failure = stream.into_result(200).unwrap_err();
        assert_eq!(failure.status, 200);
        assert!(failure.message.contains("no rewrite"), "{}", failure.message);
    }

    #[test]
    fn a_failed_event_carries_its_code() {
        let mut stream = Stream::new();
        feed(
            &mut stream,
            &["data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}"],
        );
        let failure = stream.into_result(429).unwrap_err();
        assert!(failure
            .message
            .contains("subscription_sharing_usage_limit_exceeded"));
    }

    #[test]
    fn a_json_error_body_is_reported_with_its_status() {
        let mut stream = Stream::new();
        feed(&mut stream, &["{\"detail\":\"Store must be set to false\"}"]);
        let failure = stream.into_result(400).unwrap_err();
        assert_eq!(failure.status, 400);
        assert!(failure.message.contains("Store must be set to false"));
    }

    #[test]
    fn the_status_is_taken_from_the_end_of_the_stream() {
        let lines = vec![
            "data: {\"type\":\"response.completed\"}".to_string(),
            String::new(),
            "200".to_string(),
        ];
        let (body, status) = take_status(&lines);
        assert_eq!(status, 200);
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn a_body_that_is_not_a_status_keeps_its_last_line() {
        let lines = vec!["{\"detail\":\"nope\"}".to_string(), "0".to_string()];
        let (body, status) = take_status(&lines);
        assert_eq!(status, 0);
        // The body is what the failure message is built from, so it survives.
        assert_eq!(body, vec!["{\"detail\":\"nope\"}".to_string()]);
    }

    #[test]
    fn the_model_list_names_the_client_version_it_announces() {
        assert!(models_url().ends_with("?client_version=0.0.1"), "{}", models_url());
    }

    #[test]
    fn a_model_list_without_models_is_a_failure_not_an_empty_account() {
        let failure = parse_models("{\"error\":{\"message\":\"token expired\"}}").unwrap_err();
        assert!(failure.message.contains("token expired"), "{}", failure.message);
        assert!(parse_models("not json").is_err());
    }

    #[test]
    fn instructions_join_the_global_prompt_the_mode_and_the_app() {
        assert_eq!(
            compose_instructions("Fix it.", Some("Be brief."), Some("Slack")),
            "Fix it.\n\nBe brief.\n\nThe text was dictated into Slack."
        );
        // An app with no mode of its own still gets the global prompt.
        assert_eq!(
            compose_instructions("Fix it.", None, Some("Notes")),
            "Fix it.\n\nThe text was dictated into Notes."
        );
        // Empty pieces never leave stray blank lines behind.
        assert_eq!(compose_instructions("Fix it.", Some("  "), None), "Fix it.");
        assert_eq!(compose_instructions("", None, None), "");
    }
}
