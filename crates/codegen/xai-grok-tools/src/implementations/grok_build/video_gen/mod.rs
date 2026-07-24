//! Video generation module. Hosts the shared [`VideoGenClient`] and the
//! `image_to_video` and `reference_to_video` tools, which generate videos via
//! the xAI Video Generation API and save them to the local filesystem so the
//! model can reference them in code (e.g. `<video src="videos/hero.mp4">`).
//!
//! Architecture follows the same pattern as `image_gen`:
//!
//! - [`VideoGenConfig`] is built from session credentials by the host and
//!   injected into the tool registry.
//! - When `Enabled`, a [`VideoGenClient`] is constructed once and injected
//!   into `Resources`. The tools read it at runtime via `resources.require()`.
//! - When `Disabled`, the tools are not registered so the model never sees them.
//!
//! The generated video is written to `<session_folder>/videos/<n>.mp4`
//! where `<n>` is a session-scoped counter (1, 2, 3, ... — 1 token each).
//! The tools return the absolute path so the model can copy or move the
//! video into the project working directory when it needs a persistent asset.
//!
//! Video generation is asynchronous:
//! 1. POST to `/v1/videos/generations` → receive a `request_id`
//! 2. Poll GET `/v1/videos/{request_id}` until status is `"done"`
//! 3. Download video bytes from the API URL, or an optional presigned GET URL

use base64::Engine as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::Deserialize;

use crate::attribution::{SharedAttributionCallback, ToolConsumer};
use crate::types::SharedApiKeyProvider;

use crate::types::output::{MediaGenOutput, ToolOutput};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::SessionFolder;
use crate::types::tool::{ToolKind, ToolNamespace};

const XAI_VIDEO_BASE_MODEL: &str = "grok-imagine-video";
const XAI_VIDEO_QUALITY_MODEL: &str = "grok-imagine-video-1.5-preview";
const VIDEO_START_TIMEOUT_SECS: u64 = 60;
const VIDEO_GEN_TIMEOUT_SECS: u64 = 300;
const VIDEO_POLL_INTERVAL_SECS: u64 = 5;
const VIDEO_POLL_REQUEST_TIMEOUT_SECS: u64 = 30;
const VIDEO_DOWNLOAD_TIMEOUT_SECS: u64 = 120;
const DEFAULT_ZDR_VIDEO_PRESIGN_EXPIRES_SECS: u64 = 900;
/// Presign at request start; must survive generation poll + local download.
const MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS: u64 =
    VIDEO_GEN_TIMEOUT_SECS + VIDEO_DOWNLOAD_TIMEOUT_SECS + 60;
const DEFAULT_ZDR_VIDEO_KEY_PREFIX: &str = "grok-videos/";
const ZDR_VIDEO_CONTENT_TYPE: &str = "video/mp4";
const DEFAULT_VIDEO_DIR: &str = "videos";
const DEFAULT_RESOLUTION: &str = "480p";
const DEFAULT_IMAGINE_VIDEO_DURATION_SECS: u32 = 6;
const MAX_R2V_REFERENCE_IMAGES: usize = 7;
const VALID_IMAGINE_VIDEO_ASPECT_RATIOS: &[&str] = &["1:1", "16:9", "9:16", "3:2", "2:3"];
const VALID_VIDEO_RESOLUTIONS: &[&str] = &["480p", "720p"];
const IMAGINE_VIDEO_DURATIONS_SECS: &[u32] = &[6, 10];

pub use xai_grok_tools_api::slash_commands::{
    IMAGE_TO_VIDEO_TOOL_NAME, IMAGINE_VIDEO_COMMAND_NAME, imagine_video_instruction,
    imagine_video_usage_message,
};

pub const REFERENCE_TO_VIDEO_TOOL_NAME: &str = "reference_to_video";

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct S3AccessCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl S3AccessCredentials {
    fn is_valid(&self) -> bool {
        !self.access_key_id.trim().is_empty() && !self.secret_access_key.trim().is_empty()
    }

    fn to_static(&self) -> xai_file_utils::s3::S3StaticCredentials {
        xai_file_utils::s3::S3StaticCredentials {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
        }
    }
}

impl std::fmt::Debug for S3AccessCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3AccessCredentials")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct ZdrVideoOutputS3Config {
    pub bucket: String,
    pub endpoint: String,
    pub region: String,
    #[serde(default = "default_zdr_video_key_prefix")]
    pub key_prefix: String,
    #[serde(default = "default_zdr_video_presign_expires_secs")]
    pub expires_secs: u64,
    pub read_write: S3AccessCredentials,
    #[serde(default)]
    pub read_only: Option<S3AccessCredentials>,
}

fn default_zdr_video_key_prefix() -> String {
    DEFAULT_ZDR_VIDEO_KEY_PREFIX.to_owned()
}

fn default_zdr_video_presign_expires_secs() -> u64 {
    DEFAULT_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
}

impl ZdrVideoOutputS3Config {
    pub fn is_valid(&self) -> bool {
        !self.bucket.trim().is_empty()
            && !self.endpoint.trim().is_empty()
            && !self.region.trim().is_empty()
            && self.read_write.is_valid()
    }
}

