#![allow(dead_code)]

//! WeChat Official Account channel for IronClaw.
//!
//! This WASM component implements the channel interface for handling WeChat
//! webhooks and sending messages back via the Customer Service Message API.
//!
//! # Features
//!
//! - Webhook-based message receiving (GET verification + POST messages)
//! - Text, voice (with recognition), image, and event message handling
//! - Customer Service Message API for async replies (bypasses 5s timeout)
//! - Byte-aware message chunking (2048-byte UTF-8 limit)
//! - Markdown stripping for WeChat plain-text display
//! - Access token caching with auto-refresh
//!
//! # Security
//!
//! - AppID and AppSecret are injected by host during HTTP requests
//! - WASM never sees raw credentials
//! - Verify token is passed via config for SHA1 webhook verification

// Generate bindings from the WIT file
wit_bindgen::generate!({
    world: "sandboxed-channel",
    path: "../../wit/channel.wit",
});

use serde::{Deserialize, Serialize};

// Re-export generated types
use exports::near::agent::channel::{
    AgentResponse, ChannelConfig, Guest, HttpEndpointConfig, IncomingHttpRequest,
    OutgoingHttpResponse, StatusUpdate,
};
use near::agent::channel_host::{self, EmittedMessage};

// ============================================================================
// Constants
// ============================================================================

const CHANNEL_NAME: &str = "wechat";

/// Workspace paths for persistent state across WASM callbacks.
const ACCESS_TOKEN_PATH: &str = "state/access_token.json";
const VERIFY_TOKEN_PATH: &str = "state/verify_token";
const OWNER_ID_PATH: &str = "state/owner_id";

/// WeChat Customer Service API text message byte limit.
const MAX_MESSAGE_BYTES: usize = 2000;

// ============================================================================
// WeChat XML Message Types
// ============================================================================

/// Metadata stored with emitted messages for response routing.
#[derive(Debug, Serialize, Deserialize)]
struct WeChatMessageMetadata {
    /// User's OpenID (sender).
    from_user: String,
    /// Official Account's original ID (receiver).
    to_user: String,
    /// Message type for context.
    msg_type: String,
    /// Target agent for keyword-based routing (if matched).
    #[serde(skip_serializing_if = "Option::is_none")]
    target_agent: Option<String>,
}

/// Cached access token with expiry.
#[derive(Debug, Serialize, Deserialize)]
struct AccessTokenCache {
    token: String,
    expires_at: u64,
}

/// WeChat API token response.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    errcode: Option<i64>,
    errmsg: Option<String>,
}

/// WeChat API generic response (for sendMessage etc.)
#[derive(Debug, Deserialize)]
struct WeChatApiResponse {
    errcode: Option<i64>,
    errmsg: Option<String>,
}

/// Config from capabilities.json `config` field.
#[derive(Debug, Deserialize)]
struct WeChatConfig {
    verify_token: Option<String>,
    owner_id: Option<String>,
    dm_policy: Option<String>,
    allow_from: Option<Vec<String>>,
}

// ============================================================================
// Channel Implementation
// ============================================================================

struct WeChatChannel;

impl Guest for WeChatChannel {
    /// Initialize the channel. Parse config and persist state.
    fn on_start(config_json: String) -> Result<ChannelConfig, String> {
        channel_host::log(channel_host::LogLevel::Info, "WeChat channel starting");

        // Parse config
        if let Ok(config) = serde_json::from_str::<WeChatConfig>(&config_json) {
            // Persist verify token for webhook validation
            if let Some(ref token) = config.verify_token {
                let _ = channel_host::workspace_write(VERIFY_TOKEN_PATH, token);
            }
            // Persist owner_id
            if let Some(ref owner_id) = config.owner_id {
                let _ = channel_host::workspace_write(OWNER_ID_PATH, owner_id);
            }
        }

        Ok(ChannelConfig {
            display_name: "WeChat".to_string(),
            http_endpoints: vec![HttpEndpointConfig {
                path: "/webhook/wechat".to_string(),
                methods: vec!["GET".to_string(), "POST".to_string()],
                require_secret: false, // We do our own SHA1 verification
            }],
            poll: None,
        })
    }

