//! The one production adapter: blocking Chat Completions over ureq
//! (ADR 0010). Profiles are presets of two wire choices, not separate
//! clients. One request is one attempt; the kernel owns retries.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::egress::{EgressRequest, Role, Target};
use super::{ModelBoundary, ModelError, Prices, Reply, Usage};

/// Most bytes of a response body (research, transport).
const BODY_MAX: u64 = 1024 * 1024;
/// Most bytes of an error body read, and characters of its message kept:
/// enough for "unknown model" or "bad parameter", not a page of HTML.
const ERROR_BODY_MAX: u64 = 8 * 1024;
const ERROR_MESSAGE_CHARS: usize = 300;
/// Connecting may take this long at most, so a dead host fails before send.
const CONNECT_MAX: Duration = Duration::from_secs(10);
/// A model name rides in every request beside the capped messages.
const MODEL_NAME_MAX: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    OpenAi,
    LlamaCpp,
    Custom,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Profile::OpenAi => "openai",
            Profile::LlamaCpp => "llama_cpp",
            Profile::Custom => "custom",
        }
    }
}

/// How the output schema is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// `response_format: {type: json_schema, json_schema: {name, schema, strict}}`.
    JsonSchema,
    /// llama.cpp's documented `response_format: {type: json_object, schema}`.
    FlatJsonSchema,
    /// `response_format: {type: json_object}`: JSON, schema unenforced.
    JsonObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenField {
    MaxCompletionTokens,
    MaxTokens,
}

/// A bearer token: never printed, and only read when a request is sent.
#[derive(Clone)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Debug, Clone)]
pub struct ChatConfig {
    pub profile: Profile,
    /// Up to and including the API version, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub model: String,
    pub output: OutputMode,
    pub token_field: TokenField,
    pub api_key: Option<Secret>,
    pub prices: Option<Prices>,
}

impl ChatConfig {
    /// From `RELIC_MODEL_*` variables, read through `get`:
    /// `PROFILE` (openai, llama_cpp, custom; default openai), `URL`,
    /// `NAME`, `API_KEY`, `OUTPUT` and `TOKEN_FIELD` (custom only), and
    /// `PRICE_IN` / `PRICE_OUT` in US dollars per million tokens.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |k: &str| get(&format!("RELIC_MODEL_{k}")).filter(|v| !v.is_empty());
        let profile = match var("PROFILE").as_deref() {
            None | Some("openai") => Profile::OpenAi,
            Some("llama_cpp") => Profile::LlamaCpp,
            Some("custom") => Profile::Custom,
            Some(p) => {
                return Err(format!(
                    "RELIC_MODEL_PROFILE {p:?} is not openai, llama_cpp or custom"
                ));
            }
        };
        let base_url = match (var("URL"), profile) {
            (Some(url), _) => url,
            (None, Profile::OpenAi) => "https://api.openai.com/v1".into(),
            (None, _) => {
                return Err(format!(
                    "RELIC_MODEL_URL is required for the {} profile",
                    profile.name()
                ));
            }
        };
        let model = var("NAME").ok_or("RELIC_MODEL_NAME is required: the model to ask for")?;
        if model.len() > MODEL_NAME_MAX {
            return Err(format!("RELIC_MODEL_NAME is over {MODEL_NAME_MAX} bytes"));
        }
        let (output, token_field) = match profile {
            Profile::OpenAi => (OutputMode::JsonSchema, TokenField::MaxCompletionTokens),
            Profile::LlamaCpp => (OutputMode::FlatJsonSchema, TokenField::MaxTokens),
            Profile::Custom => {
                let output = match var("OUTPUT").as_deref() {
                    Some("json_schema") => OutputMode::JsonSchema,
                    Some("flat_json_schema") => OutputMode::FlatJsonSchema,
                    Some("json_object") => OutputMode::JsonObject,
                    _ => return Err("the custom profile needs RELIC_MODEL_OUTPUT: json_schema, flat_json_schema or json_object".into()),
                };
                let tokens = match var("TOKEN_FIELD").as_deref() {
                    Some("max_completion_tokens") => TokenField::MaxCompletionTokens,
                    Some("max_tokens") => TokenField::MaxTokens,
                    _ => return Err("the custom profile needs RELIC_MODEL_TOKEN_FIELD: max_completion_tokens or max_tokens".into()),
                };
                (output, tokens)
            }
        };
        let api_key = var("API_KEY").map(Secret);
        if profile == Profile::OpenAi && api_key.is_none() {
            return Err("the openai profile needs RELIC_MODEL_API_KEY".into());
        }
        let price = |k: &str| -> Result<Option<u64>, String> {
            let Some(v) = var(k) else { return Ok(None) };
            match v.parse::<f64>() {
                // Whole micro-dollars per million tokens is fine-grained enough.
                Ok(usd) if usd.is_finite() && usd >= 0.0 => Ok(Some((usd * 1e6).round() as u64)),
                _ => Err(format!(
                    "RELIC_MODEL_{k} {v:?} is not a price in US dollars per million tokens"
                )),
            }
        };
        let prices = match (price("PRICE_IN")?, price("PRICE_OUT")?) {
            (Some(input_per_mtok), Some(output_per_mtok)) => Some(Prices {
                input_per_mtok,
                output_per_mtok,
            }),
            (None, None) => None,
            _ => {
                return Err(
                    "set both RELIC_MODEL_PRICE_IN and RELIC_MODEL_PRICE_OUT, or neither".into(),
                );
            }
        };
        Ok(Self {
            profile,
            base_url,
            model,
            output,
            token_field,
            api_key,
            prices,
        })
    }
}

