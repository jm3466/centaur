use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    ApiError,
    api_jwt::{bearer_token, verify_console_jwt},
    routes::{AppState, non_empty_env},
    slack_proxy::{
        content_disposition_filename, content_length, ensure_upload_size, http_client,
        validate_content_type, validate_filename,
    },
};

const DEFAULT_DISCORD_API_URL: &str = "https://discord.com/api/v10";
// Discord asks HTTP API clients to identify themselves in this format.
const DISCORD_USER_AGENT: &str = "DiscordBot (https://github.com/paradigmxyz/centaur, 0.1.0)";
const MAX_DISCORD_FILE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_DISCORD_MESSAGES_LIMIT: u16 = 100;
const DISCORD_CHANNEL_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
// Announcement, public, and private thread channel types.
const DISCORD_THREAD_TYPES: [u8; 3] = [10, 11, 12];
const DISCORD_SUPPRESS_EMBEDS_FLAG: u64 = 1 << 2;

pub(crate) fn discord_proxy_router() -> Router<AppState> {
    Router::new()
        .route("/api/discord/channels", get(get_discord_channels))
        .route(
            "/api/discord/channels/{channel_id}/messages",
            get(get_discord_messages).post(post_discord_message),
        )
        .route(
            "/api/discord/channels/{channel_id}/messages/{message_id}",
            get(get_discord_message),
        )
        .route(
            "/api/discord/channels/{channel_id}/messages/{message_id}/attachments/{attachment_id}",
            get(download_discord_attachment),
        )
        .route(
            "/api/discord/channels/{channel_id}/files",
            post(upload_discord_file).layer(DefaultBodyLimit::disable()),
        )
}