    /// Handle incoming HTTP request from WeChat.
    fn on_http_request(req: IncomingHttpRequest) -> OutgoingHttpResponse {
        match req.method.as_str() {
            "GET" => handle_verification(req),
            "POST" => handle_message(req),
            _ => http_response(405, "Method Not Allowed"),
        }
    }

    /// Not used — WeChat is webhook-only.
    fn on_poll() {}

    /// Deliver agent response back to WeChat user via Customer Service API.
    fn on_respond(response: AgentResponse) -> Result<(), String> {
        let metadata: WeChatMessageMetadata =
            serde_json::from_str(&response.metadata_json).map_err(|e| {
                format!("Failed to parse response metadata: {e}")
            })?;

        let content = strip_markdown(&response.content);

        // Get access token (refresh if needed)
        let access_token = get_access_token()?;

        // Split into chunks that fit WeChat's byte limit
        let chunks = split_message(&content, MAX_MESSAGE_BYTES);

        channel_host::log(
            channel_host::LogLevel::Info,
            &format!(
                "Sending {} chunk(s) to {}",
                chunks.len(),
                &metadata.from_user
            ),
        );

        for chunk in &chunks {
            send_text_message(&access_token, &metadata.from_user, chunk)?;
        }

        Ok(())
    }

    /// Handle agent status updates.
    fn on_status(_update: StatusUpdate) {
        // WeChat has no typing indicator API for Customer Service messages.
        // Nothing to do here.
    }

    /// Clean up on shutdown.
    fn on_shutdown() {
        channel_host::log(channel_host::LogLevel::Info, "WeChat channel shutting down");
    }
}

export!(WeChatChannel);

// ============================================================================
// Webhook Verification (GET)
// ============================================================================

/// Handle WeChat webhook verification (GET request).
/// WeChat sends: signature, timestamp, nonce, echostr
/// We compute SHA1(sort([token, timestamp, nonce])) and compare with signature.
fn handle_verification(req: IncomingHttpRequest) -> OutgoingHttpResponse {
    let query: serde_json::Value =
        serde_json::from_str(&req.query_json).unwrap_or(serde_json::Value::Null);

    let signature = query_str(&query, "signature");
    let timestamp = query_str(&query, "timestamp");
    let nonce = query_str(&query, "nonce");
    let echostr = query_str(&query, "echostr");

    if signature.is_empty() || echostr.is_empty() {
        return http_response(400, "Missing verification parameters");
    }

    // Read the verify token from workspace (set during on_start)
    let verify_token = channel_host::workspace_read(VERIFY_TOKEN_PATH)
        .unwrap_or_default();

    if verify_token.is_empty() {
        channel_host::log(
            channel_host::LogLevel::Error,
            "Verify token not configured. Set verify_token in channel config.",
        );
        return http_response(500, "Verify token not configured");
    }

    // Compute SHA1 of sorted [token, timestamp, nonce]
    let mut parts = vec![verify_token.as_str(), timestamp.as_str(), nonce.as_str()];
    parts.sort();
    let combined = parts.join("");
    let computed = sha1_hex(&combined);

    if computed == signature {
        channel_host::log(channel_host::LogLevel::Info, "WeChat webhook verification passed");
        // Return echostr as plain text (WeChat requirement)
        OutgoingHttpResponse {
            status: 200,
            headers_json: r#"{"Content-Type":"text/plain"}"#.to_string(),
            body: echostr.into_bytes(),
        }
    } else {
        channel_host::log(
            channel_host::LogLevel::Warn,
            &format!(
                "WeChat verification failed: expected={computed}, got={signature}"
            ),
        );
        http_response(403, "Forbidden")
    }
}

// ============================================================================
// Message Handling (POST)
// ============================================================================