/// The `KEY=VALUE` lines of `$XDG_CONFIG_HOME/relic/env` (else
/// `~/.config/relic/env`), read as a shell would `source` them; none when the
/// file is absent. Not a `.env` in the working directory: a cloned repository
/// could ship one that sends your records and key to its own server.
fn settings() -> Result<std::collections::HashMap<String, String>, String> {
    let dir = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    };
    // As for the database, a relative XDG value is ignored, or the settings
    // would depend on where the command ran.
    let Some(config) = dir("XDG_CONFIG_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| dir("HOME").map(|h| h.join(".config")))
    else {
        return Ok(Default::default());
    };
    let path = config.join("relic/env");
    let text = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        r => r.map_err(|e| format!("{}: {e}", path.display()))?,
    };
    let mut vars = std::collections::HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("{}:{}: not KEY=VALUE", path.display(), n + 1))?;
        let v = v.trim();
        // `source` strips the quotes; a key kept in them would not authenticate.
        let v = ['"', '\'']
            .iter()
            .find_map(|&q| v.strip_prefix(q)?.strip_suffix(q))
            .unwrap_or(v);
        vars.insert(k.trim().to_string(), v.to_string());
    }
    Ok(vars)
}

pub struct ChatCompletionsClient {
    config: ChatConfig,
    endpoint: String,
    origin: String,
    /// Plain http to a loopback address: a model on this machine.
    local: bool,
    agent: ureq::Agent,
}

impl ChatCompletionsClient {
    /// Configured by the `RELIC_MODEL_*` variables ([`ChatConfig::from_env`]),
    /// each one unset or empty in the environment taken from [`settings`].
    pub fn from_env() -> Result<Self, String> {
        let file = settings()?;
        Self::new(ChatConfig::from_env(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| file.get(k).cloned())
        })?)
    }

    /// The model it asks for.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn new(config: ChatConfig) -> Result<Self, String> {
        let (origin, local) = origin(&config.base_url)?;
        let endpoint = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        let agent = ureq::Agent::config_builder()
            // Not the environment's proxy, not a redirect: the request goes
            // where the profile says or nowhere (research, transport).
            .proxy(None)
            .max_redirects(0)
            .https_only(!local)
            .http_status_as_error(false)
            // A fresh connection per call: a connect failure then proves
            // nothing was sent, which the retry rule depends on.
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .user_agent(concat!("relic/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Self {
            config,
            endpoint,
            origin,
            local,
            agent,
        })
    }

    /// The request body, without the secret.
    pub fn body(&self, req: &EgressRequest) -> Value {
        let name = match req.role() {
            Role::Root => "relic_root_action",
            Role::Child => "relic_child_result",
        };
        let format = match self.config.output {
            OutputMode::JsonSchema => {
                json!({"type": "json_schema", "json_schema": {"name": name, "schema": req.schema(), "strict": true}})
            }
            OutputMode::FlatJsonSchema => json!({"type": "json_object", "schema": req.schema()}),
            OutputMode::JsonObject => json!({"type": "json_object"}),
        };
        let token_field = token_field(self.config.token_field);
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": req.system()},
                {"role": "user", "content": req.user()},
            ],
            "stream": false,
            "n": 1,
            "response_format": format,
        });
        body[token_field] = json!(req.role().max_output_tokens());
        body
    }
}

