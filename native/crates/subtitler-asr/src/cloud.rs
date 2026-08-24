//! Explicit, local-first cloud ASR routing contracts.
//!
//! This module deliberately does **not** perform HTTP requests or upload
//! audio. It gives a future native-host integration the types needed to
//! describe a provider, show an exact but redacted disclosure, obtain an
//! affirmative per-job consent token, and select a route safely. In
//! particular, a hardware recommendation or
//! [`subtitler_core::ProcessingPreference`] is not a consent token and cannot
//! cause a cloud route to be selected.
//!
//! A future uploader must still resolve each configured hostname immediately
//! before connecting, reject private DNS answers, pin that connection, and
//! re-check every redirect. `CloudEndpoint` intentionally validates literal
//! addresses and local host names only; it performs no DNS lookup because this
//! contract has no network side effects.

use crate::TranscriptionRequest;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use subtitler_core::{JobId, MediaSource};
use thiserror::Error;
use url::{Host, Url};

const MAX_MODEL_NAME_LENGTH: usize = 128;
const MAX_PROVIDER_LABEL_LENGTH: usize = 80;

/// The cloud providers supported by this contract. A provider name is
/// disclosure metadata only; it does not assert that a provider currently
/// offers a particular model or upload API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudProviderKind {
    OpenAI,
    OpenRouter,
    OpenAICompatible,
}

impl CloudProviderKind {
    /// Short user-facing provider label suitable for the consent surface.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::OpenAI => "OpenAI",
            Self::OpenRouter => "OpenRouter",
            Self::OpenAICompatible => "OpenAI-compatible provider",
        }
    }
}

/// A safe representation of a configured cloud endpoint for display or a
/// future extension/native disclosure message. It intentionally carries only
/// scheme, host, and an optional non-default port: no path, query, fragment,
/// username, password, or API key can be rendered from this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedEndpointIdentity {
    scheme: String,
    host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

impl RedactedEndpointIdentity {
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> Option<u16> {
        self.port
    }
}

impl fmt::Display for RedactedEndpointIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(port) => write!(formatter, "{}://{}:{}", self.scheme, host, port),
            None => write!(formatter, "{}://{}", self.scheme, host),
        }
    }
}

/// A validated base endpoint retained only inside the ASR boundary. Its raw
/// URL is intentionally not serializable, displayable, or exposed to callers;
/// callers receive [`RedactedEndpointIdentity`] for consent and diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct CloudEndpoint {
    url: Url,
    identity: RedactedEndpointIdentity,
}

impl CloudEndpoint {
    /// Validates a provider base URL without performing network I/O.
    ///
    /// Cloud endpoints must be HTTPS, have no embedded credentials, omit a
    /// query string and fragment, and use neither a private literal address
    /// nor a well-known local hostname. Custom OpenAI-compatible endpoints
    /// should be a stable API base such as `https://asr.example.com/v1`.
    pub fn parse(value: &str) -> Result<Self, CloudEndpointError> {
        let url = Url::parse(value).map_err(|_| CloudEndpointError::InvalidUrl)?;
        if url.scheme() != "https" || url.host().is_none() {
            return Err(CloudEndpointError::HttpsRequired);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(CloudEndpointError::EmbeddedCredentials);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(CloudEndpointError::QueryOrFragment);
        }
        if url.port() == Some(0) {
            return Err(CloudEndpointError::InvalidPort);
        }

        let host = url.host().ok_or(CloudEndpointError::HttpsRequired)?;
        let redacted_host = match host {
            Host::Domain(domain) => {
                if is_local_domain(domain) {
                    return Err(CloudEndpointError::PrivateOrLocalHost);
                }
                domain.trim_end_matches('.').to_owned()
            }
            Host::Ipv4(address) => {
                if is_private_or_local_address(IpAddr::V4(address)) {
                    return Err(CloudEndpointError::PrivateOrLocalHost);
                }
                address.to_string()
            }
            Host::Ipv6(address) => {
                if is_private_or_local_address(IpAddr::V6(address)) {
                    return Err(CloudEndpointError::PrivateOrLocalHost);
                }
                address.to_string()
            }
        };

        Ok(Self {
            identity: RedactedEndpointIdentity {
                scheme: "https".to_owned(),
                host: redacted_host,
                port: url.port(),
            },
            url,
        })
    }

    /// The only endpoint information suitable for UI, logs, or IPC.
    pub fn redacted_identity(&self) -> &RedactedEndpointIdentity {
        &self.identity
    }
}

impl fmt::Debug for CloudEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CloudEndpoint")
            .field(&self.identity)
            .finish()
    }
}