impl std::fmt::Debug for ZdrVideoOutputS3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZdrVideoOutputS3Config")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("key_prefix", &self.key_prefix)
            .field("expires_secs", &self.expires_secs)
            .field("read_write", &self.read_write)
            .field("read_only", &self.read_only.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// HTTP client for xAI Video Generation API. Cloned per-request; shares `Arc` state.
#[derive(Clone)]
pub struct VideoGenClient {
    http: reqwest::Client,
    download_http: reqwest::Client,
    base_url: String,
    writer: super::storage::SessionFileWriter,
    zdr_video_output_s3: Option<ZdrVideoOutputS3Config>,
    api_key_provider: Option<SharedApiKeyProvider>,
    /// Optional 401-attribution hook. Hosts wire this so a 401 from the
    /// Video Generation API emits an `auth_401_attribution` event with
    /// `consumer` of `"VideoGen.start"` (start request) or
    /// `"VideoGen.poll"` (poll request) for unified auth-failure telemetry.
    attribution_callback: Option<SharedAttributionCallback>,
    /// When `true`, the user is on a tier the Imagine server zero-limits
    /// (free / X Basic). The video tools short-circuit before any HTTP call
    /// and return the SuperGrok upsell prose. See [`VideoGenClient::is_tier_restricted`].
    tier_restricted: bool,
}

impl VideoGenClient {
    pub fn new(
        config: &VideoGenConfig,
        api_key_provider: Option<SharedApiKeyProvider>,
    ) -> Result<Self, xai_tool_runtime::ToolError> {
        let VideoGenConfig::Enabled {
            api_key,
            base_url,
            extra_headers,
            zdr_video_output_s3,
            tier_restricted,
        } = config
        else {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "Cannot create VideoGenClient from disabled config",
            ));
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Always bake the static api_key as the default Authorization header.
        // The dynamic provider overrides per-request; this is the fallback.
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Invalid API key for header: {e}"
                ))
            })?,
        );

        extra_headers.into_iter().try_for_each(|(key, value)| {
            let header_name =
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                    xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Invalid header name '{key}': {e}"
                    ))
                })?;
            let header_value = HeaderValue::from_str(value).map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Invalid header value for '{key}': {e}"
                ))
            })?;
            headers.insert(header_name, header_value);
            Ok::<(), xai_tool_runtime::ToolError>(())
        })?;

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Failed to build HTTP client: {e}"
                ))
            })?;

        let download_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(VIDEO_DOWNLOAD_TIMEOUT_SECS))
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Failed to build download client: {e}"
                ))
            })?;

        Ok(Self {
            http,
            download_http,
            base_url: base_url.clone(),
            writer: super::storage::SessionFileWriter::new(DEFAULT_VIDEO_DIR, "mp4"),
            zdr_video_output_s3: zdr_video_output_s3
                .as_ref()
                .map(|c| (**c).clone())
                .filter(ZdrVideoOutputS3Config::is_valid),
            api_key_provider,
            attribution_callback: None,
            tier_restricted: *tier_restricted,
        })
    }

    /// Whether the current user's tier (free / X Basic) is zero-limited on
    /// Imagine server-side. The video tools use this to short-circuit with the
    /// SuperGrok upsell instead of issuing a doomed request.
    pub(crate) fn is_tier_restricted(&self) -> bool {
        self.tier_restricted
    }

    /// Wire a 401-attribution callback into this client. Idempotent;
    /// safe to call before or after the first request.
    pub fn with_attribution_callback(
        mut self,
        callback: Option<SharedAttributionCallback>,
    ) -> Self {
        self.attribution_callback = callback;
        self
    }

    async fn current_bearer(&self) -> Option<String> {
        crate::types::api_key_provider::resolve_bearer(self.api_key_provider.as_ref()).await
    }

    fn record_401_attribution(&self, consumer: ToolConsumer, sent_bearer: Option<&str>) {
        crate::attribution::emit_401(self.attribution_callback.as_ref(), consumer, sent_bearer);
    }

    pub async fn generate_with_images(
        &self,
        model: &'static str,
        prompt: &str,
        duration: Option<u32>,
        aspect_ratio: Option<&str>,
        resolution: &str,
        image: Option<String>,
        reference_images: Vec<String>,
    ) -> Result<VideoOutcome, xai_tool_runtime::ToolError> {
        let start_url = format!("{}/videos/generations", self.base_url.trim_end_matches('/'));

        let presigned = match &self.zdr_video_output_s3 {
            Some(config) => Some(self.presign_zdr_output_urls(config).await?),
            None => None,
        };

        let payload = GenerateVideoPayload {
            model,
            prompt,
            image: image.map(|url| VideoImageUrl { url }),
            duration,
            aspect_ratio,
            resolution,
            reference_images: reference_images
                .into_iter()
                .map(|url| VideoImageUrl { url })
                .collect(),
            output: presigned.as_ref().map(|urls| VideoOutput {
                upload_url: urls.upload_url.clone(),
            }),
        };

        let sent_bearer = self.current_bearer().await;
        let mut req = self
            .http
            .post(&start_url)
            .timeout(std::time::Duration::from_secs(VIDEO_START_TIMEOUT_SECS))
            .json(&payload);
        if let Some(ref key) = sent_bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {key}"));
        }

        let response = req.send().await.map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Video generation API request failed: {e}"
            ))
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.record_401_attribution(ToolConsumer::VideoGenStart, sent_bearer.as_deref());
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(200).collect();
            tracing::warn!(http_status = %status, "Video generation API error: {truncated}");
            return Err(xai_tool_runtime::ToolError::new(
                xai_tool_runtime::ToolErrorKind::Custom,
                format!("Video generation failed with HTTP {status}: {truncated}"),
            )
            .with_details(serde_json::json!({"code": "http_failure", "status": status.as_u16()})));
        }

        let body = response.text().await.map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to read video generation start response body: {e}"
            ))
        })?;

        let start_resp: VideoGenStartResponse = serde_json::from_str(&body).map_err(|e| {
            let preview: String = body.chars().take(500).collect();
            tracing::warn!("Video generation API returned unparseable body: {preview}");
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to parse video generation start response: {e} — body preview: {preview}"
            ))
        })?;

        let request_id = start_resp.request_id;
        if request_id.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "No request_id received from the video generation API.",
            ));
        }

        tracing::info!(request_id = %request_id, "Video generation started, polling for completion");

        let poll_url = format!(
            "{}/videos/{}",
            self.base_url.trim_end_matches('/'),
            request_id
        );
        let poll_timeout = std::time::Duration::from_secs(VIDEO_POLL_REQUEST_TIMEOUT_SECS);
        let poll_interval = std::time::Duration::from_secs(VIDEO_POLL_INTERVAL_SECS);
        let deadline = std::time::Duration::from_secs(VIDEO_GEN_TIMEOUT_SECS);
        let started = tokio::time::Instant::now();

        loop {
            tokio::time::sleep(poll_interval).await;

            if started.elapsed() >= deadline {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Video generation did not complete within {}s (request_id={request_id})",
                    VIDEO_GEN_TIMEOUT_SECS
                )));
            }

            let poll_sent_bearer = self.current_bearer().await;
            let mut poll_req = self.http.get(&poll_url).timeout(poll_timeout);
            if let Some(ref key) = poll_sent_bearer {
                poll_req = poll_req.header(AUTHORIZATION, format!("Bearer {key}"));
            }

            let poll_response = poll_req.send().await.map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Video poll request failed: {e}"
                ))
            })?;

            let poll_status = poll_response.status();
            if poll_status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    ToolConsumer::VideoGenPoll,
                    poll_sent_bearer.as_deref(),
                );
            }
            if !poll_status.is_success() && poll_status.as_u16() != 202 {
                let body = poll_response.text().await.unwrap_or_default();
                let truncated: String = body.chars().take(200).collect();
                return Err(xai_tool_runtime::ToolError::new(
                    xai_tool_runtime::ToolErrorKind::Custom,
                    format!("Video poll failed with HTTP {poll_status}: {truncated}"),
                )
                .with_details(
                    serde_json::json!({"code": "http_failure", "status": poll_status.as_u16()}),
                ));
            }

            let poll_body = poll_response.text().await.map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Failed to read video poll response body: {e}"
                ))
            })?;

            let poll_data: VideoGenPollResponse =
                serde_json::from_str(&poll_body).map_err(|e| {
                    let preview: String = poll_body.chars().take(500).collect();
                    tracing::warn!("Video poll API returned unparseable body: {preview}");
                    xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Failed to parse video poll response: {e} — body preview: {preview}"
                    ))
                })?;

            match poll_data.status.as_str() {
                "done" => {
                    let video_url = poll_data.video.and_then(|v| v.url).unwrap_or_default();
                    tracing::info!(
                        request_id = %request_id,
                        elapsed_secs = started.elapsed().as_secs(),
                        "Video generation completed"
                    );
                    return match presigned {
                        Some(urls) => self.finish_zdr_video(&request_id, urls).await,
                        None if video_url.is_empty() => {
                            Err(xai_tool_runtime::ToolError::invalid_arguments(
                                "Video generation completed but no download URL was returned.",
                            ))
                        }
                        None => self
                            .download_video(&video_url)
                            .await
                            .map(VideoOutcome::Bytes),
                    };
                }
                "failed" => {
                    let preview: String = poll_body.chars().take(300).collect();
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Video generation failed on the server (request_id={request_id}): {preview}"
                    )));
                }
                "expired" => {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Video generation request expired (request_id={request_id})."
                    )));
                }
                other => {
                    tracing::debug!(
                        status = %other,
                        elapsed_secs = started.elapsed().as_secs(),
                        "Video generation still in progress"
                    );
                }
            }
        }
    }

    /// Download video bytes from a pre-signed temporary URL (no auth headers).
    async fn download_video(&self, url: &str) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        let response = self.download_http.get(url).send().await.map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!("Failed to download video: {e}"))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(xai_tool_runtime::ToolError::new(
                xai_tool_runtime::ToolErrorKind::Custom,
                format!("Video download failed (HTTP {status})"),
            )
            .with_details(serde_json::json!({"code": "http_failure", "status": status.as_u16()})));
        }

        response.bytes().await.map(|b| b.to_vec()).map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to read video bytes: {e}"
            ))
        })
    }

    async fn finish_zdr_video(
        &self,
        request_id: &str,
        urls: ZdrPresignedUrls,
    ) -> Result<VideoOutcome, xai_tool_runtime::ToolError> {
        let config = self.zdr_video_output_s3.as_ref().ok_or_else(|| {
            xai_tool_runtime::ToolError::invalid_arguments(
                "Presigned video output config missing after presign",
            )
        })?;

        // A presigned GET means the client must download locally. Propagate
        // failures instead of silently treating the run as upload-only success.
        if let Some(get_url) = urls.get_url.as_deref() {
            let bytes = self.download_video(get_url).await.map_err(|e| {
                tracing::warn!(
                    request_id = %request_id,
                    "Presigned video download failed (GET URL was minted): {e}"
                );
                e
            })?;
            return Ok(VideoOutcome::Bytes(bytes));
        }

        // No pre-minted GET URL — retry presign (may succeed now that the
        // object exists) and attempt a local download before falling back to
        // a remote reference URL for the model.
        match self.presign_and_download(config, &urls, request_id).await {
            Ok(bytes) => Ok(VideoOutcome::Bytes(bytes)),
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    "Post-upload video download failed, returning remote reference: {e}"
                );
                let reference_url = self.zdr_reference_url(config, &urls).await?;
                Ok(VideoOutcome::UploadedUrl(reference_url))
            }
        }
    }

    async fn presign_zdr_output_urls(
        &self,
        config: &ZdrVideoOutputS3Config,
    ) -> Result<ZdrPresignedUrls, xai_tool_runtime::ToolError> {
        let object_key = zdr_video_object_key(&config.key_prefix);
        let expires_in =
            std::time::Duration::from_secs(zdr_presign_expires_secs(config.expires_secs));
        let endpoint = Some(config.endpoint.as_str());

        let upload_url = xai_file_utils::s3::presign_put_url(
            &config.region,
            endpoint,
            &config.read_write.to_static(),
            &config.bucket,
            &object_key,
            ZDR_VIDEO_CONTENT_TYPE,
            expires_in,
        )
        .await
        .map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to presign video upload URL: {e}"
            ))
        })?;

        if !is_http_url(&upload_url) {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Presigned upload URL is not http(s): {upload_url}"
            )));
        }

        let get_url = match self
            .presign_zdr_get_url(config, &object_key, expires_in)
            .await
        {
            Ok(url) => Some(url),
            Err(e) => {
                tracing::warn!(
                    "Video GET presign failed before generation; will retry download after upload completes: {e}"
                );
                None
            }
        };

        Ok(ZdrPresignedUrls {
            object_key,
            upload_url,
            get_url,
            expires_in,
        })
    }

    /// Re-presign a GET URL after generation and attempt a local download.
    async fn presign_and_download(
        &self,
        config: &ZdrVideoOutputS3Config,
        urls: &ZdrPresignedUrls,
        request_id: &str,
    ) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        let get_url = self
            .presign_zdr_get_url(config, &urls.object_key, urls.expires_in)
            .await?;
        tracing::info!(
            request_id = %request_id,
            "Post-upload video GET presign succeeded, attempting download"
        );
        self.download_video(&get_url).await
    }

    async fn zdr_reference_url(
        &self,
        config: &ZdrVideoOutputS3Config,
        urls: &ZdrPresignedUrls,
    ) -> Result<String, xai_tool_runtime::ToolError> {
        if let Some(get_url) = urls.get_url.as_deref().filter(|u| is_http_url(u)) {
            return Ok(get_url.to_owned());
        }
        self.presign_zdr_get_url(config, &urls.object_key, urls.expires_in)
            .await
    }

    async fn presign_zdr_get_url(
        &self,
        config: &ZdrVideoOutputS3Config,
        object_key: &str,
        expires_in: std::time::Duration,
    ) -> Result<String, xai_tool_runtime::ToolError> {
        let endpoint = Some(config.endpoint.as_str());
        let (creds, creds_source) = zdr_get_credentials(config);
        let url = xai_file_utils::s3::presign_get_url(
            &config.region,
            endpoint,
            &creds.to_static(),
            &config.bucket,
            object_key,
            expires_in,
        )
        .await
        .map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to presign video GET URL ({creds_source}): {e}"
            ))
        })?;

        if !is_http_url(&url) {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Presigned GET URL is not http(s): {url}"
            )));
        }
        Ok(url)
    }
}