impl ModelBoundary for ChatCompletionsClient {
    fn target(&self) -> Target {
        Target {
            profile: self.config.profile.name().into(),
            origin: self.origin.clone(),
        }
    }

    /// A model on this machine bills nothing (research, budget and cost
    /// lease); a remote one without configured prices has an unknown cost.
    fn prices(&self) -> Option<Prices> {
        self.config.prices.or(self.local.then_some(Prices {
            input_per_mtok: 0,
            output_per_mtok: 0,
        }))
    }

    fn call(&self, req: &EgressRequest, deadline: Instant) -> Result<Reply, ModelError> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ModelError::BeforeSend(
                "the investigation's deadline has passed".into(),
            ));
        }
        let mut request = self
            .agent
            .post(&self.endpoint)
            .config()
            .timeout_global(Some(left))
            .timeout_connect(Some(left.min(CONNECT_MAX)))
            .build();
        if let Some(Secret(key)) = &self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        let response = request.send_json(self.body(req)).map_err(classify)?;
        let status = response.status().as_u16();
        if status == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(ModelError::RateLimited { retry_after });
        }
        if !(200..300).contains(&status) {
            // The status alone does not say a model name or parameter is
            // wrong; the body usually does. An unreadable body is just none.
            let body = response
                .into_body()
                .with_config()
                .limit(ERROR_BODY_MAX)
                .read_to_string();
            return Err(ModelError::Status {
                code: status,
                message: body.ok().and_then(|b| error_message(&b)),
            });
        }
        let text = response
            .into_body()
            .with_config()
            .limit(BODY_MAX)
            .read_to_string()
            .map_err(classify)?;
        reply(&text)
    }
}

/// `relic model check`: one synthetic child call and one synthetic root call,
/// each held to the same checks recall uses. Never sends user data
/// (research, model HTTP boundary). The receipt says what the endpoint
/// did either way; the error says why it failed.
pub fn check(client: &ChatCompletionsClient) -> (Value, Result<(), String>) {
    use crate::rlm::executor::{check_child_result, child_payload};
    use crate::rlm::model::{EvidenceItem, EvidencePacket, parse_output};
    use crate::rlm::protocol::{RootActionKind, RootActionV1};

    let packet = EvidencePacket {
        packet_id: "pkt_00".into(),
        items: vec![EvidenceItem {
            item_id: "item_0".into(),
            evidence_id: "ev1_0000000000000001".into(),
            safe_text: "ls -la /tmp".into(),
            truncation: None,
        }],
    };
    let state = json!({
        "question": "Which command listed the files in /tmp?",
        "state_version": 0,
        "corpus": {"observation_count": 1},
        "handles": [], "notes": [],
        "budget_remaining": {"root_calls": 1, "child_calls": 0, "plan_ops": 8, "rows_returned": 20},
    });
    let probes = [
        (
            Role::Child,
            child_payload("Did this command list files in /tmp?", &packet),
        ),
        (Role::Root, state),
    ];
    let mut ledger =
        crate::rlm::budget::BudgetLedger::new(crate::rlm::protocol::BudgetV1::prototype_default());
    let mut roles = serde_json::Map::new();
    let mut verdict = Ok(());
    for (role, payload) in probes {
        let name = format!("{role:?}").to_lowercase();
        let outcome = EgressRequest::new(role, payload, &client.target(), &mut ledger, &mut |_| {})
            .map_err(|e| e.to_string())
            .and_then(|req| {
                client
                    .call(&req, Instant::now() + CHECK_DEADLINE)
                    .map_err(|e| e.to_string())
            });
        let checked = outcome
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|reply| match role {
                Role::Child => check_child_result(&packet, &reply.body)
                    .map(drop)
                    .map_err(|e| e.to_string()),
                Role::Root => parse_output::<RootActionV1>(
                    &reply.body,
                    crate::rlm::coordinator::ROOT_RESULT_MAX,
                )
                .map_err(|e| format!("{e:?}"))
                .and_then(|a| match a.kind {
                    RootActionKind::Plan => crate::rlm::validate::validate_plan(&a, 0)
                        .map(drop)
                        .map_err(|e| e.to_string()),
                    RootActionKind::Finish if a.finish.is_some() => Ok(()),
                    RootActionKind::Finish => Err("a finish without its body".into()),
                }),
            });
        let reply = outcome.as_ref().ok();
        roles.insert(
            name.clone(),
            json!({
                "ok": checked.is_ok(),
                "returned_model": reply.and_then(|r| r.returned_model.clone()),
                "usage_reported": reply.is_some_and(|r| r.usage.is_some()),
                "error": checked.as_ref().err(),
            }),
        );
        if let (Err(e), Ok(())) = (checked, &verdict) {
            verdict = Err(format!("{name} call: {e}"));
        }
    }
    let receipt = json!({
        "schema": "relic.model-check/v1",
        "relic_version": env!("CARGO_PKG_VERSION"),
        "profile": client.config.profile.name(),
        "endpoint_origin_hash": crate::rlm::evidence::hex_hash(client.origin.as_bytes()),
        "requested_model": client.config.model,
        "output_mode": match client.config.output {
            OutputMode::JsonSchema => "json_schema",
            OutputMode::FlatJsonSchema => "flat_json_schema",
            OutputMode::JsonObject => "json_object",
        },
        "token_field": token_field(client.config.token_field),
        "checked_at_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        "roles": roles,
    });
    (receipt, verdict)
}