/// Endpoint validation errors omit the submitted address so accidental logs
/// cannot disclose a custom provider path or query string.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CloudEndpointError {
    #[error("The cloud provider endpoint is not a valid HTTPS URL.")]
    InvalidUrl,
    #[error("Cloud provider endpoints must use HTTPS and name a host.")]
    HttpsRequired,
    #[error("Cloud provider endpoints must not contain embedded credentials.")]
    EmbeddedCredentials,
    #[error("Cloud provider endpoints must not include a query string or fragment.")]
    QueryOrFragment,
    #[error("Cloud provider endpoints must not use port zero.")]
    InvalidPort,
    #[error(
        "Subtitler will not use a cloud provider endpoint on a private or local network host."
    )]
    PrivateOrLocalHost,
}

/// A provider model identifier. It is deliberately narrow enough to keep
/// generated configuration and diagnostics bounded; model names are not
/// treated as secrets.
#[derive(Clone, PartialEq, Eq)]
pub struct CloudModelName(String);

impl CloudModelName {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CloudConfigurationError> {
        let trimmed = value.as_ref().trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_MODEL_NAME_LENGTH
            || trimmed.chars().any(char::is_control)
        {
            return Err(CloudConfigurationError::InvalidModelName);
        }
        Ok(Self(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CloudModelName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CloudModelName")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for CloudModelName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An API key whose value is never rendered by this crate and is not
/// serializable. It exists primarily for small embedders and tests; production
/// integrations should normally supply an [`ApiKeyProvider`] backed by OS
/// credential storage.
pub struct SecretApiKey(String);

impl SecretApiKey {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CloudCredentialError> {
        let value = value.as_ref();
        if value.trim().is_empty() {
            return Err(CloudCredentialError::EmptyKey);
        }
        Ok(Self(value.to_owned()))
    }

    fn expose_to(&self, receiver: &mut dyn FnMut(&str)) {
        receiver(&self.0);
    }
}

impl fmt::Debug for SecretApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretApiKey(<redacted>)")
    }
}

impl fmt::Display for SecretApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Supplies a key only to a caller-provided in-memory callback. The trait
/// never returns a serializable or displayable secret value. A future upload
/// implementation can provide the authorization header inside this callback
/// without putting credentials in job records, provider configuration, or
/// diagnostics.
pub trait ApiKeyProvider: Send + Sync {
    fn with_api_key(
        &self,
        provider: CloudProviderKind,
        receiver: &mut dyn FnMut(&str),
    ) -> Result<(), CloudCredentialError>;
}

/// A process-memory key source for callers that already hold an explicit API
/// key. It is intentionally not serializable; do not use it as persistent
/// configuration or write it to logs.
pub struct StaticApiKeyProvider {
    key: SecretApiKey,
}

impl StaticApiKeyProvider {
    pub fn new(key: SecretApiKey) -> Self {
        Self { key }
    }
}

impl fmt::Debug for StaticApiKeyProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticApiKeyProvider")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl ApiKeyProvider for StaticApiKeyProvider {
    fn with_api_key(
        &self,
        _provider: CloudProviderKind,
        receiver: &mut dyn FnMut(&str),
    ) -> Result<(), CloudCredentialError> {
        self.key.expose_to(receiver);
        Ok(())
    }
}

/// Credential-provider errors deliberately omit provider or key values.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CloudCredentialError {
    #[error("The configured cloud API key is empty.")]
    EmptyKey,
    #[error("No API key is available for the selected cloud provider.")]
    Unavailable,
}

/// Common contract implemented by each non-local ASR provider configuration.
/// It defines selection and disclosure metadata only: it has no method that
/// uploads audio or starts a network request.
pub trait CloudSpeechProvider: Send + Sync {
    fn kind(&self) -> CloudProviderKind;
    /// The exact provider label shown to the user before this provider may be
    /// selected. For a custom endpoint this is the validated configured label,
    /// not the URL itself.
    fn disclosure_name(&self) -> &str;
    fn model(&self) -> &CloudModelName;
    fn endpoint(&self) -> &CloudEndpoint;
    fn api_key_provider(&self) -> &dyn ApiKeyProvider;
}

/// Configuration for OpenAI's official API endpoint.
#[derive(Clone)]
pub struct OpenAIProvider {
    model: CloudModelName,
    endpoint: CloudEndpoint,
    api_key_provider: Arc<dyn ApiKeyProvider>,
}

impl OpenAIProvider {
    pub fn new(
        model: impl AsRef<str>,
        api_key_provider: Arc<dyn ApiKeyProvider>,
    ) -> Result<Self, CloudConfigurationError> {
        Self::with_endpoint(
            official_endpoint("https://api.openai.com/v1"),
            model,
            api_key_provider,
        )
    }

    fn with_endpoint(
        endpoint: CloudEndpoint,
        model: impl AsRef<str>,
        api_key_provider: Arc<dyn ApiKeyProvider>,
    ) -> Result<Self, CloudConfigurationError> {
        Ok(Self {
            model: CloudModelName::new(model)?,
            endpoint,
            api_key_provider,
        })
    }
}

