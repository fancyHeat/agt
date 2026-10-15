//! Optional LLM clean-up of transcripts ("polish"), behind [`agt_core::polish::Polisher`].
//!
//! Two APIs share one blocking HTTP client (`ureq`, keep-alive connection pool):
//! - OpenAI-compatible `POST {base_url}/chat/completions` (Ollama, LM Studio, llama.cpp's
//!   `llama-server`, cloud providers);
//! - the Anthropic Messages API, `POST {base_url}/v1/messages`.
//!
//! `provider = "s1-mini"` ([`managed`]) is the OpenAI-compatible API against a
//! `llama-server` that AGT downloads, starts and supervises itself.
//!
//! The pipeline calls [`polish_or_raw`], which never fails: it skips short transcripts and
//! falls back to the raw transcript on errors, timeouts and suspicious output (see
//! [`guard`]). Every request has a hard deadline ([`PolishConfig::timeout`]) covering
//! connect, send and the whole response.

pub mod guard;
pub mod managed;
pub mod prompt;

use agt_core::{
    config::{PolishConfig, PolishProvider},
    polish::{PolishCtx, Polisher, Result},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

mod reach;
use reach::Reach;

pub use guard::{Outcome, polish_or_raw};
pub use managed::{Env, Status};

/// Deadline for [`Polisher::warmup`]: a local server may have to load the model first.
const WARMUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Builds the configured backend. Fails on settings that can never work (no API key for
/// Anthropic, a malformed URL, no llama.cpp build for this platform), so the app can say so
/// at start-up. For `s1-mini` this only starts the background thread that downloads and
/// runs the server (see [`managed`]); it never blocks.
pub fn load(cfg: &PolishConfig, env: &Env) -> std::result::Result<Box<dyn Polisher>, String> {
    match cfg.provider {
        PolishProvider::S1Mini => Ok(Box::new(managed::start(cfg, env)?)),
        PolishProvider::OpenAi | PolishProvider::Anthropic => load_remote(cfg),
    }
}

/// A server the user runs (`openai`, `anthropic`).
fn load_remote(cfg: &PolishConfig) -> std::result::Result<Box<dyn Polisher>, String> {
    let base = cfg.base_url().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(format!(
            "polish.base_url must start with http:// or https:// (got {base:?})"
        ));
    }
    let key = cfg.api_key();
    if cfg.provider == PolishProvider::Anthropic && key.is_none() {
        return Err(format!(
            "no API key for anthropic: set the {} environment variable or polish.api_key",
            cfg.api_key_var().unwrap_or("ANTHROPIC_API_KEY")
        ));
    }
    if key.is_some() && !is_https_or_loopback(base) {
        tracing::warn!(
            "polish: the API key will be sent unencrypted over plain http to {base}; use https unless that server is on a network you trust"
        );
    }
    let api = match cfg.provider {
        PolishProvider::Anthropic => Api::Anthropic,
        PolishProvider::OpenAi | PolishProvider::S1Mini => Api::OpenAi,
    };
    Ok(Box::new(Llm::new(cfg, api, base, key)))
}

/// `https://...`, or plain http to this machine.
fn is_https_or_loopback(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("http://") else {
        return true;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Api {
    OpenAi,
    Anthropic,
}

struct Llm {
    cfg: PolishConfig,
    api: Api,
    /// `base_url` as given (for [`Polisher::name`]).
    base: String,
    url: String,
    key: Option<String>,
    agent: ureq::Agent,
    reach: Arc<Reach>,
}

impl Llm {
    fn new(cfg: &PolishConfig, api: Api, base: &str, key: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            // Read error bodies instead of turning 4xx/5xx into opaque errors.
            .http_status_as_error(false)
            // A redirect would be followed as a GET (useless here) and ureq would carry our
            // custom `x-api-key` header along to wherever it points.
            .max_redirects(0)
            // Dictations are often minutes apart; keep the (TLS) connection for reuse longer
            // than ureq's 15 s default. A failed request never returns its connection.
            .max_idle_age(Duration::from_secs(90))
            .user_agent(concat!("agt/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self {
            url: match api {
                Api::OpenAi => format!("{base}/chat/completions"),
                Api::Anthropic => format!("{base}/v1/messages"),
            },
            base: base.to_owned(),
            cfg: cfg.clone(),
            api,
            key,
            agent,
            reach: Arc::new(Reach::new(base)),
        }
    }
}

impl Polisher for Llm {
    fn name(&self) -> String {
        let provider = match self.api {
            Api::OpenAi => "openai-compatible",
            Api::Anthropic => "anthropic",
        };
        format!("{provider} {} at {}", self.cfg.model(), self.base)
    }

    /// Sends one short request: loads the model on a local server, opens the (TLS)
    /// connection, and surfaces a bad key or model name at start-up rather than on the
    /// first dictation.
    fn warmup(&self) -> Result<()> {
        self.request(WARMUP_TEXT, WARMUP_TIMEOUT).map(drop)
    }

    fn polish(&self, text: &str, _ctx: &PolishCtx) -> Result<String> {
        self.request(text, self.cfg.timeout(text.split_whitespace().count()))
    }
}

/// What [`Polisher::warmup`] sends.
const WARMUP_TEXT: &str = "um so this is uh a test";

impl Llm {
    fn request(&self, text: &str, timeout: Duration) -> Result<String> {
        // A server known to be unreachable: fail now instead of after the deadline (on
        // Windows even a refused local connection takes ~2 s of retries).
        self.reach.ready()?;
        let p = prompt::build(&self.cfg, text);
        // Cleaned text is about as long as the input. UTF-8 bytes / 2 leaves ~2x headroom for
        // English (~4 bytes per token) and still enough for CJK (3 bytes per char, ~1 token
        // per char), while stopping a runaway answer early.
        let max_tokens = text.len() / 2 + 64;
        let messages: Vec<Value> = p
            .turns
            .iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect();
        let req = self
            .agent
            .post(&self.url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("content-type", "application/json");
        let (req, body) = match self.api {
            Api::OpenAi => {
                let mut all = vec![json!({ "role": "system", "content": p.system })];
                all.extend(messages);
                let body = json!({
                    "model": self.cfg.model(),
                    "messages": all,
                    "max_tokens": max_tokens,
                    "temperature": 0,
                    "stream": false,
                });
                let req = match &self.key {
                    Some(k) => req.header("authorization", &format!("Bearer {k}")),
                    None => req,
                };
                (req, body)
            }
            Api::Anthropic => {
                // No sampling parameters: current Claude models reject non-default ones.
                // Thinking off: this is a latency-bound rewrite, not a reasoning task.
                let body = json!({
                    "model": self.cfg.model(),
                    "max_tokens": max_tokens,
                    "system": p.system,
                    "messages": messages,
                    "thinking": { "type": "disabled" },
                });
                let req = req
                    .header("x-api-key", self.key.as_deref().unwrap_or_default())
                    .header("anthropic-version", "2023-06-01");
                (req, body)
            }
        };
        // Neither the request nor the response is logged: both hold dictated text, and logs
        // (agt.db events) must never contain any.
        let mut resp = req
            .send(body.to_string())
            .inspect_err(|_| self.reach.check())?;
        let status = resp.status();
        let body = resp.body_mut().read_to_string()?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), error_message(&body)).into());
        }
        // The response holds dictated text: never put it in the error.
        let v: Value =
            serde_json::from_str(&body).map_err(|e| format!("malformed response: {e}"))?;
        Ok(match self.api {
            Api::OpenAi => parse_openai(&v),
            Api::Anthropic => parse_anthropic(&v),
        }?)
    }
}

/// `choices[0].message.content`; a cut-off answer is an error.
fn parse_openai(v: &Value) -> std::result::Result<String, &'static str> {
    let choice = &v["choices"][0];
    if choice["finish_reason"] == "length" {
        return Err("output truncated (max_tokens)");
    }
    choice["message"]["content"]
        .as_str()
        .map(str::to_owned)
        .ok_or("no choices[0].message.content in response")
}

/// The text blocks of `content` (read by type: a response may start with other blocks).
fn parse_anthropic(v: &Value) -> std::result::Result<String, &'static str> {
    match v["stop_reason"].as_str() {
        Some("max_tokens") => return Err("output truncated (max_tokens)"),
        Some("refusal") => return Err("model refused"),
        _ => {}
    }
    let blocks = v["content"].as_array().ok_or("no content in response")?;
    let text: String = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    if text.is_empty() && !blocks.iter().any(|b| b["type"] == "text") {
        return Err("no text block in response");
    }
    Ok(text)
}

/// `error.message` (OpenAI and Anthropic both use it). Any other body (e.g. a proxy's HTML
/// page, which might echo the request) is only described by its size, since the error ends
/// up in a WARN event.
fn error_message(body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| match &v["error"] {
            Value::String(s) => Some(s.clone()),
            e => e["message"].as_str().map(str::to_owned),
        });
    match message {
        Some(m) => snippet(&m),
        None => format!("non-JSON error body ({} bytes)", body.len()),
    }
}

fn snippet(s: &str) -> String {
    const MAX: usize = 200;
    let s = s.trim();
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests;