/// How long each probe may take.
const CHECK_DEADLINE: Duration = Duration::from_secs(60);
fn token_field(field: TokenField) -> &'static str {
    match field {
        TokenField::MaxCompletionTokens => "max_completion_tokens",
        TokenField::MaxTokens => "max_tokens",
    }
}

/// `scheme://host[:port]` of `base_url`, and whether it is plain HTTP to a
/// literal loopback address, the only place cleartext may go.
fn origin(base_url: &str) -> Result<(String, bool), String> {
    let (scheme, rest) = base_url
        .split_once("://")
        .ok_or_else(|| format!("{base_url:?} is not a URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(format!(
            "{base_url:?} needs a host and no user name or password"
        ));
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    let loopback = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    match scheme {
        "https" => Ok((format!("https://{authority}"), false)),
        "http" if loopback => Ok((format!("http://{authority}"), true)),
        "http" => Err(format!(
            "{base_url:?}: plain http is only for a loopback address such as 127.0.0.1; use https"
        )),
        _ => Err(format!("{base_url:?} is not http or https")),
    }
}

/// Which side of the send a transport failure fell on. Only a failure to
/// connect proves nothing went out; anything later may have run.
fn classify(e: ureq::Error) -> ModelError {
    use std::io::ErrorKind::*;
    use ureq::Timeout;
    match e {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Tls(_) => {
            ModelError::BeforeSend(e.to_string())
        }
        ureq::Error::Timeout(Timeout::Resolve | Timeout::Connect) => {
            ModelError::BeforeSend(e.to_string())
        }
        ureq::Error::Io(ref io)
            if matches!(
                io.kind(),
                ConnectionRefused | AddrNotAvailable | NetworkUnreachable | HostUnreachable
            ) =>
        {
            ModelError::BeforeSend(e.to_string())
        }
        ureq::Error::BodyExceedsLimit(_) => ModelError::Oversize,
        e => ModelError::Ambiguous(e.to_string()),
    }
}