impl fmt::Debug for OpenAIProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAIProvider")
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("api_key_provider", &"<opaque API-key provider>")
            .finish()
    }
}

impl CloudSpeechProvider for OpenAIProvider {
    fn kind(&self) -> CloudProviderKind {
        CloudProviderKind::OpenAI
    }

    fn disclosure_name(&self) -> &str {
        CloudProviderKind::OpenAI.display_name()
    }

    fn model(&self) -> &CloudModelName {
        &self.model
    }

    fn endpoint(&self) -> &CloudEndpoint {
        &self.endpoint
    }

    fn api_key_provider(&self) -> &dyn ApiKeyProvider {
        self.api_key_provider.as_ref()
    }
}

/// Configuration for OpenRouter's official API endpoint. Whether an account
/// exposes a compatible transcription model is intentionally a future
/// execution-time capability check, not an assumption made by this contract.
#[derive(Clone)]
pub struct OpenRouterProvider {
    model: CloudModelName,
    endpoint: CloudEndpoint,
    api_key_provider: Arc<dyn ApiKeyProvider>,
}

impl OpenRouterProvider {
    pub fn new(
        model: impl AsRef<str>,
        api_key_provider: Arc<dyn ApiKeyProvider>,
    ) -> Result<Self, CloudConfigurationError> {
        Self::with_endpoint(
            official_endpoint("https://openrouter.ai/api/v1"),
            model,
            api_key_provider,
        )
    }

    fn with_endpoint(
        endpoint: CloudEndpoint,
        model: impl AsRef<str>,
        api_key_provider: Arc<dyn ApiKeyProvider>,
    ) -> Result<Self, CloudConfigurationError> {
        Ok(Self {
            model: CloudModelName::new(model)?,
            endpoint,
            api_key_provider,
        })
    }
}

impl fmt::Debug for OpenRouterProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterProvider")
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("api_key_provider", &"<opaque API-key provider>")
            .finish()
    }
}

impl CloudSpeechProvider for OpenRouterProvider {
    fn kind(&self) -> CloudProviderKind {
        CloudProviderKind::OpenRouter
    }

    fn disclosure_name(&self) -> &str {
        CloudProviderKind::OpenRouter.display_name()
    }

    fn model(&self) -> &CloudModelName {
        &self.model
    }

    fn endpoint(&self) -> &CloudEndpoint {
        &self.endpoint
    }

    fn api_key_provider(&self) -> &dyn ApiKeyProvider {
        self.api_key_provider.as_ref()
    }
}

/// Configuration for a deliberately named OpenAI-compatible endpoint.
/// `display_name` is used in the consent disclosure so a generic custom URL
/// never turns into an unlabelled upload destination.
#[derive(Clone)]
pub struct OpenAICompatibleProvider {
    display_name: String,
    model: CloudModelName,
    endpoint: CloudEndpoint,
    api_key_provider: Arc<dyn ApiKeyProvider>,
}

impl OpenAICompatibleProvider {
    pub fn new(
        display_name: impl AsRef<str>,
        endpoint_url: impl AsRef<str>,
        model: impl AsRef<str>,
        api_key_provider: Arc<dyn ApiKeyProvider>,
    ) -> Result<Self, CloudConfigurationError> {
        let display_name = validate_provider_label(display_name.as_ref())?;
        Ok(Self {
            display_name,
            model: CloudModelName::new(model)?,
            endpoint: CloudEndpoint::parse(endpoint_url.as_ref())?,
            api_key_provider,
        })
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

impl fmt::Debug for OpenAICompatibleProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAICompatibleProvider")
            .field("display_name", &self.display_name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("api_key_provider", &"<opaque API-key provider>")
            .finish()
    }
}

impl CloudSpeechProvider for OpenAICompatibleProvider {
    fn kind(&self) -> CloudProviderKind {
        CloudProviderKind::OpenAICompatible
    }

    fn disclosure_name(&self) -> &str {
        &self.display_name
    }

    fn model(&self) -> &CloudModelName {
        &self.model
    }

    fn endpoint(&self) -> &CloudEndpoint {
        &self.endpoint
    }