fn zdr_presign_expires_secs(configured: u64) -> u64 {
    configured.max(MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS)
}

fn zdr_get_credentials(config: &ZdrVideoOutputS3Config) -> (&S3AccessCredentials, &'static str) {
    if let Some(read_only) = config.read_only.as_ref() {
        if read_only.is_valid() {
            return (read_only, "read_only");
        }
        tracing::warn!(
            "tools.zdr_video_output_s3.read_only is incomplete; falling back to read_write for GET presign"
        );
    }
    (&config.read_write, "read_write")
}

fn zdr_video_object_key(prefix: &str) -> String {
    let prefix = prefix.trim();
    let object_id = uuid::Uuid::new_v4();
    if prefix.is_empty() {
        format!("{object_id}.mp4")
    } else {
        let normalized = if prefix.ends_with('/') {
            prefix.to_owned()
        } else {
            format!("{prefix}/")
        };
        format!("{normalized}{object_id}.mp4")
    }
}

fn is_http_url(raw: &str) -> bool {
    url::Url::parse(raw)
        .map(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or(false)
}

/// Session-level configuration. Same shape as [`ImageGenConfig`].
///
/// [`ImageGenConfig`]: super::image_gen::ImageGenConfig
#[derive(Debug, Clone, Default)]
pub enum VideoGenConfig {
    #[default]
    Disabled,
    Enabled {
        api_key: String,
        base_url: String,
        extra_headers: indexmap::IndexMap<String, String>,
        zdr_video_output_s3: Option<Box<ZdrVideoOutputS3Config>>,
        /// `true` when the user is on a tier the Imagine server zero-limits
        /// (free / X Basic). The video tools stay advertised but short-circuit
        /// at call time with the SuperGrok upsell prose. Set by the host from
        /// the subscription tier; always `false` for team / API-key / workspace.
        tier_restricted: bool,
    },
}

impl VideoGenConfig {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    /// Stamp [`super::image_gen::SESSION_ID_HEADER`] onto `extra_headers`.
    /// A caller-provided value is never overwritten. No-op when `Disabled`.
    pub fn stamp_session_id_header(&mut self, session_id: &str) {
        if let Self::Enabled { extra_headers, .. } = self {
            extra_headers
                .entry(super::image_gen::SESSION_ID_HEADER.to_string())
                .or_insert_with(|| session_id.to_string());
        }
    }
}

/// Prose returned to the model (as a normal, successful tool result) when a
/// free / X Basic user calls a video tool. The model relays it to the user;
/// the deliberate `/imagine-video` slash command shows the SuperGrok upsell
/// modal instead.
pub(crate) const TIER_RESTRICTED_UPSELL: &str = "Video generation is a SuperGrok feature and isn't available on the free or X Basic tier. Let the user know they can unlock image and video generation by upgrading to SuperGrok: https://grok.com/supergrok?referrer=grok-build. Do not retry this tool.";

fn default_resolution_name() -> String {
    DEFAULT_RESOLUTION.to_owned()
}

pub enum VideoOutcome {
    Bytes(Vec<u8>),
    UploadedUrl(String),
}

#[derive(serde::Serialize)]
struct GenerateVideoPayload<'a> {
    model: &'static str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<VideoImageUrl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<&'a str>,
    resolution: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reference_images: Vec<VideoImageUrl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<VideoOutput>,
}