/// Handle incoming WeChat message (POST request with XML body).
fn handle_message(req: IncomingHttpRequest) -> OutgoingHttpResponse {
    let body_str = match String::from_utf8(req.body) {
        Ok(s) => s,
        Err(_) => return http_response(400, "Invalid UTF-8 body"),
    };

    // Parse XML using simple tag extraction (no XML parser dependency)
    let msg_type = extract_xml_cdata(&body_str, "MsgType").unwrap_or_default();
    let from_user = extract_xml_cdata(&body_str, "FromUserName").unwrap_or_default();
    let to_user = extract_xml_cdata(&body_str, "ToUserName").unwrap_or_default();
    let content = extract_xml_cdata(&body_str, "Content").unwrap_or_default();
    let recognition = extract_xml_cdata(&body_str, "Recognition").unwrap_or_default();
    let event = extract_xml_cdata(&body_str, "Event").unwrap_or_default();

    if from_user.is_empty() {
        return http_response(400, "Missing FromUserName");
    }

    channel_host::log(
        channel_host::LogLevel::Info,
        &format!("Received {msg_type} message from {from_user}"),
    );

    // Send immediate "thinking" indicator before LLM processes the message.
    // Only for user-generated messages (text, voice, image), not events.
    if matches!(msg_type.as_str(), "text" | "voice" | "image") {
        send_typing_indicator(&from_user);
    }

    // Route to specific agent based on message content keywords.
    // For voice messages, use recognition text (content is empty for voice).
    let route_text = if !content.is_empty() { &content } else { &recognition };
    let target_agent = route_by_keywords(route_text);

    let metadata = WeChatMessageMetadata {
        from_user: from_user.clone(),
        to_user: to_user.clone(),
        msg_type: msg_type.clone(),
        target_agent: target_agent.clone(),
    };
    let metadata_json = serde_json::to_string(&metadata).unwrap_or_default();

    if let Some(ref agent) = target_agent {
        channel_host::log(
            channel_host::LogLevel::Info,
            &format!("Keyword routing → agent '{agent}'"),
        );
    }

    match msg_type.as_str() {
        "text" => {
            if !content.is_empty() {
                channel_host::emit_message(&EmittedMessage {
                    user_id: from_user.clone(),
                    user_name: None,
                    content: content.clone(),
                    thread_id: None,
                    metadata_json,
                });
            }
        }
        "voice" => {
            // Use WeChat's built-in voice recognition
            let text = recognition
                .trim()
                .trim_end_matches(|c: char| {
                    matches!(
                        c,
                        '\u{3002}' | '\u{FF1F}' | '\u{FF01}' | '\u{FF0C}'
                            | '\u{3001}' | '\u{FF1B}' | '\u{FF1A}'
                            | '.' | '?' | '!' | ',' | ';' | ':'
                    )
                })
                .to_string();

            if !text.is_empty() {
                channel_host::log(
                    channel_host::LogLevel::Info,
                    &format!("Voice recognition: \"{recognition}\" -> \"{text}\""),
                );
                channel_host::emit_message(&EmittedMessage {
                    user_id: from_user.clone(),
                    user_name: None,
                    content: text,
                    thread_id: None,
                    metadata_json,
                });
            } else {
                channel_host::log(
                    channel_host::LogLevel::Warn,
                    "Voice recognition returned empty text",
                );
            }
        }
        "image" => {
            // Emit a notification that an image was received
            channel_host::emit_message(&EmittedMessage {
                user_id: from_user.clone(),
                user_name: None,
                content: "[User sent an image]".to_string(),
                thread_id: None,
                metadata_json,
            });
        }
        "event" => {
            match event.as_str() {
                "subscribe" => {
                    channel_host::log(
                        channel_host::LogLevel::Info,
                        &format!("New subscriber: {from_user}"),
                    );
                    // Emit a subscribe event as a message
                    channel_host::emit_message(&EmittedMessage {
                        user_id: from_user.clone(),
                        user_name: None,
                        content: "[User subscribed to the account]".to_string(),
                        thread_id: None,
                        metadata_json,
                    });
                }
                "unsubscribe" => {
                    channel_host::log(
                        channel_host::LogLevel::Info,
                        &format!("Unsubscribed: {from_user}"),
                    );
                }
                _ => {
                    channel_host::log(
                        channel_host::LogLevel::Debug,
                        &format!("Unhandled event type: {event}"),
                    );
                }
            }
        }
        _ => {
            channel_host::log(
                channel_host::LogLevel::Debug,
                &format!("Unhandled message type: {msg_type}"),
            );
        }
    }

    // Return immediate "success" response to avoid WeChat's 5-second timeout.
    // The actual reply is sent via Customer Service API in on_respond.
    http_response(200, "success")
}