    fn api_key_provider(&self) -> &dyn ApiKeyProvider {
        self.api_key_provider.as_ref()
    }
}

/// A cloud provider configuration suitable for a potential route. This enum
/// has no serde implementation because every variant owns an opaque credential
/// provider. Serialize a [`CloudProcessingDisclosure`] instead when showing
/// the user what would be sent.
#[derive(Clone)]
pub enum CloudProviderConfiguration {
    OpenAI(OpenAIProvider),
    OpenRouter(OpenRouterProvider),
    OpenAICompatible(OpenAICompatibleProvider),
}

impl CloudProviderConfiguration {
    pub fn as_provider(&self) -> &dyn CloudSpeechProvider {
        match self {
            Self::OpenAI(provider) => provider,
            Self::OpenRouter(provider) => provider,
            Self::OpenAICompatible(provider) => provider,
        }
    }
}

impl fmt::Debug for CloudProviderConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenAI(provider) => formatter.debug_tuple("OpenAI").field(provider).finish(),
            Self::OpenRouter(provider) => {
                formatter.debug_tuple("OpenRouter").field(provider).finish()
            }
            Self::OpenAICompatible(provider) => formatter
                .debug_tuple("OpenAICompatible")
                .field(provider)
                .finish(),
        }
    }
}

impl CloudSpeechProvider for CloudProviderConfiguration {
    fn kind(&self) -> CloudProviderKind {
        self.as_provider().kind()
    }

    fn disclosure_name(&self) -> &str {
        self.as_provider().disclosure_name()
    }

    fn model(&self) -> &CloudModelName {
        self.as_provider().model()
    }

    fn endpoint(&self) -> &CloudEndpoint {
        self.as_provider().endpoint()
    }

    fn api_key_provider(&self) -> &dyn ApiKeyProvider {
        self.as_provider().api_key_provider()
    }
}

/// Configuration for the existing [`crate::LocalProvider`] implementation.
/// It contains only normal local ASR settings and can be selected without a
/// cloud disclosure or consent token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalProviderConfiguration {
    transcription: TranscriptionRequest,
}

impl LocalProviderConfiguration {
    pub fn new(transcription: TranscriptionRequest) -> Self {
        Self { transcription }
    }

    pub fn transcription(&self) -> &TranscriptionRequest {
        &self.transcription
    }

    pub fn into_transcription(self) -> TranscriptionRequest {
        self.transcription
    }
}

/// The normalized audio scope that a future cloud request may send. It is
/// shown verbatim in the consent disclosure and compared again at route time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloudAudioScope {
    EntireRecording,
    TimeRange { start_ms: u64, end_ms: u64 },
}

impl CloudAudioScope {
    pub const fn entire_recording() -> Self {
        Self::EntireRecording
    }

    pub fn time_range(start_ms: u64, end_ms: u64) -> Result<Self, CloudDisclosureError> {
        let scope = Self::TimeRange { start_ms, end_ms };
        scope.validate()?;
        Ok(scope)
    }

    fn validate(&self) -> Result<(), CloudDisclosureError> {
        if let Self::TimeRange { start_ms, end_ms } = self {
            if end_ms <= start_ms {
                return Err(CloudDisclosureError::InvalidAudioScope);
            }
        }
        Ok(())
    }
}

/// Source categories that are safe to disclose. The original URL, file path,
/// page title, query string, and browser-session data are never retained here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudAudioSourceKind {
    LocalFile,
    DirectMedia,
    BrowserPage,
    NormalizedAudio,
}

/// Redacted source metadata shown as part of an explicit cloud disclosure.
/// `host` is intentionally the origin hostname only, never a full URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedAudioSource {
    kind: CloudAudioSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
}

impl RedactedAudioSource {
    pub fn from_media_source(source: &MediaSource) -> Self {
        match source {
            MediaSource::LocalFile { .. } => Self {
                kind: CloudAudioSourceKind::LocalFile,
                host: None,
            },
            MediaSource::DirectUrl { media_url } => Self {
                kind: CloudAudioSourceKind::DirectMedia,
                host: redacted_host_from_url(media_url),
            },
            MediaSource::Page { page_url } => Self {
                kind: CloudAudioSourceKind::BrowserPage,
                host: redacted_host_from_url(page_url),
            },
        }
    }

    /// Use when the original acquisition source is intentionally unavailable
    /// to the cloud-routing layer. It still makes clear that normalized audio,
    /// not video, would be sent after consent.
    pub const fn normalized_audio() -> Self {
        Self {
            kind: CloudAudioSourceKind::NormalizedAudio,
            host: None,
        }
    }

    pub const fn kind(&self) -> CloudAudioSourceKind {
        self.kind
    }

    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }
}

/// The complete safe-to-serialize disclosure that must be presented before a
/// caller creates [`CloudProcessingConsent`]. It identifies the selected
/// provider model but never serializes an API key,
/// raw source URL, query string, local path, session credential, or provider
/// endpoint path. The private endpoint binding preserves the exact validated
/// endpoint for consent comparison, so two custom paths on one host cannot
/// share an approval merely because their displayed origin is the same.
///
/// Deliberately do not deserialize this type: a valid consent disclosure must
/// originate in the native provider configuration, not be reconstructed from
/// browser-controlled data after its private endpoint binding was removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CloudProcessingDisclosure {
    provider: CloudProviderKind,
    provider_name: String,
    model: String,
    endpoint: RedactedEndpointIdentity,
    audio_scope: CloudAudioScope,
    source: RedactedAudioSource,
    #[serde(skip_serializing)]
    endpoint_binding: CloudEndpoint,
}