#[derive(serde::Serialize)]
struct VideoImageUrl {
    url: String,
}

#[derive(serde::Serialize)]
struct VideoOutput {
    upload_url: String,
}

struct ZdrPresignedUrls {
    object_key: String,
    upload_url: String,
    get_url: Option<String>,
    /// Cached TTL for re-presigning after generation completes.
    expires_in: std::time::Duration,
}

#[derive(Debug, serde::Deserialize)]
struct VideoGenStartResponse {
    #[serde(default)]
    request_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct VideoGenPollResponse {
    #[serde(default)]
    status: String,
    video: Option<VideoGenVideoInfo>,
}

#[derive(Debug, serde::Deserialize)]
struct VideoGenVideoInfo {
    url: Option<String>,
}

async fn resolve_image_reference(value: &str) -> Result<String, xai_tool_runtime::ToolError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "image reference must not be empty",
        ));
    }

    if value.starts_with("data:image/") {
        let comma = value.find(',').ok_or_else(|| {
            xai_tool_runtime::ToolError::invalid_arguments("malformed data URL in image reference")
        })?;
        if !value[..comma].contains(";base64") {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "image references only support base64 data URLs",
            ));
        }
        return Ok(value.to_owned());
    }

    if value.starts_with("https://") {
        return Ok(value.to_owned());
    }

    let raw_bytes = tokio::fs::read(value).await.map_err(|e| {
        xai_tool_runtime::ToolError::invalid_arguments(format!(
            "image reference not readable: {value} ({e})"
        ))
    })?;
    if raw_bytes.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "image reference contained no data",
        ));
    }

    let (_w, _h, mime) =
        crate::util::image_validate::validate_image_bytes(&raw_bytes).map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!("invalid image reference: {e}"))
        })?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