// ============================================================================
// Keyword-Based Agent Routing
// ============================================================================

/// Route incoming message to a specific agent based on keyword matching.
/// Returns the agent ID if a keyword match is found, or None for default routing
/// (which falls through to the main/Iron agent).
///
/// Priority order: newsbot > tutor > zoe
fn route_by_keywords(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    let lower = text.to_lowercase();

    // newsbot (TechPulse 科技脉搏): 科技资讯相关
    const NEWSBOT_KEYWORDS: &[&str] = &[
        "资讯", "新闻", "digest", "news", "日报", "科技",
    ];

    // tutor (学术助教): 学术相关
    const TUTOR_KEYWORDS: &[&str] = &[
        "讲义", "作业", "homework", "lecture", "课程", "笔记",
    ];

    // zoe (开发编排): 开发相关
    const ZOE_KEYWORDS: &[&str] = &[
        "zoe", "开发", "施工", "dev", "code", "编码",
    ];

    for kw in NEWSBOT_KEYWORDS {
        if lower.contains(kw) {
            return Some("newsbot".to_string());
        }
    }

    for kw in TUTOR_KEYWORDS {
        if lower.contains(kw) {
            return Some("tutor".to_string());
        }
    }

    for kw in ZOE_KEYWORDS {
        if lower.contains(kw) {
            return Some("zoe".to_string());
        }
    }

    None
}

// ============================================================================
// WeChat API: Access Token
// ============================================================================

/// Get a valid access token, refreshing if expired.
fn get_access_token() -> Result<String, String> {
    let now_ms = channel_host::now_millis();

    // Check cached token
    if let Some(cached_str) = channel_host::workspace_read(ACCESS_TOKEN_PATH) {
        if let Ok(cached) = serde_json::from_str::<AccessTokenCache>(&cached_str) {
            // 5-minute buffer before expiry
            if cached.expires_at > now_ms + 300_000 {
                return Ok(cached.token);
            }
        }
    }

    // Refresh token
    // Host replaces {WECHAT_APP_ID} and {WECHAT_APP_SECRET} via url_path credential injection
    let url = "https://api.weixin.qq.com/cgi-bin/token?grant_type=client_credential&appid={WECHAT_APP_ID}&secret={WECHAT_APP_SECRET}";

    let resp = channel_host::http_request("GET", url, "{}", None, Some(10_000))
        .map_err(|e| format!("Token request failed: {e}"))?;

    if resp.status != 200 {
        return Err(format!("Token endpoint returned status {}", resp.status));
    }

    let body = String::from_utf8(resp.body)
        .map_err(|_| "Token response is not valid UTF-8".to_string())?;

    let token_resp: TokenResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse token response: {e}"))?;

    if let Some(errcode) = token_resp.errcode {
        if errcode != 0 {
            let errmsg = token_resp.errmsg.unwrap_or_default();
            return Err(format!("WeChat token error {errcode}: {errmsg}"));
        }
    }

    let access_token = token_resp
        .access_token
        .ok_or("No access_token in response")?;
    let expires_in = token_resp.expires_in.unwrap_or(7200);

    // Cache the token
    let cache = AccessTokenCache {
        token: access_token.clone(),
        expires_at: now_ms + expires_in * 1000,
    };
    let cache_json = serde_json::to_string(&cache).unwrap_or_default();
    let _ = channel_host::workspace_write(ACCESS_TOKEN_PATH, &cache_json);

    channel_host::log(
        channel_host::LogLevel::Info,
        "WeChat access token refreshed",
    );

    Ok(access_token)
}

// ============================================================================
// WeChat API: Typing Indicator
// ============================================================================

/// Send an immediate "⌛️..." typing indicator to the user.
/// Best-effort: failures are logged but don't block message processing.
fn send_typing_indicator(to_user: &str) {
    match get_access_token() {
        Ok(token) => {
            if let Err(e) = send_text_message(&token, to_user, "⌛...") {
                channel_host::log(
                    channel_host::LogLevel::Warn,
                    &format!("Typing indicator send failed: {e}"),
                );
            }
        }
        Err(e) => {
            channel_host::log(
                channel_host::LogLevel::Warn,
                &format!("Typing indicator skipped (token error): {e}"),
            );
        }
    }
}