impl CloudProcessingDisclosure {
    pub fn for_provider(
        provider: &dyn CloudSpeechProvider,
        audio_scope: CloudAudioScope,
        source: RedactedAudioSource,
    ) -> Result<Self, CloudDisclosureError> {
        audio_scope.validate()?;
        let endpoint_binding = provider.endpoint().clone();
        Ok(Self {
            provider: provider.kind(),
            provider_name: provider.disclosure_name().to_owned(),
            model: provider.model().as_str().to_owned(),
            endpoint: endpoint_binding.redacted_identity().clone(),
            audio_scope,
            source,
            endpoint_binding,
        })
    }

    pub const fn provider(&self) -> CloudProviderKind {
        self.provider
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// The provider model identifier shown with the consent disclosure. It is
    /// part of the private consent comparison, so changing a model cannot
    /// reuse an approval that was shown for another model.
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn endpoint(&self) -> &RedactedEndpointIdentity {
        &self.endpoint
    }

    pub fn audio_scope(&self) -> &CloudAudioScope {
        &self.audio_scope
    }

    pub fn source(&self) -> &RedactedAudioSource {
        &self.source
    }
}

/// A non-serializable proof that the caller obtained an affirmative user
/// action after displaying the exact disclosure for one job. This type cannot
/// be created by a struct literal, and route selection compares every relevant
/// field again so it cannot be reused for another provider, model, endpoint,
/// source, audio scope, or job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudProcessingConsent {
    job_id: JobId,
    disclosure: CloudProcessingDisclosure,
}

impl CloudProcessingConsent {
    /// Call only after the user has been shown `disclosure` and explicitly
    /// approved the stated provider, endpoint identity, and audio scope.
    pub fn confirm(
        job_id: JobId,
        disclosure: CloudProcessingDisclosure,
    ) -> Result<Self, CloudDisclosureError> {
        disclosure.audio_scope.validate()?;
        Ok(Self { job_id, disclosure })
    }

    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    pub fn disclosure(&self) -> &CloudProcessingDisclosure {
        &self.disclosure
    }

    fn permits(&self, job_id: &JobId, disclosure: &CloudProcessingDisclosure) -> bool {
        &self.job_id == job_id && &self.disclosure == disclosure
    }
}

/// Which route the caller intends to use. The default always remains local.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderSelectionIntent {
    #[default]
    LocalFirst,
    CloudRequested,
}

/// Inputs to a one-job route decision. This type intentionally stores opaque
/// provider configuration rather than a serializable API key or upload-ready
/// request.
#[derive(Clone, Debug)]
pub struct ProviderRouteRequest {
    job_id: JobId,
    local: LocalProviderConfiguration,
    intent: ProviderSelectionIntent,
    cloud: Option<CloudProviderConfiguration>,
    consent: Option<CloudProcessingConsent>,
    source: RedactedAudioSource,
    audio_scope: CloudAudioScope,
}

impl ProviderRouteRequest {
    pub fn local_first(job_id: JobId, local: LocalProviderConfiguration) -> Self {
        Self {
            job_id,
            local,
            intent: ProviderSelectionIntent::LocalFirst,
            cloud: None,
            consent: None,
            source: RedactedAudioSource::normalized_audio(),
            audio_scope: CloudAudioScope::entire_recording(),
        }
    }

    /// Makes a cloud provider available while retaining the default local
    /// route. This is useful when a settings screen can offer cloud as an
    /// option but the user has not requested it for this job.
    pub fn with_available_cloud_provider(mut self, cloud: CloudProviderConfiguration) -> Self {
        self.cloud = Some(cloud);
        self
    }

    /// Constructs an explicit cloud request. Passing `None` for `consent` is
    /// allowed only so callers can receive a typed `CloudConsentRequired`
    /// error; it can never result in a cloud route.
    pub fn cloud_requested(
        job_id: JobId,
        local: LocalProviderConfiguration,
        cloud: CloudProviderConfiguration,
        consent: Option<CloudProcessingConsent>,
        source: RedactedAudioSource,
        audio_scope: CloudAudioScope,
    ) -> Self {
        Self {
            job_id,
            local,
            intent: ProviderSelectionIntent::CloudRequested,
            cloud: Some(cloud),
            consent,
            source,
            audio_scope,
        }
    }
}

/// Selects a route without performing any ASR or network activity.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProviderRouteSelector;