fn validate_one_of(
    field: &str,
    value: &str,
    allowed: &[&str],
) -> Result<(), xai_tool_runtime::ToolError> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
        "`{field}` must be one of: {}. Got {value}.",
        allowed.join(", ")
    )))
}

fn validate_imagine_duration(duration: Option<u32>) -> Result<(), xai_tool_runtime::ToolError> {
    if let Some(secs) = duration
        && !IMAGINE_VIDEO_DURATIONS_SECS.contains(&secs)
    {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "`duration` must be either 6 or 10 seconds. Got {secs}."
        )));
    }
    Ok(())
}

fn duration_from_json<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Input {
        Int(u32),
        Str(String),
    }

    match <Option<Input> as serde::Deserialize>::deserialize(deserializer)? {
        Some(Input::Int(value)) => Ok(Some(value)),
        Some(Input::Str(value)) => value
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom("duration must be 6 or 10")),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ImageToVideoInput {
    #[serde(default)]
    #[schemars(
        description = "Optional prompt to guide the video generation model. If omitted, a natural animation applies automatically."
    )]
    pub prompt: Option<String>,

    #[schemars(
        description = "Source image to animate. Provide an absolute filesystem path, HTTPS URL, or `data:image/...;base64,...` URL."
    )]
    pub image: String,

    #[serde(
        default,
        deserialize_with = "duration_from_json",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(
        description = "Duration of the video generation, either 6 or 10 seconds. Default to 6 unless the user requests longer."
    )]
    pub duration: Option<u32>,

    #[serde(default = "default_resolution_name")]
    #[schemars(
        description = "Resolution name of the video generation, only specify it when user asks for a specific resolution, either 480p or 720p. Defaults to 480p unless the user specifically requests for higher quality."
    )]
    pub resolution_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ReferenceToVideoInput {
    #[schemars(
        description = "Prompt to guide the video generation model. Describe the desired video."
    )]
    pub prompt: String,

    #[schemars(
        description = "Reference images. Provide 2 to 7 entries; the images are used as style/content references for the generated video. Each entry may be an absolute filesystem path, HTTPS URL, or `data:image/...;base64,...` URL."
    )]
    pub images: Vec<String>,

    #[schemars(
        description = "Aspect ratio of the generated video, decide it based on the user's request. 1:1 for square (icons, profiles), 16:9 for wide (landscapes, cinematic), 9:16 for tall (phone wallpapers, stories), 3:2 for horizontal photos, 2:3 for vertical (portraits, posters)."
    )]
    pub aspect_ratio: String,

    #[serde(
        default,
        deserialize_with = "duration_from_json",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(
        description = "Duration of the video generation, either 6 or 10 seconds. Defaults to 6."
    )]
    pub duration: Option<u32>,

    #[serde(default = "default_resolution_name")]
    #[schemars(
        description = "Resolution name of the video generation, only specify it when user asks for a specific resolution, either 480p or 720p. Defaults to 480p."
    )]
    pub resolution_name: String,
}