#[derive(Debug, Deserialize)]
struct DiscordMessagesQuery {
    #[serde(default)]
    limit: Option<u16>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordMessageRequest {
    content: String,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    suppress_embeds: bool,
}

#[derive(Debug, Deserialize)]
struct DiscordFileUploadQuery {
    filename: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordProxyClaims {
    // Tokens minted before the Discord claim existed carry no Discord channels.
    #[serde(default)]
    discord: DiscordChannelClaims,
}

#[derive(Debug, Default, Deserialize)]
struct DiscordChannelClaims {
    #[serde(default)]
    upload_channels: Vec<String>,
    #[serde(default)]
    download_channels: Vec<String>,
    #[serde(default)]
    history_channels: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DiscordChannelsResponse {
    ok: bool,
    channels: Vec<DiscordChannelItem>,
    count: usize,
}

#[derive(Debug, Serialize)]
struct DiscordChannelItem {
    id: String,
    name: String,
    guild_id: String,
    topic: String,
    can_upload: bool,
    can_download: bool,
    can_read_history: bool,
}

#[derive(Debug, Serialize)]
struct DiscordMessagesResponse {
    ok: bool,
    channel_id: String,
    messages: Value,
}

#[derive(Debug, Serialize)]
struct DiscordMessageResponse {
    ok: bool,
    channel_id: String,
    message: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct DiscordChannel {
    #[serde(default, rename = "type")]
    channel_type: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    topic: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordAttachment {
    url: String,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    content_type: Option<String>,
}

async fn get_discord_channels(
    headers: HeaderMap,
) -> Result<Json<DiscordChannelsResponse>, ApiError> {
    let claims = authorize_discord_proxy(&headers)?;
    let channel_ids = discord_channel_ids_from_claims(&claims)?;

    let config = discord_proxy_config()?;
    let client = http_client();
    let mut channels = Vec::new();
    for channel_id in channel_ids {
        match discord_channel(client, config, &channel_id).await {
            Ok(channel) => channels.push(discord_channel_item(&claims, &channel_id, &channel)),
            Err(error) => {
                tracing::warn!(
                    channel_id,
                    error = %error,
                    "skipping Discord channel whose metadata could not be fetched"
                );
            }
        }
    }
    channels.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(Json(DiscordChannelsResponse {
        ok: true,
        count: channels.len(),
        channels,
    }))
}

async fn get_discord_messages(
    headers: HeaderMap,
    Path(channel_id): Path<String>,
    Query(query): Query<DiscordMessagesQuery>,
) -> Result<Json<DiscordMessagesResponse>, ApiError> {
    let claims = authorize_discord_proxy(&headers)?;
    validate_discord_id(&channel_id, "channel")?;
    validate_discord_messages_query(&query)?;

    let config = discord_proxy_config()?;
    let client = http_client();
    ensure_discord_channel_allowed(
        &claims,
        client,
        config,
        &channel_id,
        DiscordChannelPermission::History,
    )
    .await?;
    let messages = discord_messages(client, config, &channel_id, &query).await?;
    Ok(Json(DiscordMessagesResponse {
        ok: true,
        channel_id,
        messages,
    }))
}

async fn get_discord_message(
    headers: HeaderMap,
    Path((channel_id, message_id)): Path<(String, String)>,
) -> Result<Json<DiscordMessageResponse>, ApiError> {
    let claims = authorize_discord_proxy(&headers)?;
    validate_discord_id(&channel_id, "channel")?;
    validate_discord_id(&message_id, "message")?;

    let config = discord_proxy_config()?;
    let client = http_client();
    ensure_discord_channel_allowed(
        &claims,
        client,
        config,
        &channel_id,
        DiscordChannelPermission::History,
    )
    .await?;
    let message = discord_message(client, config, &channel_id, &message_id).await?;
    Ok(Json(DiscordMessageResponse {
        ok: true,
        channel_id,
        message,
    }))
}

async fn post_discord_message(
    headers: HeaderMap,
    Path(channel_id): Path<String>,
    Json(request): Json<DiscordMessageRequest>,
) -> Result<Json<DiscordMessageResponse>, ApiError> {
    let claims = authorize_discord_proxy(&headers)?;
    validate_discord_id(&channel_id, "channel")?;
    if let Some(reply_to) = request.reply_to.as_deref() {
        validate_discord_id(reply_to, "message")?;
    }

    let config = discord_proxy_config()?;
    let client = http_client();
    ensure_discord_channel_allowed(
        &claims,
        client,
        config,
        &channel_id,
        DiscordChannelPermission::Upload,
    )
    .await?;
    let payload = discord_message_payload(
        Some(&request.content),
        request.reply_to.as_deref(),
        request.suppress_embeds,
    );
    let message = discord_api_json(
        client
            .post(format!("{}/channels/{channel_id}/messages", config.api_url))
            .json(&payload),
        config,
        "message post",
    )
    .await?;
    Ok(Json(DiscordMessageResponse {
        ok: true,
        channel_id,
        message,
    }))
}

async fn upload_discord_file(
    headers: HeaderMap,
    Path(channel_id): Path<String>,
    Query(query): Query<DiscordFileUploadQuery>,
    body: Body,
) -> Result<Json<DiscordMessageResponse>, ApiError> {
    let claims = authorize_discord_proxy(&headers)?;
    validate_discord_id(&channel_id, "channel")?;
    validate_filename(&query.filename)?;
    if let Some(reply_to) = query.reply_to.as_deref() {
        validate_discord_id(reply_to, "message")?;
    }
    if let Some(content_type) = query.content_type.as_deref() {
        validate_content_type(content_type)?;
    }
    let content_length = content_length(&headers)?;
    ensure_upload_size(content_length, MAX_DISCORD_FILE_BYTES)?;

    let config = discord_proxy_config()?;
    let client = http_client();
    ensure_discord_channel_allowed(
        &claims,
        client,
        config,
        &channel_id,
        DiscordChannelPermission::Upload,
    )
    .await?;
    let message =
        upload_discord_file_bytes(client, config, &channel_id, &query, body, content_length)
            .await?;
    Ok(Json(DiscordMessageResponse {
        ok: true,
        channel_id,
        message,
    }))
}

async fn download_discord_attachment(
    headers: HeaderMap,
    Path((channel_id, message_id, attachment_id)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let claims = authorize_discord_proxy(&headers)?;
    validate_discord_id(&channel_id, "channel")?;
    validate_discord_id(&message_id, "message")?;
    validate_discord_id(&attachment_id, "attachment")?;

    let config = discord_proxy_config()?;
    let client = http_client();
    ensure_discord_channel_allowed(
        &claims,
        client,
        config,
        &channel_id,
        DiscordChannelPermission::Download,
    )
    .await?;
    discord_attachment_response(client, config, &channel_id, &message_id, &attachment_id).await
}

/// Streams an attachment of a message read from `channel_id`, so a download
/// can only reach files posted in the authorized channel.
async fn discord_attachment_response(
    client: &reqwest::Client,
    config: &DiscordProxyConfig,
    channel_id: &str,
    message_id: &str,
    attachment_id: &str,
) -> Result<Response, ApiError> {
    let message = discord_message(client, config, channel_id, message_id).await?;
    let attachment = discord_attachment(&message, attachment_id)?;

    let upstream = client.get(&attachment.url).send().await.map_err(|error| {
        ApiError::Internal(format!("Discord attachment download failed: {error}"))
    })?;
    if !upstream.status().is_success() {
        return Err(ApiError::BadRequest(format!(
            "Discord attachment download failed with status {}",
            upstream.status().as_u16()
        )));
    }

    let upstream_content_length = upstream.headers().get(header::CONTENT_LENGTH).cloned();
    let mut response = Body::from_stream(upstream.bytes_stream()).into_response();
    let headers = response.headers_mut();
    if let Some(value) = attachment
        .content_type
        .as_deref()
        .and_then(|value| value.parse().ok())
    {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(value) = upstream_content_length {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let filename = if attachment.filename.is_empty() {
        attachment_id
    } else {
        &attachment.filename
    };
    if let Ok(value) = content_disposition_filename(filename).parse::<HeaderValue>() {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

fn discord_attachment(message: &Value, attachment_id: &str) -> Result<DiscordAttachment, ApiError> {
    let attachment = message
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|attachment| attachment.get("id").and_then(Value::as_str) == Some(attachment_id))
        .ok_or_else(|| {
            ApiError::NotFound("attachment is not on this Discord message".to_owned())
        })?;
    let attachment = serde_json::from_value::<DiscordAttachment>(attachment.clone())
        .map_err(|error| ApiError::Internal(format!("Discord attachment was invalid: {error}")))?;
    if attachment.size > MAX_DISCORD_FILE_BYTES {
        return Err(ApiError::PayloadTooLarge(format!(
            "attachment exceeds {MAX_DISCORD_FILE_BYTES} byte limit"
        )));
    }
    Ok(attachment)
}

async fn discord_messages(
    client: &reqwest::Client,
    config: &DiscordProxyConfig,
    channel_id: &str,
    query: &DiscordMessagesQuery,
) -> Result<Value, ApiError> {
    let mut url = format!("{}/channels/{channel_id}/messages", config.api_url);
    let params = [
        ("limit", query.limit.map(|limit| limit.to_string())),
        ("before", query.before.clone()),
        ("after", query.after.clone()),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name}={value}")))
    .collect::<Vec<_>>();
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    discord_api_json(client.get(url), config, "message list").await
}

async fn discord_message(
    client: &reqwest::Client,
    config: &DiscordProxyConfig,
    channel_id: &str,
    message_id: &str,
) -> Result<Value, ApiError> {
    discord_api_json(
        client.get(format!(
            "{}/channels/{channel_id}/messages/{message_id}",
            config.api_url
        )),
        config,
        "message lookup",
    )
    .await
}

async fn upload_discord_file_bytes(
    client: &reqwest::Client,
    config: &DiscordProxyConfig,
    channel_id: &str,
    query: &DiscordFileUploadQuery,
    body: Body,
    content_length: u64,
) -> Result<Value, ApiError> {
    let mut payload =
        discord_message_payload(query.content.as_deref(), query.reply_to.as_deref(), false);
    payload.insert(
        "attachments".to_owned(),
        json!([{ "id": 0, "filename": query.filename }]),
    );
    let mut file = Part::stream_with_length(
        reqwest::Body::wrap_stream(body.into_data_stream()),
        content_length,
    )
    .file_name(query.filename.clone());
    if let Some(content_type) = query.content_type.as_deref() {
        file = file
            .mime_str(content_type)
            .map_err(|_| ApiError::BadRequest("invalid content_type".to_owned()))?;
    }
    let form = Form::new()
        .text("payload_json", Value::Object(payload).to_string())
        .part("files[0]", file);
    discord_api_json(
        client
            .post(format!("{}/channels/{channel_id}/messages", config.api_url))
            .multipart(form),
        config,
        "file upload",
    )
    .await
}

fn discord_message_payload(
    content: Option<&str>,
    reply_to: Option<&str>,
    suppress_embeds: bool,
) -> Map<String, Value> {
    let mut payload = Map::new();
    if let Some(content) = content {
        payload.insert("content".to_owned(), json!(content));
    }
    // A reference without a channel is a same-channel reply, never a forward
    // of a message from another channel.
    if let Some(message_id) = reply_to {
        payload.insert(
            "message_reference".to_owned(),
            json!({ "message_id": message_id }),
        );
    }
    if suppress_embeds {
        payload.insert("flags".to_owned(), json!(DISCORD_SUPPRESS_EMBEDS_FLAG));
    }
    payload
}

async fn discord_api_json(
    request: reqwest::RequestBuilder,
    config: &DiscordProxyConfig,
    action: &str,
) -> Result<Value, ApiError> {
    let mut authorization = HeaderValue::from_str(&format!("Bot {}", config.bot_token))
        .map_err(|_| ApiError::Internal("DISCORD_BOT_TOKEN is not a valid header".to_owned()))?;
    authorization.set_sensitive(true);
    let response = request
        .header(header::AUTHORIZATION, authorization)
        .header(header::USER_AGENT, DISCORD_USER_AGENT)
        .send()
        .await
        .map_err(|error| ApiError::Internal(format!("Discord API request failed: {error}")))?;
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        tracing::warn!(action, "Discord API rate limited");
        return Err(ApiError::TooManyRequests {
            message: format!("Discord {action} is rate limited"),
            retry_after: response.headers().get(header::RETRY_AFTER).cloned(),
        });
    }

    let value = response.json::<Value>().await.map_err(|error| {
        ApiError::Internal(format!("Discord API response was not JSON: {error}"))
    })?;
    if !status.is_success() {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(ApiError::BadRequest(format!(
            "Discord {action} failed with status {}: {message}",
            status.as_u16()
        )));
    }
    Ok(value)
}

struct CachedDiscordChannel {
    fetched_at: Instant,
    channel: DiscordChannel,
}

type DiscordChannelCache = tokio::sync::Mutex<BTreeMap<String, CachedDiscordChannel>>;

fn discord_channel_cache() -> &'static DiscordChannelCache {
    static CACHE: OnceLock<DiscordChannelCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn discord_channel(
    client: &reqwest::Client,
    config: &DiscordProxyConfig,
    channel_id: &str,
) -> Result<DiscordChannel, ApiError> {
    {
        let cache = discord_channel_cache().lock().await;
        if let Some(entry) = cache.get(channel_id)
            && entry.fetched_at.elapsed() < DISCORD_CHANNEL_CACHE_TTL
        {
            return Ok(entry.channel.clone());
        }
    }

    let value = discord_api_json(
        client.get(format!("{}/channels/{channel_id}", config.api_url)),
        config,
        "channel lookup",
    )
    .await?;
    let channel = serde_json::from_value::<DiscordChannel>(value).map_err(|error| {
        ApiError::Internal(format!("Discord channel response was invalid: {error}"))
    })?;
    discord_channel_cache().lock().await.insert(
        channel_id.to_owned(),
        CachedDiscordChannel {
            fetched_at: Instant::now(),
            channel: channel.clone(),
        },
    );
    Ok(channel)
}

// No Debug derive: bot_token must not end up in logs via {:?} formatting.
struct DiscordProxyConfig {
    api_url: String,
    bot_token: String,
}

fn discord_proxy_config() -> Result<&'static DiscordProxyConfig, ApiError> {
    static CELL: OnceLock<DiscordProxyConfig> = OnceLock::new();
    if let Some(config) = CELL.get() {
        return Ok(config);
    }
    let config = DiscordProxyConfig::from_env()?;
    Ok(CELL.get_or_init(|| config))
}

impl DiscordProxyConfig {
    fn from_env() -> Result<Self, ApiError> {
        let bot_token = non_empty_env("DISCORD_BOT_TOKEN")
            .ok_or_else(|| ApiError::Internal("DISCORD_BOT_TOKEN is not configured".to_owned()))?;
        Ok(Self {
            api_url: non_empty_env("DISCORD_API_URL")
                .unwrap_or_else(|| DEFAULT_DISCORD_API_URL.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            bot_token,
        })
    }
}

fn authorize_discord_proxy(headers: &HeaderMap) -> Result<DiscordProxyClaims, ApiError> {
    let token = bearer_token(headers)?;
    verify_console_jwt(token)
}

enum DiscordChannelPermission {
    Upload,
    Download,
    History,
}

impl DiscordChannelPermission {
    fn allowed_channels<'a>(&self, claims: &'a DiscordProxyClaims) -> &'a [String] {
        match self {
            Self::Upload => &claims.discord.upload_channels,
            Self::Download => &claims.discord.download_channels,
            Self::History => &claims.discord.history_channels,
        }
    }

    fn forbidden_message(&self) -> &'static str {
        match self {
            Self::Upload => "JWT is not authorized to upload to this Discord channel",
            Self::Download => "JWT is not authorized to download from this Discord channel",
            Self::History => "JWT is not authorized to read history from this Discord channel",
        }
    }
}

async fn ensure_discord_channel_allowed(
    claims: &DiscordProxyClaims,
    client: &reqwest::Client,
    config: &DiscordProxyConfig,
    channel_id: &str,
    permission: DiscordChannelPermission,
) -> Result<(), ApiError> {
    let allowed = permission.allowed_channels(claims);
    if allowed.iter().any(|allowed| allowed == channel_id) {
        return Ok(());
    }

    // A thread inherits the permissions of its parent channel.
    if !allowed.is_empty() {
        let channel = discord_channel(client, config, channel_id).await?;
        if let Some(parent_id) = discord_thread_parent(&channel)
            && allowed.iter().any(|allowed| allowed == parent_id)
        {
            return Ok(());
        }
    }

    Err(ApiError::Forbidden(
        permission.forbidden_message().to_owned(),
    ))
}

fn discord_thread_parent(channel: &DiscordChannel) -> Option<&str> {
    if DISCORD_THREAD_TYPES.contains(&channel.channel_type) {
        channel.parent_id.as_deref()
    } else {
        None
    }
}

fn discord_channel_ids_from_claims(claims: &DiscordProxyClaims) -> Result<Vec<String>, ApiError> {
    let mut channel_ids = BTreeSet::new();
    for channel_id in claims
        .discord
        .upload_channels
        .iter()
        .chain(claims.discord.download_channels.iter())
        .chain(claims.discord.history_channels.iter())
    {
        validate_discord_id(channel_id, "channel")?;
        channel_ids.insert(channel_id.to_owned());
    }
    Ok(channel_ids.into_iter().collect())
}

fn discord_channel_item(
    claims: &DiscordProxyClaims,
    channel_id: &str,
    channel: &DiscordChannel,
) -> DiscordChannelItem {
    let granted = |channels: &[String]| channels.iter().any(|allowed| allowed == channel_id);
    DiscordChannelItem {
        id: channel_id.to_owned(),
        name: channel
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| channel_id.to_owned()),
        guild_id: channel.guild_id.clone().unwrap_or_default(),
        topic: channel.topic.clone().unwrap_or_default(),
        can_upload: granted(&claims.discord.upload_channels),
        can_download: granted(&claims.discord.download_channels),
        can_read_history: granted(&claims.discord.history_channels),
    }
}

fn validate_discord_id(value: &str, kind: &str) -> Result<(), ApiError> {
    if (17..=20).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    Err(ApiError::BadRequest(format!("invalid Discord {kind} ID")))
}

fn validate_discord_messages_query(query: &DiscordMessagesQuery) -> Result<(), ApiError> {
    if let Some(limit) = query.limit
        && !(1..=MAX_DISCORD_MESSAGES_LIMIT).contains(&limit)
    {
        return Err(ApiError::BadRequest(format!(
            "Discord messages limit must be between 1 and {MAX_DISCORD_MESSAGES_LIMIT}"
        )));
    }
    if query.before.is_some() && query.after.is_some() {
        return Err(ApiError::BadRequest(
            "Discord messages accept before or after, not both".to_owned(),
        ));
    }
    for message_id in [query.before.as_deref(), query.after.as_deref()]
        .into_iter()
        .flatten()
    {
        validate_discord_id(message_id, "message")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use axum::{body::to_bytes, extract::RawQuery};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

    fn test_jwt(secret: &[u8], claims: Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    fn test_claims(upload: &[&str], download: &[&str], history: &[&str]) -> DiscordProxyClaims {
        let owned = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect();
        DiscordProxyClaims {
            discord: DiscordChannelClaims {
                upload_channels: owned(upload),
                download_channels: owned(download),
                history_channels: owned(history),
            },
        }
    }

    async fn serve(app: axum::Router) -> (DiscordProxyConfig, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = DiscordProxyConfig {
            api_url: format!("http://{address}"),
            bot_token: "test-token".to_owned(),
        };
        (config, server)
    }

    #[test]
    fn verifies_hs256_jwt_and_separate_discord_channel_claims() {
        let token = test_jwt(
            b"secret",
            json!({
                "iss": "centaur-console",
                "sub": "prn_123",
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
                "discord": {
                    "upload_channels": ["100000000000000001"],
                    "download_channels": ["100000000000000002"],
                    "history_channels": ["100000000000000003"]
                }
            }),
        );
        let claims = crate::api_jwt::verify_hs256_jwt::<DiscordProxyClaims>(
            &token,
            b"secret",
            "centaur-api",
            "centaur-console",
        )
        .unwrap();
        assert_eq!(claims.discord.upload_channels, ["100000000000000001"]);
        assert_eq!(claims.discord.download_channels, ["100000000000000002"]);
        assert_eq!(claims.discord.history_channels, ["100000000000000003"]);
    }

    #[test]
    fn tokens_without_a_discord_claim_grant_no_channels() {
        let token = test_jwt(
            b"secret",
            json!({
                "iss": "centaur-console",
                "sub": "prn_123",
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
                "slack": { "upload_channels": ["C123456789"] }
            }),
        );
        let claims = crate::api_jwt::verify_hs256_jwt::<DiscordProxyClaims>(
            &token,
            b"secret",
            "centaur-api",
            "centaur-console",
        )
        .unwrap();
        assert!(claims.discord.upload_channels.is_empty());
        assert!(claims.discord.download_channels.is_empty());
        assert!(claims.discord.history_channels.is_empty());
    }

    #[test]
    fn rejects_invalid_jwt_signature() {
        let token = test_jwt(
            b"secret",
            json!({
                "iss": "centaur-console",
                "sub": "prn_123",
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
                "discord": { "upload_channels": ["100000000000000001"] }
            }),
        );
        assert!(matches!(
            crate::api_jwt::verify_hs256_jwt::<DiscordProxyClaims>(
                &token,
                b"other-secret",
                "centaur-api",
                "centaur-console"
            )
            .unwrap_err(),
            ApiError::Unauthorized(_)
        ));
    }

    #[tokio::test]
    async fn channel_listing_requires_jwt() {
        let result = get_discord_channels(HeaderMap::new()).await;

        assert!(matches!(result, Err(ApiError::Unauthorized(_))));
    }

    #[test]
    fn extracts_deduped_channel_ids_from_all_discord_claims() {
        let claims = test_claims(
            &["100000000000000002"],
            &["100000000000000001", "100000000000000002"],
            &["100000000000000003"],
        );

        assert_eq!(
            discord_channel_ids_from_claims(&claims).unwrap(),
            [
                "100000000000000001",
                "100000000000000002",
                "100000000000000003"
            ]
        );
        assert!(matches!(
            discord_channel_ids_from_claims(&test_claims(&["general"], &[], &[])).unwrap_err(),
            ApiError::BadRequest(_)
        ));
    }

    #[test]
    fn channel_item_reports_each_permission_flag() {
        let claims = test_claims(&["100000000000000001"], &[], &["100000000000000001"]);
        let channel = serde_json::from_value::<DiscordChannel>(json!({
            "id": "100000000000000001",
            "type": 0,
            "name": "general",
            "guild_id": "100000000000000000",
            "topic": null
        }))
        .unwrap();

        let item = discord_channel_item(&claims, "100000000000000001", &channel);

        assert_eq!(item.name, "general");
        assert_eq!(item.guild_id, "100000000000000000");
        assert_eq!(item.topic, "");
        assert!(item.can_upload);
        assert!(!item.can_download);
        assert!(item.can_read_history);
    }

    #[tokio::test]
    async fn threads_inherit_their_parent_channel_permissions() {
        let lookups = Arc::new(AtomicUsize::new(0));
        let handler_lookups = lookups.clone();
        let app = axum::Router::new().route(
            "/channels/{channel_id}",
            get(move |Path(channel_id): Path<String>| {
                let lookups = handler_lookups.clone();
                async move {
                    lookups.fetch_add(1, Ordering::SeqCst);
                    let channel = match channel_id.as_str() {
                        "200000000000000011" => {
                            json!({ "type": 11, "parent_id": "200000000000000001" })
                        }
                        "200000000000000012" => {
                            json!({ "type": 12, "parent_id": "200000000000000002" })
                        }
                        // A text channel's parent is its category, not a grant.
                        _ => json!({ "type": 0, "parent_id": "200000000000000001" }),
                    };
                    axum::Json(channel)
                }
            }),
        );
        let (config, server) = serve(app).await;
        let claims = test_claims(&["200000000000000001"], &[], &["200000000000000001"]);
        let allowed = |channel_id: &'static str, permission| {
            ensure_discord_channel_allowed(&claims, http_client(), &config, channel_id, permission)
        };

        allowed("200000000000000001", DiscordChannelPermission::Upload)
            .await
            .unwrap();
        assert_eq!(lookups.load(Ordering::SeqCst), 0);

        allowed("200000000000000011", DiscordChannelPermission::Upload)
            .await
            .unwrap();
        allowed("200000000000000011", DiscordChannelPermission::History)
            .await
            .unwrap();
        assert_eq!(lookups.load(Ordering::SeqCst), 1);

        for channel_id in ["200000000000000012", "200000000000000021"] {
            assert!(matches!(
                allowed(channel_id, DiscordChannelPermission::Upload)
                    .await
                    .unwrap_err(),
                ApiError::Forbidden(_)
            ));
        }
        assert_eq!(lookups.load(Ordering::SeqCst), 3);

        assert!(matches!(
            allowed("200000000000000011", DiscordChannelPermission::Download)
                .await
                .unwrap_err(),
            ApiError::Forbidden(_)
        ));
        assert_eq!(lookups.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn message_list_sends_the_bot_token_and_paging_query() {
        let seen = Arc::new(Mutex::new(None));
        let handler_seen = seen.clone();
        let app = axum::Router::new().route(
            "/channels/{channel_id}/messages",
            get(move |headers: HeaderMap, RawQuery(query): RawQuery| {
                let seen = handler_seen.clone();
                async move {
                    *seen.lock().unwrap() = Some((
                        headers[header::AUTHORIZATION].to_str().unwrap().to_owned(),
                        headers[header::USER_AGENT].to_str().unwrap().to_owned(),
                        query,
                    ));
                    axum::Json(json!([{ "id": "300000000000000008", "content": "hi" }]))
                }
            }),
        );
        let (config, server) = serve(app).await;
        let query = DiscordMessagesQuery {
            limit: Some(5),
            before: Some("300000000000000009".to_owned()),
            after: None,
        };

        let messages = discord_messages(http_client(), &config, "300000000000000001", &query)
            .await
            .unwrap();

        assert_eq!(
            messages,
            json!([{ "id": "300000000000000008", "content": "hi" }])
        );
        let (authorization, user_agent, query) = seen.lock().unwrap().take().unwrap();
        assert_eq!(authorization, "Bot test-token");
        assert_eq!(user_agent, DISCORD_USER_AGENT);
        assert_eq!(query.as_deref(), Some("limit=5&before=300000000000000009"));
        server.abort();
    }

    #[tokio::test]
    async fn discord_rate_limits_pass_through_without_retrying() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = attempts.clone();
        let app = axum::Router::new().route(
            "/channels/{channel_id}/messages",
            get(move || {
                let attempts = handler_attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "3")],
                        axum::Json(
                            json!({ "message": "You are being rate limited.", "retry_after": 2.5 }),
                        ),
                    )
                }
            }),
        );
        let (config, server) = serve(app).await;
        let query = DiscordMessagesQuery {
            limit: None,
            before: None,
            after: None,
        };