// ============================================================================
// WeChat API: Send Message
// ============================================================================

/// Send a text message via Customer Service Message API.
fn send_text_message(
    access_token: &str,
    to_user: &str,
    content: &str,
) -> Result<(), String> {
    let url = format!(
        "https://api.weixin.qq.com/cgi-bin/message/custom/send?access_token={access_token}"
    );

    let payload = serde_json::json!({
        "touser": to_user,
        "msgtype": "text",
        "text": { "content": content }
    });

    let body = serde_json::to_vec(&payload).unwrap_or_default();

    let resp = channel_host::http_request(
        "POST",
        &url,
        r#"{"Content-Type":"application/json"}"#,
        Some(&body),
        Some(10_000),
    )
    .map_err(|e| format!("Send message failed: {e}"))?;

    if resp.status != 200 {
        return Err(format!("Send message returned status {}", resp.status));
    }

    let resp_body = String::from_utf8(resp.body).unwrap_or_default();
    if let Ok(api_resp) = serde_json::from_str::<WeChatApiResponse>(&resp_body) {
        if let Some(errcode) = api_resp.errcode {
            if errcode != 0 {
                let errmsg = api_resp.errmsg.unwrap_or_default();
                return Err(format!("WeChat send error {errcode}: {errmsg}"));
            }
        }
    }

    Ok(())
}

// ============================================================================
// Byte-Aware Message Chunking
// ============================================================================