/// Acquire the shared [`VideoGenClient`] and session folder from tool
/// resources. Shared by all video-generation tools so the acquisition logic
/// lives in one place.
async fn acquire_video_client(
    ctx: &xai_tool_runtime::ToolCallContext,
) -> Result<(VideoGenClient, std::path::PathBuf), xai_tool_runtime::ToolError> {
    use crate::types::tool_metadata::shared_resources;
    let resources = shared_resources(ctx)?;
    let res = resources.lock().await;
    let client = res.require::<VideoGenClient>()?.clone();
    let session_folder = res.require::<SessionFolder>()?.0.clone();
    Ok((client, session_folder))
}

/// Persist generated video bytes to the session folder and return the absolute
/// path. Shared by all video-generation tools so the save + logging logic lives
/// in one place.
async fn save_video_bytes(
    client: &VideoGenClient,
    session_folder: &std::path::Path,
    video_bytes: &[u8],
) -> Result<std::path::PathBuf, xai_tool_runtime::ToolError> {
    let absolute_path = client
        .writer
        .save(session_folder, video_bytes, None)
        .await
        .map_err(|e| xai_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;

    tracing::info!(
        path = %absolute_path.display(),
        bytes = video_bytes.len(),
        "video saved to disk"
    );

    Ok(absolute_path)
}

async fn media_output_from_outcome(
    client: &VideoGenClient,
    session_folder: &std::path::Path,
    outcome: VideoOutcome,
) -> Result<MediaGenOutput, xai_tool_runtime::ToolError> {
    match outcome {
        VideoOutcome::Bytes(bytes) => {
            let path = save_video_bytes(client, session_folder, &bytes).await?;
            Ok(MediaGenOutput::new(path))
        }
        VideoOutcome::UploadedUrl(url) => Ok(MediaGenOutput::uploaded(url)),
    }
}

#[derive(Debug, Default)]
pub struct ImageToVideoTool;

impl crate::types::tool_metadata::ToolMetadata for ImageToVideoTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ImageToVideo
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Generate a video from a single source image; returns the saved video's absolute path. When telling the user where it was saved, refer to it by its short session-relative path (e.g. `videos/1.mp4`) rather than the absolute path, so it renders as a clickable link that opens the video. Provide `image` for the image to animate and optionally a `prompt` to guide the animation. Use this tool when the user provides an image and wants it animated, turned into a video, or used as the first frame. Example: image_to_video(image="/Users/me/photo.jpg", prompt="gentle camera push-in with wind moving the hair", duration=6, resolution_name="480p")"##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ImageToVideoTool {
    type Args = ImageToVideoInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(IMAGE_TO_VIDEO_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            IMAGE_TO_VIDEO_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.image_to_video",
        skip_all,
        fields(prompt_len = input.prompt.as_deref().unwrap_or("").len(), duration = ?input.duration, resolution = %input.resolution_name)
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ImageToVideoInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        validate_imagine_duration(input.duration)?;
        validate_one_of(
            "resolution_name",
            &input.resolution_name,
            VALID_VIDEO_RESOLUTIONS,
        )?;
        let image = resolve_image_reference(&input.image).await?;
        let prompt = input.prompt.unwrap_or_default();

        let (client, session_folder) = acquire_video_client(&ctx).await?;

        // Free / X Basic users are zero-limited on Imagine server-side; return
        // the upsell prose instead of a doomed request.
        if client.is_tier_restricted() {
            return Ok(ToolOutput::Text(TIER_RESTRICTED_UPSELL.into()));
        }

        let outcome = client
            .generate_with_images(
                XAI_VIDEO_QUALITY_MODEL,
                &prompt,
                Some(
                    input
                        .duration
                        .unwrap_or(DEFAULT_IMAGINE_VIDEO_DURATION_SECS),
                ),
                None,
                &input.resolution_name,
                Some(image),
                Vec::new(),
            )
            .await?;

        let media = media_output_from_outcome(&client, &session_folder, outcome).await?;

        Ok(ToolOutput::ImageToVideo(media))
    }
}

#[derive(Debug, Default)]
pub struct ReferenceToVideoTool;