        let error = discord_messages(http_client(), &config, "300000000000000002", &query)
            .await
            .unwrap_err();
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "3");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn upstream_errors_keep_the_discord_message() {
        let app = axum::Router::new().route(
            "/channels/{channel_id}/messages/{message_id}",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({ "message": "Unknown Message", "code": 10008 })),
                )
            }),
        );
        let (config, server) = serve(app).await;

        let error = discord_message(
            http_client(),
            &config,
            "300000000000000003",
            "300000000000000004",
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            ApiError::BadRequest(message)
                if message == "Discord message lookup failed with status 404: Unknown Message"
        ));
        server.abort();
    }

    #[test]
    fn message_payload_builds_a_same_channel_reply() {
        assert_eq!(
            Value::Object(discord_message_payload(
                Some("hi"),
                Some("400000000000000001"),
                true
            )),
            json!({
                "content": "hi",
                "message_reference": { "message_id": "400000000000000001" },
                "flags": 4
            })
        );
        assert_eq!(
            Value::Object(discord_message_payload(None, None, false)),
            json!({})
        );
    }

    #[tokio::test]
    async fn file_upload_sends_payload_json_and_the_file_part() {
        let seen = Arc::new(Mutex::new(None));
        let handler_seen = seen.clone();
        let app = axum::Router::new().route(
            "/channels/{channel_id}/messages",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let seen = handler_seen.clone();
                async move {
                    *seen.lock().unwrap() = Some((
                        headers[header::CONTENT_TYPE].to_str().unwrap().to_owned(),
                        String::from_utf8_lossy(&body).into_owned(),
                    ));
                    axum::Json(json!({ "id": "500000000000000009" }))
                }
            }),
        );
        let (config, server) = serve(app).await;
        let query = DiscordFileUploadQuery {
            filename: "notes.txt".to_owned(),
            content: Some("see attached".to_owned()),
            reply_to: Some("500000000000000002".to_owned()),
            content_type: Some("text/plain".to_owned()),
        };

        let message = upload_discord_file_bytes(
            http_client(),
            &config,
            "500000000000000001",
            &query,
            Body::from("file bytes"),
            10,
        )
        .await
        .unwrap();

        assert_eq!(message["id"], "500000000000000009");
        let (content_type, body) = seen.lock().unwrap().take().unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary="));
        let payload_part = body
            .split_once(r#"name="payload_json""#)
            .and_then(|(_, rest)| rest.split_once("\r\n\r\n"))
            .and_then(|(_, rest)| rest.split_once("\r\n--"))
            .map(|(payload, _)| payload)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(payload_part).unwrap(),
            json!({
                "content": "see attached",
                "message_reference": { "message_id": "500000000000000002" },
                "attachments": [{ "id": 0, "filename": "notes.txt" }]
            })
        );
        assert!(body.contains(r#"name="files[0]"; filename="notes.txt""#));
        assert!(
            body.to_ascii_lowercase()
                .contains("content-type: text/plain")
        );
        assert!(body.contains("file bytes"));
        server.abort();
    }

    async fn message_with_attachment(
        Path((channel_id, message_id)): Path<(String, String)>,
        headers: HeaderMap,
    ) -> Response {
        if (channel_id.as_str(), message_id.as_str())
            != ("600000000000000001", "600000000000000002")
        {
            return StatusCode::NOT_FOUND.into_response();
        }
        let host = headers[header::HOST].to_str().unwrap();
        axum::Json(json!({
            "id": message_id,
            "attachments": [{
                "id": "600000000000000003",
                "filename": "report.pdf",
                "size": 7,
                "url": format!("http://{host}/attachments/report.pdf"),
                "content_type": "application/pdf"
            }]
        }))
        .into_response()
    }

    #[tokio::test]
    async fn attachment_download_streams_the_file_of_a_message_in_the_channel() {
        let app = axum::Router::new()
            .route(
                "/channels/{channel_id}/messages/{message_id}",
                get(message_with_attachment),
            )
            .route(
                "/attachments/report.pdf",
                get(|headers: HeaderMap| async move {
                    // The bot token must never reach the CDN.
                    if headers.contains_key(header::AUTHORIZATION) {
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                    "%PDF-1.".into_response()
                }),
            );
        let (config, server) = serve(app).await;

        let response = discord_attachment_response(
            http_client(),
            &config,
            "600000000000000001",
            "600000000000000002",
            "600000000000000003",
        )
        .await
        .unwrap();

        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/pdf");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"%PDF-1.");
        server.abort();
    }

    #[test]
    fn attachment_lookup_requires_the_attachment_and_enforces_the_size_cap() {
        let message = json!({
            "attachments": [
                {
                    "id": "600000000000000011",
                    "filename": "huge.bin",
                    "size": MAX_DISCORD_FILE_BYTES + 1,
                    "url": "https://cdn.discordapp.com/attachments/1/2/huge.bin"
                }
            ]
        });

        assert!(matches!(
            discord_attachment(&message, "600000000000000011").unwrap_err(),
            ApiError::PayloadTooLarge(_)
        ));
        assert!(matches!(
            discord_attachment(&message, "600000000000000012").unwrap_err(),
            ApiError::NotFound(_)
        ));
        assert!(matches!(
            discord_attachment(&json!({}), "600000000000000011").unwrap_err(),
            ApiError::NotFound(_)
        ));
    }

    #[test]
    fn validates_discord_ids_and_message_queries() {
        validate_discord_id("12345678901234567", "channel").unwrap();
        validate_discord_id("12345678901234567890", "channel").unwrap();
        for value in [
            "",
            "1234567890123456",
            "123456789012345678901",
            "C123456789",
            "1234567890123456a",
            "../1234567890123456",
        ] {
            assert!(matches!(
                validate_discord_id(value, "channel").unwrap_err(),
                ApiError::BadRequest(_)
            ));
        }

        let query = |limit, before: Option<&str>, after: Option<&str>| DiscordMessagesQuery {
            limit,
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
        };
        validate_discord_messages_query(&query(Some(100), Some("12345678901234567"), None))
            .unwrap();
        for invalid in [
            query(Some(0), None, None),
            query(Some(101), None, None),
            query(None, Some("12345678901234567"), Some("12345678901234568")),
            query(None, None, Some("not-a-message")),
        ] {
            assert!(matches!(
                validate_discord_messages_query(&invalid).unwrap_err(),
                ApiError::BadRequest(_)
            ));
        }
    }
}