/// Split a long message into chunks that fit within a byte limit.
/// WeChat Customer Service API limit: ~2048 bytes per text message.
/// We use 2000 to leave a safety margin.
fn split_message(text: &str, max_bytes: usize) -> Vec<String> {
    if text.len() <= max_bytes {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.split('\n') {
        let candidate = if current.is_empty() {
            line.to_string()
        } else {
            format!("{current}\n{line}")
        };

        if candidate.len() > max_bytes {
            if !current.is_empty() {
                chunks.push(current);
                current = line.to_string();
                // If single line still exceeds, hard-split by characters
                if current.len() > max_bytes {
                    hard_split_chars(&mut chunks, &current, max_bytes);
                    current = String::new();
                }
            } else {
                // Single line exceeds limit — hard split by characters
                hard_split_chars(&mut chunks, line, max_bytes);
                current = String::new();
            }
        } else {
            current = candidate;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

/// Hard-split a string by character boundaries to fit within max_bytes.
fn hard_split_chars(chunks: &mut Vec<String>, text: &str, max_bytes: usize) {
    let mut partial = String::new();
    for ch in text.chars() {
        let ch_len = ch.len_utf8();
        if partial.len() + ch_len > max_bytes {
            if !partial.is_empty() {
                chunks.push(partial);
            }
            partial = String::from(ch);
        } else {
            partial.push(ch);
        }
    }
    if !partial.is_empty() {
        chunks.push(partial);
    }
}

// ============================================================================
// Markdown Stripping
// ============================================================================

/// Strip Markdown formatting for WeChat plain-text display.
fn strip_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());

    for line in text.lines() {
        let stripped = line.trim_start();

        // Strip heading markers
        if stripped.starts_with('#') {
            let content = stripped.trim_start_matches('#').trim_start();
            result.push_str(content);
        } else {
            result.push_str(stripped);
        }

        result.push('\n');
    }

    // Strip bold/italic markers
    let result = result.replace("**", "");
    let result = result.replace("__", "");
    let result = result.replace('*', "");

    // Strip inline code
    let result = result.replace('`', "");

    // Convert [text](url) to text (url)
    let result = convert_markdown_links(&result);

    result.trim().to_string()
}

/// Convert Markdown links [text](url) to plain text: text (url)
fn convert_markdown_links(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '[' {
            // Look for ](url)
            if let Some(close_bracket) = find_char(&chars, ']', i + 1) {
                if close_bracket + 1 < chars.len() && chars[close_bracket + 1] == '(' {
                    if let Some(close_paren) = find_char(&chars, ')', close_bracket + 2) {
                        let link_text: String = chars[i + 1..close_bracket].iter().collect();
                        let url: String =
                            chars[close_bracket + 2..close_paren].iter().collect();
                        result.push_str(&link_text);
                        result.push_str(" (");
                        result.push_str(&url);
                        result.push(')');
                        i = close_paren + 1;
                        continue;
                    }
                }
            }
            result.push('[');
        } else {
            result.push(chars[i]);
        }
        i += 1;
    }

    result
}

fn find_char(chars: &[char], target: char, start: usize) -> Option<usize> {
    for (idx, &ch) in chars[start..].iter().enumerate() {
        if ch == target {
            return Some(start + idx);
        }
    }
    None
}

// ============================================================================
// XML Parsing Helpers
// ============================================================================

/// Extract CDATA content from an XML tag: <Tag><![CDATA[content]]></Tag>
/// Also handles plain content: <Tag>content</Tag>
fn extract_xml_cdata(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");

    let start = xml.find(&open)?;
    let after_open = start + open.len();
    let end = xml[after_open..].find(&close)?;
    let inner = &xml[after_open..after_open + end];

    // Strip CDATA wrapper if present
    let content = if inner.starts_with("<![CDATA[") && inner.ends_with("]]>") {
        &inner[9..inner.len() - 3]
    } else {
        inner.trim()
    };

    Some(content.to_string())
}

// ============================================================================
// SHA1 Implementation (minimal, no external dependency)
// ============================================================================

/// Compute SHA1 hash and return hex string.
fn sha1_hex(input: &str) -> String {
    let hash = sha1_digest(input.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA1 message digest (RFC 3174).
fn sha1_digest(message: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let ml = (message.len() as u64) * 8;

    // Pre-processing: add padding
    let mut padded = message.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&ml.to_be_bytes());

    // Process each 512-bit block
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);

        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    digest[0..4].copy_from_slice(&h0.to_be_bytes());
    digest[4..8].copy_from_slice(&h1.to_be_bytes());
    digest[8..12].copy_from_slice(&h2.to_be_bytes());
    digest[12..16].copy_from_slice(&h3.to_be_bytes());
    digest[16..20].copy_from_slice(&h4.to_be_bytes());
    digest
}

// ============================================================================
// Utility Helpers
// ============================================================================

/// Extract a string value from a JSON query object.
fn query_str<'a>(query: &'a serde_json::Value, key: &str) -> String {
    query
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Build a simple HTTP response.
fn http_response(status: u16, body: &str) -> OutgoingHttpResponse {
    OutgoingHttpResponse {
        status,
        headers_json: r#"{"Content-Type":"text/plain"}"#.to_string(),
        body: body.as_bytes().to_vec(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_xml_cdata() {
        let xml = r#"<xml>
<ToUserName><![CDATA[gh_123456]]></ToUserName>
<FromUserName><![CDATA[oUser123]]></FromUserName>
<CreateTime>1348831860</CreateTime>
<MsgType><![CDATA[text]]></MsgType>
<Content><![CDATA[Hello World]]></Content>
<MsgId>1234567890123456</MsgId>
</xml>"#;

        assert_eq!(extract_xml_cdata(xml, "ToUserName"), Some("gh_123456".to_string()));
        assert_eq!(extract_xml_cdata(xml, "FromUserName"), Some("oUser123".to_string()));
        assert_eq!(extract_xml_cdata(xml, "MsgType"), Some("text".to_string()));
        assert_eq!(extract_xml_cdata(xml, "Content"), Some("Hello World".to_string()));
        assert_eq!(extract_xml_cdata(xml, "CreateTime"), Some("1348831860".to_string()));
        assert_eq!(extract_xml_cdata(xml, "NonExistent"), None);
    }

    #[test]
    fn test_extract_xml_voice() {
        let xml = r#"<xml>
<MsgType><![CDATA[voice]]></MsgType>
<Recognition><![CDATA[今日资讯。]]></Recognition>
</xml>"#;

        assert_eq!(extract_xml_cdata(xml, "Recognition"), Some("今日资讯。".to_string()));
    }

    #[test]
    fn test_split_message_short() {
        let text = "Hello";
        let chunks = split_message(text, 2000);
        assert_eq!(chunks, vec!["Hello"]);
    }

    #[test]
    fn test_split_message_multiline() {
        let line = "A".repeat(1000);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = split_message(&text, 2000);
        // First two lines fit in ~2001 bytes (with newline), so they should split
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= 2000);
        }
    }

    #[test]
    fn test_split_message_cjk() {
        // Each CJK char is 3 bytes in UTF-8
        let text: String = std::iter::repeat('中').take(700).collect();
        assert_eq!(text.len(), 2100); // 700 * 3 = 2100 bytes
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        // First chunk should have at most 666 chars (666*3=1998)
        assert!(chunks[0].len() <= 2000);
        assert!(chunks[1].len() <= 2000);
    }

    #[test]
    fn test_strip_markdown() {
        assert_eq!(strip_markdown("## Hello"), "Hello");
        assert_eq!(strip_markdown("**bold**"), "bold");
        assert_eq!(strip_markdown("`code`"), "code");
        assert_eq!(
            strip_markdown("[link](https://example.com)"),
            "link (https://example.com)"
        );
    }

    #[test]
    fn test_sha1_hex() {
        // Known SHA1: SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(
            sha1_hex("abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn test_sha1_wechat_verification() {
        // Simulate WeChat verification:
        // sort(["token", "timestamp", "nonce"]) -> join -> SHA1
        let token = "test_token";
        let timestamp = "1234567890";
        let nonce = "nonce123";

        let mut parts = vec![token, timestamp, nonce];
        parts.sort();
        let combined = parts.join("");
        let hash = sha1_hex(&combined);

        // Verify it's a valid 40-char hex string
        assert_eq!(hash.len(), 40);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_route_by_keywords_newsbot() {
        assert_eq!(route_by_keywords("今日资讯"), Some("newsbot".to_string()));
        assert_eq!(route_by_keywords("看看新闻"), Some("newsbot".to_string()));
        assert_eq!(route_by_keywords("tech news today"), Some("newsbot".to_string()));
        assert_eq!(route_by_keywords("morning digest"), Some("newsbot".to_string()));
        assert_eq!(route_by_keywords("今天的科技日报"), Some("newsbot".to_string()));
    }

    #[test]
    fn test_route_by_keywords_tutor() {
        assert_eq!(route_by_keywords("生成讲义"), Some("tutor".to_string()));
        assert_eq!(route_by_keywords("帮我写作业"), Some("tutor".to_string()));
        assert_eq!(route_by_keywords("homework help"), Some("tutor".to_string()));
        assert_eq!(route_by_keywords("课程笔记"), Some("tutor".to_string()));
        assert_eq!(route_by_keywords("lecture notes"), Some("tutor".to_string()));
    }

    #[test]
    fn test_route_by_keywords_zoe() {
        assert_eq!(route_by_keywords("zoe 帮我"), Some("zoe".to_string()));
        assert_eq!(route_by_keywords("开发一个功能"), Some("zoe".to_string()));
        assert_eq!(route_by_keywords("写个code"), Some("zoe".to_string()));
        assert_eq!(route_by_keywords("编码任务"), Some("zoe".to_string()));
    }

    #[test]
    fn test_route_by_keywords_default() {
        assert_eq!(route_by_keywords("你好"), None);
        assert_eq!(route_by_keywords("今天天气怎么样"), None);
        assert_eq!(route_by_keywords(""), None);
        assert_eq!(route_by_keywords("hello world"), None);
    }

    #[test]
    fn test_route_by_keywords_case_insensitive() {
        assert_eq!(route_by_keywords("NEWS today"), Some("newsbot".to_string()));
        assert_eq!(route_by_keywords("HOMEWORK"), Some("tutor".to_string()));
        assert_eq!(route_by_keywords("ZOE"), Some("zoe".to_string()));
    }

    #[test]
    fn test_hard_split_chars() {
        let mut chunks = Vec::new();
        let text = "你好世界测试"; // 6 chars, 18 bytes
        hard_split_chars(&mut chunks, text, 9); // 3 chars per chunk
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "你好世");
        assert_eq!(chunks[1], "界测试");
    }
}