impl ProviderRouteSelector {
    pub fn select(
        &self,
        request: ProviderRouteRequest,
    ) -> Result<SelectedProviderRoute, CloudRoutingError> {
        if request.intent == ProviderSelectionIntent::LocalFirst {
            return Ok(SelectedProviderRoute::Local(request.local));
        }

        let cloud = request
            .cloud
            .ok_or(CloudRoutingError::CloudProviderUnavailable)?;
        let disclosure =
            CloudProcessingDisclosure::for_provider(&cloud, request.audio_scope, request.source)?;
        let consent = request
            .consent
            .ok_or(CloudRoutingError::CloudConsentRequired)?;
        if !consent.permits(&request.job_id, &disclosure) {
            return Err(CloudRoutingError::CloudConsentMismatch);
        }

        Ok(SelectedProviderRoute::Cloud(Box::new(
            SelectedCloudProvider {
                provider: cloud,
                disclosure,
            },
        )))
    }
}

/// A safe result of routing. Selecting `Cloud` proves that the caller supplied
/// an exact, matching per-job consent token; it still does not upload audio.
#[derive(Clone, Debug)]
pub enum SelectedProviderRoute {
    Local(LocalProviderConfiguration),
    Cloud(Box<SelectedCloudProvider>),
}

impl SelectedProviderRoute {
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }
}

/// A cloud provider and the immutable disclosure it was consented for.
#[derive(Clone, Debug)]
pub struct SelectedCloudProvider {
    provider: CloudProviderConfiguration,
    disclosure: CloudProcessingDisclosure,
}

impl SelectedCloudProvider {
    pub fn provider(&self) -> &CloudProviderConfiguration {
        &self.provider
    }

    pub fn disclosure(&self) -> &CloudProcessingDisclosure {
        &self.disclosure
    }
}

/// Routing and disclosure errors do not include source URLs, endpoints, or
/// credentials. They are suitable for user-facing status once the host maps
/// them to its normal job failure surface.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CloudRoutingError {
    #[error("Cloud processing was requested, but no cloud provider is configured.")]
    CloudProviderUnavailable,
    #[error("Cloud processing requires explicit per-job user consent before audio can leave this device.")]
    CloudConsentRequired,
    #[error(
        "The cloud consent does not match this job's provider, endpoint, source, or audio scope."
    )]
    CloudConsentMismatch,
    #[error(transparent)]
    Disclosure(#[from] CloudDisclosureError),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CloudDisclosureError {
    #[error("A cloud audio time range must have a non-zero end after its start.")]
    InvalidAudioScope,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CloudConfigurationError {
    #[error(
        "Cloud provider model names must be non-empty, bounded, and free of control characters."
    )]
    InvalidModelName,
    #[error(
        "Custom cloud provider names must be non-empty, bounded, and free of control characters."
    )]
    InvalidProviderLabel,
    #[error(transparent)]
    Endpoint(#[from] CloudEndpointError),
}

fn official_endpoint(value: &str) -> CloudEndpoint {
    CloudEndpoint::parse(value).expect("static official cloud endpoint must satisfy policy")
}

fn validate_provider_label(value: &str) -> Result<String, CloudConfigurationError> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_PROVIDER_LABEL_LENGTH
        || trimmed.chars().any(char::is_control)
    {
        return Err(CloudConfigurationError::InvalidProviderLabel);
    }
    Ok(trimmed.to_owned())
}

fn redacted_host_from_url(value: &str) -> Option<String> {
    Url::parse(value).ok().and_then(|url| {
        url.host_str()
            .map(|host| host.trim_end_matches('.').to_owned())
    })
}

fn is_local_domain(domain: &str) -> bool {
    let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
    normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized == "local"
        || normalized.ends_with(".local")
        || normalized == "home.arpa"
        || normalized.ends_with(".home.arpa")
}

fn is_private_or_local_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_private_or_local_ipv4(address),
        IpAddr::V6(address) => is_private_or_local_ipv6(address),
    }
}

fn is_private_or_local_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240
}