impl crate::types::tool_metadata::ToolMetadata for ReferenceToVideoTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ReferenceToVideo
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Generate a video from multiple reference images guided by a text prompt; returns the saved video's absolute path. When telling the user where it was saved, refer to it by its short session-relative path (e.g. `videos/1.mp4`) rather than the absolute path, so it renders as a clickable link that opens the video. Provide `images` with 2 to 7 image references and a required `prompt` describing the desired video. Use this tool when the user wants a video using multiple images as style/content references. Example: reference_to_video(prompt="blend these into a cinematic fashion shot with slow dolly movement", images=["/Users/me/ref1.jpg", "/Users/me/ref2.jpg"], aspect_ratio="16:9", duration=6, resolution_name="480p")"##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ReferenceToVideoTool {
    type Args = ReferenceToVideoInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(REFERENCE_TO_VIDEO_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            REFERENCE_TO_VIDEO_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.reference_to_video",
        skip_all,
        fields(prompt_len = input.prompt.len(), num_images = input.images.len(), aspect_ratio = %input.aspect_ratio, duration = ?input.duration, resolution = %input.resolution_name)
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ReferenceToVideoInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        if input.prompt.trim().is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "`prompt` must not be empty.",
            ));
        }
        if input.images.len() < 2 {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "`images` must contain at least two image references.",
            ));
        }
        if input.images.len() > MAX_R2V_REFERENCE_IMAGES {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "`images` must contain at most {MAX_R2V_REFERENCE_IMAGES} image references."
            )));
        }
        validate_imagine_duration(input.duration)?;
        validate_one_of(
            "aspect_ratio",
            &input.aspect_ratio,
            VALID_IMAGINE_VIDEO_ASPECT_RATIOS,
        )?;
        validate_one_of(
            "resolution_name",
            &input.resolution_name,
            VALID_VIDEO_RESOLUTIONS,
        )?;

        let mut reference_images = Vec::with_capacity(input.images.len());
        for image in &input.images {
            reference_images.push(resolve_image_reference(image).await?);
        }

        let (client, session_folder) = acquire_video_client(&ctx).await?;

        // Free / X Basic users are zero-limited on Imagine server-side; return
        // the upsell prose instead of a doomed request.
        if client.is_tier_restricted() {
            return Ok(ToolOutput::Text(TIER_RESTRICTED_UPSELL.into()));
        }

        let outcome = client
            .generate_with_images(
                XAI_VIDEO_BASE_MODEL,
                &input.prompt,
                Some(
                    input
                        .duration
                        .unwrap_or(DEFAULT_IMAGINE_VIDEO_DURATION_SECS),
                ),
                Some(&input.aspect_ratio),
                &input.resolution_name,
                None,
                reference_images,
            )
            .await?;

        let media = media_output_from_outcome(&client, &session_folder, outcome).await?;

        Ok(ToolOutput::ReferenceToVideo(media))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool_metadata::test_ctx_with_call_id;

    #[test]
    fn image_to_video_name_and_description() {
        let tool = ImageToVideoTool;
        assert_eq!(
            xai_tool_runtime::Tool::id(&tool).as_str(),
            IMAGE_TO_VIDEO_TOOL_NAME
        );
        let desc = crate::types::tool_metadata::ToolMetadata::description_template(&tool);
        assert!(desc.contains("single source image"));
        assert!(desc.contains("image_to_video"));
    }

    #[test]
    fn reference_to_video_name_and_description() {
        let tool = ReferenceToVideoTool;
        assert_eq!(
            xai_tool_runtime::Tool::id(&tool).as_str(),
            REFERENCE_TO_VIDEO_TOOL_NAME
        );
        let desc = crate::types::tool_metadata::ToolMetadata::description_template(&tool);
        assert!(desc.contains("multiple reference images"));
        assert!(desc.contains("reference_to_video"));
    }

    #[test]
    fn image_to_video_defaults_match_toolbox() {
        let input: ImageToVideoInput =
            serde_json::from_str(r#"{"image":"/tmp/source.jpg"}"#).unwrap();
        assert_eq!(input.prompt, None);
        assert_eq!(input.duration, None);
        assert_eq!(input.resolution_name, DEFAULT_RESOLUTION);
    }

    #[test]
    fn reference_to_video_input_deserializes() {
        let input: ReferenceToVideoInput = serde_json::from_str(
            r#"{"prompt":"blend these","images":["/tmp/a.jpg","/tmp/b.jpg"],"aspect_ratio":"16:9","duration":"10"}"#,
        )
        .unwrap();
        assert_eq!(input.prompt, "blend these");
        assert_eq!(input.images.len(), 2);
        assert_eq!(input.aspect_ratio, "16:9");
        assert_eq!(input.duration, Some(10));
        assert_eq!(input.resolution_name, DEFAULT_RESOLUTION);
    }

    #[test]
    fn imagine_duration_validation_allows_only_toolbox_values() {
        assert!(validate_imagine_duration(None).is_ok());
        assert!(validate_imagine_duration(Some(6)).is_ok());
        assert!(validate_imagine_duration(Some(10)).is_ok());
        assert!(validate_imagine_duration(Some(8)).is_err());
    }

    #[test]
    fn image_and_reference_payload_fields_are_serialized() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_QUALITY_MODEL,
            prompt: "animate",
            image: Some(VideoImageUrl {
                url: "data:image/png;base64,a".to_owned(),
            }),
            duration: Some(6),
            aspect_ratio: None,
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["image"]["url"], "data:image/png;base64,a");
        assert!(json.get("aspect_ratio").is_none());
        assert!(json.get("output").is_none());

        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_BASE_MODEL,
            prompt: "blend",
            image: None,
            duration: Some(6),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: vec![
                VideoImageUrl {
                    url: "data:image/png;base64,a".to_owned(),
                },
                VideoImageUrl {
                    url: "data:image/png;base64,b".to_owned(),
                },
            ],
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["reference_images"].as_array().unwrap().len(), 2);
        assert_eq!(json["aspect_ratio"], "16:9");
    }

    #[test]
    fn output_upload_url_serialized_when_present() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_QUALITY_MODEL,
            prompt: "animate",
            image: None,
            duration: Some(6),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            output: Some(VideoOutput {
                upload_url: "https://bucket.example.com/signed-put".to_owned(),
            }),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json["output"]["upload_url"],
            "https://bucket.example.com/signed-put"
        );
    }

    #[test]
    fn zdr_presign_expires_secs_clamps_below_minimum() {
        // Below minimum → clamped up.
        assert_eq!(
            zdr_presign_expires_secs(60),
            MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
        );
        assert_eq!(
            zdr_presign_expires_secs(0),
            MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
        );
        // At or above minimum → passthrough.
        assert_eq!(
            zdr_presign_expires_secs(MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS),
            MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS
        );
        let large = MIN_ZDR_VIDEO_PRESIGN_EXPIRES_SECS + 600;
        assert_eq!(zdr_presign_expires_secs(large), large);
    }

    #[test]
    fn zdr_select_get_credentials() {
        let rw = S3AccessCredentials {
            access_key_id: "rw".into(),
            secret_access_key: "rw-secret".into(),
        };
        let mut config = ZdrVideoOutputS3Config {
            bucket: "b".into(),
            endpoint: "https://s3.example.com".into(),
            region: "us-east-1".into(),
            key_prefix: String::new(),
            expires_secs: DEFAULT_ZDR_VIDEO_PRESIGN_EXPIRES_SECS,
            read_write: rw.clone(),
            read_only: None,
        };

        let (creds, source) = zdr_get_credentials(&config);
        assert_eq!((source, creds.access_key_id.as_str()), ("read_write", "rw"));

        config.read_only = Some(S3AccessCredentials {
            access_key_id: "ro".into(),
            secret_access_key: "ro-secret".into(),
        });
        let (creds, source) = zdr_get_credentials(&config);
        assert_eq!((source, creds.access_key_id.as_str()), ("read_only", "ro"));

        config.read_only = Some(S3AccessCredentials {
            access_key_id: "   ".into(),
            secret_access_key: String::new(),
        });
        let (creds, source) = zdr_get_credentials(&config);
        assert_eq!((source, creds.access_key_id.as_str()), ("read_write", "rw"));
    }

    #[test]
    fn zdr_video_output_s3_config_deserializes() {
        let cfg: ZdrVideoOutputS3Config = serde_json::from_value(serde_json::json!({
            "bucket": "team-videos",
            "endpoint": "https://s3.example.com",
            "region": "us-east-1",
            "read_write": {
                "access_key_id": "AKIATEST",
                "secret_access_key": "secret",
            },
        }))
        .unwrap();
        assert!(cfg.is_valid());
    }

    #[test]
    fn zdr_video_object_key_normalizes_prefix() {
        // No prefix → bare UUID.mp4.
        let key = zdr_video_object_key("");
        assert!(key.ends_with(".mp4"), "key must end with .mp4: {key}");
        assert!(!key.starts_with('/'), "bare key must not start with /");

        // Prefix with trailing slash → preserved.
        let key = zdr_video_object_key("team/videos/");
        assert!(
            key.starts_with("team/videos/"),
            "prefix must be preserved: {key}"
        );
        assert!(key.ends_with(".mp4"));

        // Prefix without trailing slash → slash appended.
        let key = zdr_video_object_key("team/videos");
        assert!(
            key.starts_with("team/videos/"),
            "trailing / must be added: {key}"
        );

        // Whitespace-only prefix → treated as empty.
        let key = zdr_video_object_key("   ");
        assert!(
            !key.contains(' '),
            "whitespace prefix must be trimmed: {key}"
        );
        assert!(key.ends_with(".mp4"));

        // Two calls produce different keys (UUID uniqueness).
        let a = zdr_video_object_key("v/");
        let b = zdr_video_object_key("v/");
        assert_ne!(a, b, "object keys must be unique across calls");
    }

    #[test]
    fn is_http_url_validates_scheme() {
        assert!(is_http_url("https://bucket.example.com/signed?token=abc"));
        assert!(is_http_url("http://localhost:9000/test"));
        assert!(!is_http_url("ftp://files.example.com/video.mp4"));
        assert!(!is_http_url("file:///tmp/video.mp4"));
        assert!(!is_http_url("not-a-url"));
        assert!(!is_http_url(""));
    }

    #[tokio::test]
    async fn image_to_video_rejects_bad_duration() {
        let tool = ImageToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ImageToVideoInput {
                prompt: None,
                image: "/tmp/source.jpg".into(),
                duration: Some(8),
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected invalid duration error");
        assert!(err.to_string().contains("either 6 or 10"));
    }

    #[tokio::test]
    async fn reference_to_video_rejects_bad_aspect_ratio() {
        let tool = ReferenceToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ReferenceToVideoInput {
                prompt: "blend".into(),
                images: vec!["/tmp/a.jpg".into(), "/tmp/b.jpg".into()],
                aspect_ratio: "4:3".into(),
                duration: None,
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected aspect ratio error");
        assert!(err.to_string().contains("aspect_ratio"));
    }

    #[tokio::test]
    async fn image_to_video_rejects_bad_resolution() {
        let tool = ImageToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ImageToVideoInput {
                prompt: None,
                image: "/tmp/source.jpg".into(),
                duration: None,
                resolution_name: "1080p".into(),
            },
        )
        .await
        .expect_err("Expected resolution error");
        assert!(err.to_string().contains("resolution_name"));
    }

    #[tokio::test]
    async fn reference_to_video_rejects_too_few_images() {
        let tool = ReferenceToVideoTool;
        let resources = crate::types::resources::Resources::new();
        let err = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ReferenceToVideoInput {
                prompt: "blend".into(),
                images: vec!["/tmp/a.jpg".into()],
                aspect_ratio: "16:9".into(),
                duration: None,
                resolution_name: DEFAULT_RESOLUTION.into(),
            },
        )
        .await
        .expect_err("Expected image count error");
        assert!(err.to_string().contains("at least two"));
    }

    #[test]
    fn omitted_duration_is_dropped_from_wire_payload() {
        // Regression: an unset `duration` must not be serialized at all
        // (no `null`, no synthetic default) so the server's default applies.
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_QUALITY_MODEL,
            prompt: "test",
            image: None,
            duration: None,
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            json.get("duration").is_none(),
            "duration must be omitted, got: {json:?}"
        );
    }

    #[test]
    fn explicit_duration_is_present_on_wire() {
        let payload = GenerateVideoPayload {
            model: XAI_VIDEO_QUALITY_MODEL,
            prompt: "test",
            image: None,
            duration: Some(12),
            aspect_ratio: Some("16:9"),
            resolution: DEFAULT_RESOLUTION,
            reference_images: Vec::new(),
            output: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json.get("duration"), Some(&serde_json::Value::from(12)));
    }
}