#[derive(Deserialize)]
struct Completion {
    model: Option<String>,
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
    refusal: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// What an error body says: OpenAI's `{"error": {"message"}}`, a bare
/// `error` or `message` string, or else the text itself. Redacted, as it
/// may echo what was sent, then cut to [`ERROR_MESSAGE_CHARS`]: cut first,
/// a secret across the cut is too short to be judged one and keeps its head.
fn error_message(body: &str) -> Option<String> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    let said = json.as_ref().map_or(Some(body), |v| {
        v["error"]["message"]
            .as_str()
            .or(v["error"].as_str())
            .or(v["message"].as_str())
    });
    let said =
        crate::redact::Redactor::new().redact(said.map(str::trim).filter(|s| !s.is_empty())?);
    let mut cut: String = said.chars().take(ERROR_MESSAGE_CHARS).collect();
    if cut.len() < said.len() {
        cut.push('…');
    }
    Some(cut)
}

/// A 2xx body as a [`Reply`]. Provider extras are ignored here; what the
/// model wrote is checked strictly by the kernel.
fn reply(text: &str) -> Result<Reply, ModelError> {
    let c: Completion =
        serde_json::from_str(text).map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
    let [choice] = <[Choice; 1]>::try_from(c.choices).map_err(|choices| {
        ModelError::InvalidResponse(format!("{} choices, not 1", choices.len()))
    })?;
    if let Some(refusal) = choice.message.refusal.filter(|r| !r.is_empty()) {
        return Err(ModelError::Refused(refusal));
    }
    match choice.finish_reason.as_deref() {
        Some("stop") | None => {}
        Some("length") => return Err(ModelError::Truncated),
        Some("content_filter") => return Err(ModelError::Refused("content filter".into())),
        Some(other) => {
            return Err(ModelError::InvalidResponse(format!(
                "finish_reason {other:?}"
            )));
        }
    }
    let body = choice
        .message
        .content
        .ok_or_else(|| ModelError::InvalidResponse("no message content".into()))?;
    let usage = c.usage.map(|u| Usage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
    });
    Ok(Reply {
        body,
        returned_model: c.model,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rlm::budget::BudgetLedger;
    use crate::rlm::protocol::BudgetV1;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// What the mock server does with one request, once it has read it all.
    enum Act {
        Answer {
            status: u16,
            headers: &'static str,
            body: String,
        },
        /// Holds the connection open without a byte of response.
        Hang,
        /// Closes the connection without a response.
        Close,
    }

    /// A one-connection-per-act HTTP server on loopback. Each request it
    /// read, head and body, comes out of the receiver.
    fn serve(acts: Vec<Act>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for act in acts {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let _ = tx.send(format!("{head}\r\n{}", String::from_utf8_lossy(&body)));
                let mut stream = reader.into_inner();
                match act {
                    Act::Answer {
                        status,
                        headers,
                        body,
                    } => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
                            body.len()
                        );
                    }
                    Act::Hang => std::thread::sleep(Duration::from_secs(5)),
                    Act::Close => {}
                }
            }
        });
        (url, rx)
    }

    fn config(profile: Profile, base_url: String) -> ChatConfig {
        let (output, token_field) = match profile {
            Profile::OpenAi => (OutputMode::JsonSchema, TokenField::MaxCompletionTokens),
            Profile::LlamaCpp => (OutputMode::FlatJsonSchema, TokenField::MaxTokens),
            _ => (OutputMode::JsonObject, TokenField::MaxTokens),
        };
        ChatConfig {
            profile,
            base_url,
            model: "m-1".into(),
            output,
            token_field,
            api_key: Some(Secret("sk-test-key".into())),
            prices: None,
        }
    }

    fn request(client: &ChatCompletionsClient) -> EgressRequest {
        let mut ledger = BudgetLedger::new(BudgetV1::prototype_default());
        EgressRequest::new(
            Role::Child,
            json!({"objective": "o"}),
            &client.target(),
            &mut ledger,
            &mut |_| {},
        )
        .unwrap()
    }

    fn completion(content: &str, finish: &str) -> String {
        json!({"id": "c1", "object": "chat.completion", "model": "m-1-2026", "system_fingerprint": "fp",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}})
        .to_string()
    }

    fn call_with(act: Act, wait: Duration) -> Result<Reply, ModelError> {
        let (url, _rx) = serve(vec![act]);
        let client = ChatCompletionsClient::new(config(Profile::LlamaCpp, url)).unwrap();
        client.call(&request(&client), Instant::now() + wait)
    }

    fn answer(status: u16, headers: &'static str, body: String) -> Act {
        Act::Answer {
            status,
            headers,
            body,
        }
    }

    #[test]
    fn each_failure_is_classed_as_the_retry_rule_needs() {
        let long = Duration::from_secs(5);
        // Nothing listens: refused before a byte was sent.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = ChatCompletionsClient::new(config(
            Profile::LlamaCpp,
            format!("http://127.0.0.1:{port}/v1"),
        ))
        .unwrap();
        assert!(matches!(
            client.call(&request(&client), Instant::now() + long),
            Err(ModelError::BeforeSend(_))
        ));

        let rate = call_with(answer(429, "retry-after: 2\r\n", "{}".into()), long);
        assert_eq!(
            rate,
            Err(ModelError::RateLimited {
                retry_after: Some(Duration::from_secs(2))
            })
        );
        let rate = call_with(answer(429, "", "{}".into()), long);
        assert_eq!(rate, Err(ModelError::RateLimited { retry_after: None }));
        assert_eq!(
            call_with(answer(500, "", "{}".into()), long),
            Err(ModelError::Status {
                code: 500,
                message: None
            })
        );
        assert_eq!(
            call_with(answer(529, "", "{}".into()), long),
            Err(ModelError::Status {
                code: 529,
                message: None
            })
        );
        // What the endpoint said comes along, cut short and redacted.
        let openai = json!({"error": {"message": "unknown provider for model x/y", "code": "model_not_found"}}).to_string();
        let said = call_with(answer(400, "", openai), long)
            .unwrap_err()
            .to_string();
        assert_eq!(
            said,
            "model endpoint answered HTTP 400: unknown provider for model x/y"
        );
        let plain = call_with(answer(502, "", "  Bad Gateway\n".into()), long)
            .unwrap_err()
            .to_string();
        assert_eq!(plain, "model endpoint answered HTTP 502: Bad Gateway");
        let echoed = json!({"message": "rejected: password=hunter2"}).to_string();
        let said = call_with(answer(422, "", echoed), long)
            .unwrap_err()
            .to_string();
        assert_eq!(
            said,
            "model endpoint answered HTTP 422: rejected: password=[REDACTED:password]"
        );
        let long_body = json!({"error": "y".repeat(5000)}).to_string();
        let Err(ModelError::Status {
            message: Some(m), ..
        }) = call_with(answer(400, "", long_body), long)
        else {
            panic!()
        };
        assert_eq!(
            (m.chars().count(), m.ends_with('…')),
            (ERROR_MESSAGE_CHARS + 1, true)
        );
        let across_the_cut =
            json!({"error": format!("{} ghp_{}", "y".repeat(290), "a1B2".repeat(8))}).to_string();
        let Err(ModelError::Status {
            message: Some(m), ..
        }) = call_with(answer(400, "", across_the_cut), long)
        else {
            panic!()
        };
        assert!(!m.contains("ghp_"), "{m}");
        let huge = "z".repeat(ERROR_BODY_MAX as usize + 1);
        assert_eq!(
            call_with(answer(400, "", huge), long),
            Err(ModelError::Status {
                code: 400,
                message: None
            })
        );
        // The request went out and then the line dropped, or went quiet.
        assert!(matches!(
            call_with(Act::Close, long),
            Err(ModelError::Ambiguous(_))
        ));
        let t = Instant::now();
        assert!(matches!(
            call_with(Act::Hang, Duration::from_millis(300)),
            Err(ModelError::Ambiguous(_))
        ));
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "the deadline bounds the call: {:?}",
            t.elapsed()
        );

        let big = "x".repeat(BODY_MAX as usize + 1);
        assert_eq!(
            call_with(answer(200, "", big), long),
            Err(ModelError::Oversize)
        );
        assert!(matches!(
            call_with(answer(200, "", "not json".into()), long),
            Err(ModelError::InvalidResponse(_))
        ));
        let refusal = json!({"choices": [{"message": {"content": null, "refusal": "no"}, "finish_reason": "stop"}]}).to_string();
        assert_eq!(
            call_with(answer(200, "", refusal), long),
            Err(ModelError::Refused("no".into()))
        );
        assert_eq!(
            call_with(answer(200, "", completion("{\"a\":", "length")), long),
            Err(ModelError::Truncated)
        );
        let two =
            json!({"choices": [{"message": {"content": "{}"}}, {"message": {"content": "{}"}}]})
                .to_string();
        assert!(
            matches!(call_with(answer(200, "", two), long), Err(ModelError::InvalidResponse(e)) if e.contains("2 choices"))
        );
    }

    #[test]
    fn a_completion_gives_its_content_returned_model_and_usage() {
        // Provider extras at the wire level are fine; the content is the
        // model's and the kernel checks it strictly.
        let reply = call_with(
            answer(200, "", completion("{\"ok\":true}", "stop")),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply.body, "{\"ok\":true}");
        assert_eq!(reply.returned_model.as_deref(), Some("m-1-2026"));
        assert_eq!(
            reply.usage,
            Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 3
            })
        );
    }

    #[test]
    fn the_request_carries_only_what_the_profile_says() {
        let cases = [
            (
                Profile::OpenAi,
                "max_completion_tokens",
                json!("json_schema"),
            ),
            (Profile::LlamaCpp, "max_tokens", json!("json_object")),
        ];
        for (profile, token_field, format) in cases {
            let (url, rx) = serve(vec![answer(200, "", completion("{}", "stop"))]);
            let client = ChatCompletionsClient::new(config(profile, url)).unwrap();
            let req = request(&client);
            client
                .call(&req, Instant::now() + Duration::from_secs(5))
                .unwrap();
            let sent = rx.recv().unwrap();
            let (head, body) = sent.split_once("\r\n\r\n").unwrap();
            assert!(
                head.starts_with("POST /v1/chat/completions HTTP/1.1"),
                "{head}"
            );
            assert!(
                head.to_ascii_lowercase()
                    .contains("authorization: bearer sk-test-key"),
                "{head}"
            );
            let body: Value = serde_json::from_str(body).unwrap();
            let mut keys: Vec<&str> = body
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let mut expected = vec![
                "messages",
                "model",
                "n",
                "response_format",
                "stream",
                token_field,
            ];
            expected.sort_unstable();
            assert_eq!(keys, expected, "{profile:?}");
            assert_eq!(
                (
                    body["stream"].clone(),
                    body["n"].clone(),
                    body[token_field].clone()
                ),
                (json!(false), json!(1), json!(768))
            );
            assert_eq!(body["response_format"]["type"], format, "{profile:?}");
            assert_eq!(body["messages"][1]["content"], req.user());
            // The schema rides where the profile's server reads it.
            let schema = match profile {
                Profile::OpenAi => &body["response_format"]["json_schema"]["schema"],
                _ => &body["response_format"]["schema"],
            };
            assert_eq!(schema, req.schema());
            // The cap allowed for the wrapper: what went out is within it.
            assert!(
                serde_json::to_vec(&body).unwrap().len() <= req.bytes(),
                "{profile:?}"
            );
        }
        let shown = format!(
            "{:?}",
            config(Profile::OpenAi, "https://x.example/v1".into())
        );
        assert!(!shown.contains("sk-test-key"), "{shown}");
    }

    #[test]
    fn model_check_holds_both_roles_to_recall_checks() {
        let child = |item: &str| {
            json!({"version": 1, "packet_id": "pkt_00", "assessments": [{"item_id": item, "verdict": "supports",
                "confidence": "high", "claims": [{"text": "ls listed /tmp", "supports": ["ev1_0000000000000001"], "contradicts": []}],
                "missing": []}]})
            .to_string()
        };
        let plan = json!({"version": 1, "expected_state_version": 0, "kind": "plan", "finish": null, "plan": {"ops": [
            {"op": "search", "op_id": "s1", "terms": ["ls", "tmp"], "match": "all", "limit": 10}]}})
        .to_string();
        let run = |child: String, root: String| {
            let (url, rx) = serve(vec![
                answer(200, "", completion(&child, "stop")),
                answer(200, "", completion(&root, "stop")),
            ]);
            let client = ChatCompletionsClient::new(config(Profile::OpenAi, url.clone())).unwrap();
            let (receipt, verdict) = check(&client);
            let sent: Vec<String> = rx.try_iter().collect();
            (receipt, verdict, sent, url)
        };

        let (receipt, verdict, sent, url) = run(child("item_0"), plan.clone());
        assert_eq!(verdict, Ok(()));
        assert_eq!(
            (
                receipt["roles"]["child"]["ok"].clone(),
                receipt["roles"]["root"]["ok"].clone()
            ),
            (json!(true), json!(true))
        );
        assert_eq!(receipt["roles"]["root"]["returned_model"], "m-1-2026");
        assert_eq!(sent.len(), 2);
        // The receipt names the endpoint by hash only.
        let origin = url.trim_end_matches("/v1");
        assert!(!receipt.to_string().contains(origin), "{receipt}");

        // A reply that recall would reject fails the check, and says where.
        let (receipt, verdict, _, _) = run(child("item_9"), plan);
        assert!(
            verdict
                .unwrap_err()
                .starts_with("child call: item_id \"item_9\"")
        );
        assert_eq!(
            (
                receipt["roles"]["child"]["ok"].clone(),
                receipt["roles"]["root"]["ok"].clone()
            ),
            (json!(false), json!(true))
        );
        let relate = json!({"version": 1, "expected_state_version": 0, "kind": "plan", "finish": null,
            "plan": {"ops": [{"op": "relate", "op_id": "r1"}]}});
        let (_, verdict, _, _) = run(child("item_0"), relate.to_string());
        assert!(
            verdict
                .unwrap_err()
                .starts_with("root call: relate is not run in this version")
        );
    }

    #[test]
    fn cleartext_goes_only_to_a_literal_loopback_address() {
        assert_eq!(
            origin("http://127.0.0.1:8080/v1").unwrap(),
            ("http://127.0.0.1:8080".into(), true)
        );
        assert_eq!(
            origin("http://[::1]:8000/v1").unwrap(),
            ("http://[::1]:8000".into(), true)
        );
        assert_eq!(
            origin("https://api.example.com/v1?x=1").unwrap(),
            ("https://api.example.com".into(), false)
        );
        for refused in [
            "http://localhost:8080/v1",
            "http://10.0.0.1/v1",
            "https://user:pw@api.example.com/v1",
            "ftp://x/v1",
            "api.example.com",
        ] {
            assert!(origin(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn configuration_comes_from_the_environment() {
        let env = |pairs: &[(&str, &str)]| {
            let map: std::collections::HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (format!("RELIC_MODEL_{k}"), v.to_string()))
                .collect();
            ChatConfig::from_env(move |k| map.get(k).cloned())
        };
        let c = env(&[
            ("NAME", "gpt-x"),
            ("API_KEY", "k"),
            ("PRICE_IN", "2.5"),
            ("PRICE_OUT", "10"),
        ])
        .unwrap();
        assert_eq!(
            (c.profile, c.base_url.as_str(), c.token_field),
            (
                Profile::OpenAi,
                "https://api.openai.com/v1",
                TokenField::MaxCompletionTokens
            )
        );
        assert_eq!(
            c.prices,
            Some(Prices {
                input_per_mtok: 2_500_000,
                output_per_mtok: 10_000_000
            })
        );
        let c = env(&[
            ("PROFILE", "llama_cpp"),
            ("URL", "http://127.0.0.1:8080/v1"),
            ("NAME", "q"),
        ])
        .unwrap();
        assert_eq!(
            (c.output, c.api_key.is_none()),
            (OutputMode::FlatJsonSchema, true)
        );
        assert!(env(&[("NAME", "gpt-x")]).unwrap_err().contains("API_KEY"));
        assert!(
            env(&[("PROFILE", "llama_cpp"), ("NAME", "q")])
                .unwrap_err()
                .contains("URL")
        );
        assert!(
            env(&[
                ("PROFILE", "vllm"),
                ("URL", "http://127.0.0.1/v1"),
                ("NAME", "q")
            ])
            .unwrap_err()
            .contains("not openai, llama_cpp or custom")
        );
        assert!(
            env(&[
                ("PROFILE", "custom"),
                ("URL", "http://127.0.0.1/v1"),
                ("NAME", "q")
            ])
            .unwrap_err()
            .contains("OUTPUT")
        );
        assert!(
            env(&[("NAME", "q"), ("API_KEY", "k"), ("PRICE_IN", "1")])
                .unwrap_err()
                .contains("neither")
        );
    }
}