fn is_private_or_local_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped_ipv4) = address.to_ipv4_mapped() {
        return is_private_or_local_ipv4(mapped_ipv4);
    }
    let octets = address.octets();
    let is_ipv4_compatible =
        octets[..12] == [0; 12] && !address.is_unspecified() && !address.is_loopback();
    if is_ipv4_compatible {
        return is_private_or_local_ipv4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    let is_well_known_nat64 = octets[..12]
        == [
            0x00, 0x64, 0xff, 0x9b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
    if is_well_known_nat64 {
        return is_private_or_local_ipv4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    let leading_segment = address.segments()[0];
    let is_unique_local = (leading_segment & 0xfe00) == 0xfc00; // fc00::/7
    let is_link_local = (leading_segment & 0xffc0) == 0xfe80; // fe80::/10
    let is_site_local = (leading_segment & 0xffc0) == 0xfec0; // fec0::/10
    let is_documentation = address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8;
    address.is_loopback()
        || address.is_unspecified()
        || is_unique_local
        || is_link_local
        || is_site_local
        || is_documentation
        || address.is_multicast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComputeBackend, LanguageMode, LocalModel, Quantization};
    use subtitler_core::MediaSource;

    fn local_configuration() -> LocalProviderConfiguration {
        LocalProviderConfiguration::new(TranscriptionRequest {
            language_mode: LanguageMode::English,
            word_timestamps: true,
            speaker_diarization: false,
            model: LocalModel::Small,
            quantization: Quantization::Q5Km,
            backend: ComputeBackend::Cpu,
        })
    }

    fn credentials() -> Arc<dyn ApiKeyProvider> {
        Arc::new(StaticApiKeyProvider::new(
            SecretApiKey::new("test-secret-that-must-not-appear").unwrap(),
        ))
    }

    fn openai_configuration() -> CloudProviderConfiguration {
        CloudProviderConfiguration::OpenAI(
            OpenAIProvider::new("gpt-4o-transcribe", credentials()).unwrap(),
        )
    }

    #[test]
    fn cloud_route_is_rejected_without_explicit_consent() {
        let job_id = JobId::new();
        let request = ProviderRouteRequest::cloud_requested(
            job_id,
            local_configuration(),
            openai_configuration(),
            None,
            RedactedAudioSource::normalized_audio(),
            CloudAudioScope::entire_recording(),
        );

        assert_eq!(
            ProviderRouteSelector.select(request).unwrap_err(),
            CloudRoutingError::CloudConsentRequired
        );
    }

    #[test]
    fn local_first_selection_stays_local_when_cloud_is_available() {
        let request = ProviderRouteRequest::local_first(JobId::new(), local_configuration())
            .with_available_cloud_provider(openai_configuration());

        let selected = ProviderRouteSelector.select(request).unwrap();
        assert!(selected.is_local());
        assert!(matches!(selected, SelectedProviderRoute::Local(_)));
    }

    #[test]
    fn matching_per_job_consent_selects_cloud_without_uploading() {
        let job_id = JobId::new();
        let source = RedactedAudioSource::from_media_source(&MediaSource::DirectUrl {
            media_url: "https://recording.example.test/private/meeting.mp4?token=secret".to_owned(),
        });
        let scope = CloudAudioScope::time_range(2_000, 32_000).unwrap();
        let configuration = openai_configuration();
        let disclosure =
            CloudProcessingDisclosure::for_provider(&configuration, scope.clone(), source.clone())
                .unwrap();
        let consent = CloudProcessingConsent::confirm(job_id.clone(), disclosure.clone()).unwrap();

        let selected = ProviderRouteSelector
            .select(ProviderRouteRequest::cloud_requested(
                job_id,
                local_configuration(),
                configuration,
                Some(consent),
                source,
                scope,
            ))
            .unwrap();

        let SelectedProviderRoute::Cloud(selected) = selected else {
            panic!("expected cloud selection after matching consent");
        };
        assert_eq!(selected.disclosure(), &disclosure);
        assert_eq!(selected.provider().kind(), CloudProviderKind::OpenAI);
    }

    #[test]
    fn consent_cannot_be_reused_for_another_audio_scope_or_job() {
        let job_id = JobId::new();
        let source = RedactedAudioSource::normalized_audio();
        let configuration = openai_configuration();
        let disclosure = CloudProcessingDisclosure::for_provider(
            &configuration,
            CloudAudioScope::entire_recording(),
            source.clone(),
        )
        .unwrap();
        let consent = CloudProcessingConsent::confirm(job_id, disclosure).unwrap();

        let error = ProviderRouteSelector
            .select(ProviderRouteRequest::cloud_requested(
                JobId::new(),
                local_configuration(),
                configuration,
                Some(consent),
                source,
                CloudAudioScope::time_range(1, 2).unwrap(),
            ))
            .unwrap_err();
        assert_eq!(error, CloudRoutingError::CloudConsentMismatch);
    }

    #[test]
    fn consent_cannot_be_reused_for_a_different_custom_endpoint_path_on_the_same_host() {
        let job_id = JobId::new();
        let source = RedactedAudioSource::normalized_audio();
        let scope = CloudAudioScope::entire_recording();
        let approved = CloudProviderConfiguration::OpenAICompatible(
            OpenAICompatibleProvider::new(
                "Acme ASR",
                "https://asr.acme.example/v1/tenant-a",
                "acme-transcribe-v1",
                credentials(),
            )
            .unwrap(),
        );
        let disclosure =
            CloudProcessingDisclosure::for_provider(&approved, scope.clone(), source.clone())
                .unwrap();
        let consent = CloudProcessingConsent::confirm(job_id.clone(), disclosure).unwrap();

        let changed_endpoint = CloudProviderConfiguration::OpenAICompatible(
            OpenAICompatibleProvider::new(
                "Acme ASR",
                "https://asr.acme.example/v1/tenant-b",
                "acme-transcribe-v1",
                credentials(),
            )
            .unwrap(),
        );
        let error = ProviderRouteSelector
            .select(ProviderRouteRequest::cloud_requested(
                job_id,
                local_configuration(),
                changed_endpoint,
                Some(consent),
                source,
                scope,
            ))
            .unwrap_err();

        assert_eq!(error, CloudRoutingError::CloudConsentMismatch);
    }

    #[test]
    fn endpoint_policy_rejects_private_credentials_and_local_hosts_without_false_lookalikes() {
        for endpoint in [
            "http://api.example.test/v1",
            "https://alice:secret@api.example.test/v1",
            "https://api.example.test/v1?api_key=secret",
            "https://127.0.0.1/v1",
            "https://10.0.0.1/v1",
            "https://192.168.1.10/v1",
            "https://169.254.169.254/v1",
            "https://[::1]/v1",
            "https://[::ffff:127.0.0.1]/v1",
            "https://localhost/v1",
            "https://api.localhost/v1",
            "https://meeting.local/v1",
            "https://api.home.arpa/v1",
        ] {
            assert!(CloudEndpoint::parse(endpoint).is_err(), "{endpoint}");
        }

        for endpoint in [
            "https://localhost.example.test/v1",
            "https://not-local.example.test/v1",
            "https://api.example.test/v1",
        ] {
            assert!(CloudEndpoint::parse(endpoint).is_ok(), "{endpoint}");
        }
    }

    #[test]
    fn disclosure_redacts_raw_source_metadata_and_provider_debug_redacts_keys() {
        let source = RedactedAudioSource::from_media_source(&MediaSource::DirectUrl {
            media_url: "https://alice:secret@recording.example.test/private/customer-meeting.mp4?token=very-secret&signature=also-secret".to_owned(),
        });
        let configuration = openai_configuration();
        let disclosure = CloudProcessingDisclosure::for_provider(
            &configuration,
            CloudAudioScope::entire_recording(),
            source,
        )
        .unwrap();
        let encoded = serde_json::to_string(&disclosure).unwrap();
        let debug = format!("{configuration:?}");

        assert!(encoded.contains("recording.example.test"));
        for sensitive_fragment in [
            "alice",
            "secret",
            "customer-meeting",
            "token",
            "signature",
            "very-secret",
            "test-secret-that-must-not-appear",
        ] {
            assert!(
                !encoded.contains(sensitive_fragment),
                "{sensitive_fragment}"
            );
            assert!(!debug.contains(sensitive_fragment), "{sensitive_fragment}");
        }
    }

    #[test]
    fn local_file_source_never_discloses_its_path() {
        let source = RedactedAudioSource::from_media_source(&MediaSource::LocalFile {
            path: "C:/Users/example/Confidential Meeting.wav".to_owned(),
        });
        let encoded = serde_json::to_string(&source).unwrap();

        assert_eq!(source.kind(), CloudAudioSourceKind::LocalFile);
        assert_eq!(source.host(), None);
        assert!(!encoded.contains("Confidential"));
        assert!(!encoded.contains("Users"));
    }

    #[test]
    fn credentials_are_opaque_to_debug_and_only_released_inside_a_callback() {
        let secret = SecretApiKey::new("super-secret-api-key").unwrap();
        assert!(!format!("{secret:?}").contains("super-secret-api-key"));
        assert!(!format!("{secret}").contains("super-secret-api-key"));

        let provider = StaticApiKeyProvider::new(secret);
        assert!(!format!("{provider:?}").contains("super-secret-api-key"));
        let mut received = String::new();
        provider
            .with_api_key(CloudProviderKind::OpenAI, &mut |key| {
                received.push_str(key);
            })
            .unwrap();
        assert_eq!(received, "super-secret-api-key");
    }

    #[test]
    fn custom_provider_is_validated_and_has_a_redacted_endpoint_identity() {
        let provider = OpenAICompatibleProvider::new(
            "Acme ASR",
            "https://asr.acme.example/v1/tenant/private",
            "acme-transcribe-v1",
            credentials(),
        )
        .unwrap();
        let disclosure = CloudProcessingDisclosure::for_provider(
            &provider,
            CloudAudioScope::entire_recording(),
            RedactedAudioSource::normalized_audio(),
        )
        .unwrap();

        assert_eq!(provider.display_name(), "Acme ASR");
        assert_eq!(disclosure.provider_name(), "Acme ASR");
        assert_eq!(disclosure.endpoint().host(), "asr.acme.example");
        assert_eq!(
            disclosure.endpoint().to_string(),
            "https://asr.acme.example"
        );
        assert!(!serde_json::to_string(&disclosure)
            .unwrap()
            .contains("tenant/private"));
    }
}
