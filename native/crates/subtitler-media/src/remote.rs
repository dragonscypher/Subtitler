//! Controlled direct-media acquisition.
//!
//! FFmpeg deliberately never receives a network URL. This module resolves a
//! direct HTTPS media URL itself, pins the HTTP client to the validated address
//! list for that hop, follows redirects manually, and writes the resulting
//! bytes into a private RAII-managed file. That closes the otherwise inherent
//! validate-then-resolve and redirect-to-private-network gaps of a decoder
//! fetching a URL independently.

use crate::{AudioInput, ExtractionCancellation, MediaError, MediaSourceValidator};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT_ENCODING, LOCATION, RANGE};
use reqwest::redirect::Policy;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::{Builder as TempDirBuilder, TempDir};
use thiserror::Error;
use url::{Host, Url};

/// Direct-media servers differ in whether they accept an open-ended byte
/// range. A finite range works with strict CDNs (including YouTube's media
/// CDN) while still keeping each response bounded and independently pinned.
const FINITE_RANGE_CHUNK_BYTES: u64 = 128 * 1024;

/// Resource bounds for one direct remote-media retrieval.
///
/// Defaults are intentionally finite. A user-visible cache limit can later
/// configure this policy, but a remote response must never be allowed to grow
/// without a local storage budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDownloadOptions {
    /// Number of validated redirects allowed after the initial URL.
    pub max_redirects: u8,
    /// Maximum number of source bytes written into private temporary storage.
    pub max_download_bytes: u64,
    /// Per-hop connection timeout.
    pub connect_timeout: Duration,
    /// Per-hop request deadline, including response-body transfer.
    pub request_timeout: Duration,
    /// Optional private engine cache root. `None` delegates to the OS user
    /// temporary directory through `tempfile`.
    pub temporary_root: Option<PathBuf>,
}

impl Default for RemoteDownloadOptions {
    fn default() -> Self {
        Self {
            max_redirects: 3,
            max_download_bytes: 4 * 1024 * 1024 * 1024,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(30 * 60),
            temporary_root: None,
        }
    }
}

/// Owns a downloaded source file. Dropping this value removes its unique
/// private directory, including partial data after an error or cancellation.
pub struct DownloadedRemoteMedia {
    directory: TempDir,
    input: AudioInput,
}

impl DownloadedRemoteMedia {
    /// The local-only decoder input. The remote URL is intentionally not
    /// retained by this artifact or exposed to the FFmpeg boundary.
    pub fn input(&self) -> &AudioInput {
        &self.input
    }

    /// Private source-file location for consumers that need to inspect it
    /// without retaining the original remote URL.
    pub fn path(&self) -> &Path {
        match &self.input {
            AudioInput::LocalPath(path) => path,
            AudioInput::RemoteUrl(_) => unreachable!("downloaded media is always local"),
        }
    }

    /// The private directory is useful for cleanup assertions, but callers
    /// must not persist anything beneath it.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

impl fmt::Debug for DownloadedRemoteMedia {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DownloadedRemoteMedia")
            .field("path", &"<private downloaded media>")
            .finish()
    }
}

/// Failures intentionally omit URLs, response bodies, and headers because a
/// signed media URL is sensitive job data.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RemoteMediaAcquisitionError {
    #[error("The remote media address was rejected by Subtitler's media policy.")]
    Policy(#[source] MediaError),
    #[error("Subtitler could not resolve the remote media hostname.")]
    DnsResolution,
    #[error("Subtitler could not safely connect to the remote media host.")]
    Network,
    #[error("The remote media server returned an invalid redirect.")]
    InvalidRedirect,
    #[error("The remote media server redirected too many times.")]
    RedirectLimitExceeded,
    #[error("The remote media server returned an unsupported response.")]
    UnexpectedResponse,
    #[error("The remote media server returned an invalid partial-media response.")]
    InvalidPartialResponse,
    #[error("The remote media exceeds Subtitler's temporary-cache limit ({maximum_bytes} bytes).")]
    ResponseTooLarge { maximum_bytes: u64 },
    #[error("Subtitler could not create private temporary media storage.")]
    TemporaryStorage,
    #[error("Remote media acquisition was cancelled.")]
    Cancelled,
}

/// A direct-media downloader that is deliberately separated from FFmpeg.
///
/// The production path uses a fresh client for each redirect hop. Each client
/// has redirects and proxy discovery disabled, and its DNS result is replaced
/// with addresses that were resolved and policy-checked immediately before
/// that request.
#[derive(Clone, Debug)]
pub struct RemoteMediaAcquirer {
    validator: MediaSourceValidator,
    options: RemoteDownloadOptions,
}

impl Default for RemoteMediaAcquirer {
    fn default() -> Self {
        Self::new(
            MediaSourceValidator::default(),
            RemoteDownloadOptions::default(),
        )
    }
}

impl RemoteMediaAcquirer {
    pub fn new(validator: MediaSourceValidator, options: RemoteDownloadOptions) -> Self {
        Self { validator, options }
    }

    pub fn options(&self) -> &RemoteDownloadOptions {
        &self.options
    }

    /// Retrieve a direct source into a private local file. The input is
    /// validated again here even when a dispatcher already preflighted it;
    /// acquisition is the security boundary closest to the outbound request.
    pub fn acquire(
        &self,
        url: &Url,
        cancellation: &ExtractionCancellation,
    ) -> Result<DownloadedRemoteMedia, RemoteMediaAcquisitionError> {
        let mut resolver = SystemDnsResolver;
        let mut transport = ReqwestPinnedTransport;
        self.acquire_with(url, cancellation, &mut resolver, &mut transport)
    }

    /// Acquire a direct source whose provider issues a fresh, short-lived URL
    /// for each continuation range. The refresher is called only after a
    /// validated partial response proves that another range is needed.
    ///
    /// The refreshed URL receives the same scheme, destination, redirect,
    /// DNS-pinning, bounded-cache, and private-file checks as the initial URL.
    pub fn acquire_with_url_refresher<F>(
        &self,
        initial_url: &Url,
        cancellation: &ExtractionCancellation,
        mut refresh: F,
    ) -> Result<DownloadedRemoteMedia, RemoteMediaAcquisitionError>
    where
        F: FnMut() -> Result<Url, RemoteMediaAcquisitionError>,
    {
        let mut resolver = SystemDnsResolver;
        let mut transport = ReqwestPinnedTransport;
        self.acquire_with_refresher(
            initial_url,
            cancellation,
            &mut resolver,
            &mut transport,
            &mut refresh,
        )
    }

    fn acquire_with<R: DnsResolver, T: PinnedTransport>(
        &self,
        url: &Url,
        cancellation: &ExtractionCancellation,
        resolver: &mut R,
        transport: &mut T,
    ) -> Result<DownloadedRemoteMedia, RemoteMediaAcquisitionError> {
        let mut unchanged_url = || Ok(url.clone());
        self.acquire_with_refresher(url, cancellation, resolver, transport, &mut unchanged_url)
    }

    fn acquire_with_refresher<R: DnsResolver, T: PinnedTransport, F>(
        &self,
        url: &Url,
        cancellation: &ExtractionCancellation,
        resolver: &mut R,
        transport: &mut T,
        refresh: &mut F,
    ) -> Result<DownloadedRemoteMedia, RemoteMediaAcquisitionError>
    where
        F: FnMut() -> Result<Url, RemoteMediaAcquisitionError>,
    {
        if cancellation.is_cancelled() {
            return Err(RemoteMediaAcquisitionError::Cancelled);
        }

        let mut current = self
            .validator
            .validate_remote_url(url.as_str())
            .map_err(RemoteMediaAcquisitionError::Policy)?;
        current.set_fragment(None);
        let directory = create_private_remote_temp_dir(self.options.temporary_root.as_deref())?;
        let partial_path = directory.path().join("source.media.part");
        let final_path = directory.path().join("source.media");
        let mut redirects = 0_u8;
        let mut completed_bytes = 0_u64;

        loop {
            if cancellation.is_cancelled() {
                return Err(RemoteMediaAcquisitionError::Cancelled);
            }

            if completed_bytes != 0 {
                let mut refreshed = refresh()?;
                refreshed.set_fragment(None);
                current = self
                    .validator
                    .validate_remote_url(refreshed.as_str())
                    .map_err(RemoteMediaAcquisitionError::Policy)?;
                // Each refreshed source starts its own checked redirect chain.
                redirects = 0;
            }

            let range =
                RequestedRange::for_start(completed_bytes, self.options.max_download_bytes)?;
            let mut response = loop {
                let pinned_target = self.resolve_pinned_target(&current, resolver)?;
                let response = transport.get(
                    &current,
                    &pinned_target,
                    range,
                    self.validator.policy().allow_insecure_http,
                    &self.options,
                )?;

                if !is_supported_redirect_status(response.status) {
                    break response;
                }
                if redirects >= self.options.max_redirects {
                    return Err(RemoteMediaAcquisitionError::RedirectLimitExceeded);
                }
                let Some(location) = response.location else {
                    return Err(RemoteMediaAcquisitionError::InvalidRedirect);
                };
                let mut redirect_url = current
                    .join(&location)
                    .map_err(|_| RemoteMediaAcquisitionError::InvalidRedirect)?;
                redirect_url.set_fragment(None);
                // Validate the location before scheduling the next request.
                // DNS validation happens at the beginning of that next loop,
                // immediately before the pinned connection is made.
                self.validator
                    .validate_remote_url(redirect_url.as_str())
                    .map_err(RemoteMediaAcquisitionError::Policy)?;
                current = redirect_url;
                redirects += 1;
            };

            if response.status == 200 {
                // A server that ignores a range request can only safely be
                // accepted for the first response, because it represents the
                // complete object rather than a continuation.
                if completed_bytes != 0 {
                    return Err(RemoteMediaAcquisitionError::InvalidPartialResponse);
                }
                if response
                    .content_length
                    .is_some_and(|length| length > self.options.max_download_bytes)
                {
                    return Err(RemoteMediaAcquisitionError::ResponseTooLarge {
                        maximum_bytes: self.options.max_download_bytes,
                    });
                }
                copy_response_to_private_file(
                    &mut response.body,
                    &partial_path,
                    self.options.max_download_bytes,
                    true,
                    cancellation,
                )?;
                fs::rename(&partial_path, &final_path)
                    .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)?;

                return Ok(DownloadedRemoteMedia {
                    directory,
                    input: AudioInput::LocalPath(final_path),
                });
            }

            if response.status != 206 {
                return Err(RemoteMediaAcquisitionError::UnexpectedResponse);
            }

            let content_range = response
                .content_range
                .as_deref()
                .and_then(ParsedContentRange::parse)
                .ok_or(RemoteMediaAcquisitionError::InvalidPartialResponse)?;
            if content_range.start != range.start || content_range.end > range.end {
                return Err(RemoteMediaAcquisitionError::InvalidPartialResponse);
            }
            if content_range.total > self.options.max_download_bytes {
                return Err(RemoteMediaAcquisitionError::ResponseTooLarge {
                    maximum_bytes: self.options.max_download_bytes,
                });
            }
            let expected_length = content_range
                .end
                .checked_sub(content_range.start)
                .and_then(|length| length.checked_add(1))
                .ok_or(RemoteMediaAcquisitionError::InvalidPartialResponse)?;
            let written = copy_response_to_private_file(
                &mut response.body,
                &partial_path,
                self.options
                    .max_download_bytes
                    .checked_sub(completed_bytes)
                    .ok_or(RemoteMediaAcquisitionError::ResponseTooLarge {
                        maximum_bytes: self.options.max_download_bytes,
                    })?,
                completed_bytes == 0,
                cancellation,
            )?;
            if written != expected_length {
                return Err(RemoteMediaAcquisitionError::InvalidPartialResponse);
            }
            completed_bytes = content_range
                .end
                .checked_add(1)
                .ok_or(RemoteMediaAcquisitionError::InvalidPartialResponse)?;
            if completed_bytes != content_range.total {
                continue;
            }
            fs::rename(&partial_path, &final_path)
                .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)?;

            return Ok(DownloadedRemoteMedia {
                directory,
                input: AudioInput::LocalPath(final_path),
            });
        }
    }

    fn resolve_pinned_target<R: DnsResolver>(
        &self,
        url: &Url,
        resolver: &mut R,
    ) -> Result<PinnedTarget, RemoteMediaAcquisitionError> {
        let host = url.host().ok_or(RemoteMediaAcquisitionError::Policy(
            MediaError::InvalidRemoteUrl,
        ))?;
        match host {
            Host::Ipv4(address) => {
                self.validator
                    .validate_resolved_address(IpAddr::V4(address))
                    .map_err(RemoteMediaAcquisitionError::Policy)?;
                Ok(PinnedTarget::Literal)
            }
            Host::Ipv6(address) => {
                self.validator
                    .validate_resolved_address(IpAddr::V6(address))
                    .map_err(RemoteMediaAcquisitionError::Policy)?;
                Ok(PinnedTarget::Literal)
            }
            Host::Domain(hostname) => {
                let addresses = resolver.resolve(hostname)?;
                if addresses.is_empty() {
                    return Err(RemoteMediaAcquisitionError::DnsResolution);
                }
                let mut pinned_addresses = Vec::with_capacity(addresses.len());
                for address in addresses {
                    self.validator
                        .validate_resolved_address(address)
                        .map_err(RemoteMediaAcquisitionError::Policy)?;
                    // Port zero tells reqwest to retain the URL's explicit or
                    // scheme-default port while still pinning the IP address.
                    pinned_addresses.push(SocketAddr::new(address, 0));
                }
                Ok(PinnedTarget::Domain {
                    hostname: hostname.to_owned(),
                    addresses: pinned_addresses,
                })
            }
        }
    }
}

fn create_private_remote_temp_dir(
    root: Option<&Path>,
) -> Result<TempDir, RemoteMediaAcquisitionError> {
    let mut builder = TempDirBuilder::new();
    builder.prefix("subtitler-media-");
    match root {
        Some(root) => {
            fs::create_dir_all(root).map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)?;
            builder
                .tempdir_in(root)
                .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)
        }
        None => builder
            .tempdir()
            .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage),
    }
}

fn copy_response_to_private_file(
    response: &mut dyn Read,
    path: &Path,
    maximum_bytes: u64,
    create_new: bool,
    cancellation: &ExtractionCancellation,
) -> Result<u64, RemoteMediaAcquisitionError> {
    let mut output_options = OpenOptions::new();
    output_options.write(true);
    if create_new {
        output_options.create_new(true);
    } else {
        output_options.append(true);
    }
    let mut output = output_options
        .open(path)
        .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut written = 0_u64;

    loop {
        if cancellation.is_cancelled() {
            return Err(RemoteMediaAcquisitionError::Cancelled);
        }
        let read = response
            .read(&mut buffer)
            .map_err(|_| RemoteMediaAcquisitionError::Network)?;
        if read == 0 {
            break;
        }
        written = written
            .checked_add(read as u64)
            .ok_or(RemoteMediaAcquisitionError::ResponseTooLarge { maximum_bytes })?;
        if written > maximum_bytes {
            return Err(RemoteMediaAcquisitionError::ResponseTooLarge { maximum_bytes });
        }
        output
            .write_all(&buffer[..read])
            .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)?;
    }
    output
        .flush()
        .map_err(|_| RemoteMediaAcquisitionError::TemporaryStorage)?;
    Ok(written)
}

fn is_supported_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PinnedTarget {
    /// A public IP literal needs no DNS lookup; its URL host is already the
    /// validated endpoint.
    Literal,
    /// A domain is always supplied to reqwest with only the approved resolver
    /// answers from the immediately preceding manual lookup.
    Domain {
        hostname: String,
        addresses: Vec<SocketAddr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestedRange {
    start: u64,
    end: u64,
}

impl RequestedRange {
    fn for_start(start: u64, maximum_bytes: u64) -> Result<Self, RemoteMediaAcquisitionError> {
        if start >= maximum_bytes {
            return Err(RemoteMediaAcquisitionError::ResponseTooLarge { maximum_bytes });
        }
        let end = start
            .saturating_add(FINITE_RANGE_CHUNK_BYTES - 1)
            .min(maximum_bytes - 1);
        Ok(Self { start, end })
    }

    fn header_value(self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedContentRange {
    start: u64,
    end: u64,
    total: u64,
}

impl ParsedContentRange {
    fn parse(value: &str) -> Option<Self> {
        let value = value.strip_prefix("bytes ")?;
        let (range, total) = value.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse::<u64>().ok()?;
        let end = end.parse::<u64>().ok()?;
        let total = total.parse::<u64>().ok()?;
        (total != 0 && start <= end && end < total).then_some(Self { start, end, total })
    }
}

trait DnsResolver {
    fn resolve(&mut self, hostname: &str) -> Result<Vec<IpAddr>, RemoteMediaAcquisitionError>;
}

struct SystemDnsResolver;

impl DnsResolver for SystemDnsResolver {
    fn resolve(&mut self, hostname: &str) -> Result<Vec<IpAddr>, RemoteMediaAcquisitionError> {
        (hostname, 0)
            .to_socket_addrs()
            .map_err(|_| RemoteMediaAcquisitionError::DnsResolution)
            .map(|addresses| addresses.map(|address| address.ip()).collect())
    }
}

struct RemoteHttpResponse {
    status: u16,
    location: Option<String>,
    content_range: Option<String>,
    content_length: Option<u64>,
    body: Box<dyn Read>,
}

trait PinnedTransport {
    fn get(
        &mut self,
        url: &Url,
        target: &PinnedTarget,
        range: RequestedRange,
        allow_insecure_http: bool,
        options: &RemoteDownloadOptions,
    ) -> Result<RemoteHttpResponse, RemoteMediaAcquisitionError>;
}

struct ReqwestPinnedTransport;

impl PinnedTransport for ReqwestPinnedTransport {
    fn get(
        &mut self,
        url: &Url,
        target: &PinnedTarget,
        range: RequestedRange,
        allow_insecure_http: bool,
        options: &RemoteDownloadOptions,
    ) -> Result<RemoteHttpResponse, RemoteMediaAcquisitionError> {
        let mut builder = Client::builder()
            // Redirects must be returned to the acquisition loop so the next
            // URL can be policy-checked and independently DNS-pinned.
            .redirect(Policy::none())
            // Never inherit a system/environment proxy, which could otherwise
            // receive a direct-media URL or make the destination opaque.
            .no_proxy()
            .referer(false)
            // Keep source bytes raw so the configured byte budget matches the
            // amount written to the private cache.
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            // A new client is constructed per hop. Restricting to HTTP/1 and
            // disabling idle pooling avoids connection coalescing/reuse across
            // differently pinned requests.
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout)
            .use_rustls_tls();
        if !allow_insecure_http {
            builder = builder.https_only(true);
        }
        if let PinnedTarget::Domain {
            hostname,
            addresses,
        } = target
        {
            // `hostname` is exactly the name manually resolved above. The
            // client receives no unvalidated resolver result for this hop.
            builder = builder.resolve_to_addrs(hostname, addresses);
        }
        let client = builder
            .build()
            .map_err(|_| RemoteMediaAcquisitionError::Network)?;
        let response = client
            .get(url.as_str())
            .header(ACCEPT_ENCODING, "identity")
            // Strict media CDNs can reject an open-ended request, so each
            // request uses a bounded finite range. A normal server may ignore
            // it and return the whole object with 200 on the first request.
            .header(RANGE, range.header_value())
            .send()
            .map_err(|_| RemoteMediaAcquisitionError::Network)?;
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(RemoteHttpResponse {
            status: response.status().as_u16(),
            location,
            content_range,
            content_length: response.content_length(),
            body: Box::new(response),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Cursor;

    #[derive(Default)]
    struct FakeResolver {
        answers: VecDeque<Result<Vec<IpAddr>, RemoteMediaAcquisitionError>>,
        calls: Vec<String>,
    }

    impl FakeResolver {
        fn with_answers(answers: impl IntoIterator<Item = Vec<IpAddr>>) -> Self {
            Self {
                answers: answers.into_iter().map(Ok).collect(),
                calls: Vec::new(),
            }
        }
    }

    impl DnsResolver for FakeResolver {
        fn resolve(&mut self, hostname: &str) -> Result<Vec<IpAddr>, RemoteMediaAcquisitionError> {
            self.calls.push(hostname.to_owned());
            self.answers
                .pop_front()
                .unwrap_or(Err(RemoteMediaAcquisitionError::DnsResolution))
        }
    }

    struct FakeTransport {
        responses: VecDeque<RemoteHttpResponse>,
        requests: Vec<(String, PinnedTarget, RequestedRange)>,
    }

    impl FakeTransport {
        fn with_responses(responses: impl IntoIterator<Item = RemoteHttpResponse>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                requests: Vec::new(),
            }
        }
    }

    impl PinnedTransport for FakeTransport {
        fn get(
            &mut self,
            url: &Url,
            target: &PinnedTarget,
            range: RequestedRange,
            _allow_insecure_http: bool,
            _options: &RemoteDownloadOptions,
        ) -> Result<RemoteHttpResponse, RemoteMediaAcquisitionError> {
            self.requests
                .push((url.as_str().to_owned(), target.clone(), range));
            self.responses
                .pop_front()
                .ok_or(RemoteMediaAcquisitionError::UnexpectedResponse)
        }
    }

    fn response(status: u16, location: Option<&str>, body: &[u8]) -> RemoteHttpResponse {
        RemoteHttpResponse {
            status,
            location: location.map(str::to_owned),
            content_range: None,
            content_length: Some(body.len() as u64),
            body: Box::new(Cursor::new(body.to_vec())),
        }
    }

    fn partial_response(content_range: &str, body: &[u8]) -> RemoteHttpResponse {
        RemoteHttpResponse {
            status: 206,
            location: None,
            content_range: Some(content_range.to_owned()),
            content_length: Some(body.len() as u64),
            body: Box::new(Cursor::new(body.to_vec())),
        }
    }

    fn acquirer_with(options: RemoteDownloadOptions) -> RemoteMediaAcquirer {
        RemoteMediaAcquirer::new(MediaSourceValidator::default(), options)
    }

    fn public_v4(value: [u8; 4]) -> IpAddr {
        IpAddr::from(value)
    }

    #[test]
    fn public_dns_result_is_pinned_and_downloaded_to_a_private_raii_file() {
        let root = tempfile::tempdir().unwrap();
        let options = RemoteDownloadOptions {
            temporary_root: Some(root.path().to_path_buf()),
            ..RemoteDownloadOptions::default()
        };
        let acquirer = acquirer_with(options);
        let mut resolver = FakeResolver::with_answers([vec![public_v4([93, 184, 216, 34])]]);
        let mut transport = FakeTransport::with_responses([response(200, None, b"media")]);

        let artifact = acquirer
            .acquire_with(
                &Url::parse("https://media.example.test/recording.mp4?token=secret").unwrap(),
                &ExtractionCancellation::new(),
                &mut resolver,
                &mut transport,
            )
            .unwrap();
        assert_eq!(resolver.calls, vec!["media.example.test"]);
        assert_eq!(transport.requests.len(), 1);
        assert_eq!(
            transport.requests[0].1,
            PinnedTarget::Domain {
                hostname: "media.example.test".to_owned(),
                addresses: vec![SocketAddr::from(([93, 184, 216, 34], 0))],
            }
        );
        assert_eq!(
            transport.requests[0].2,
            RequestedRange {
                start: 0,
                end: FINITE_RANGE_CHUNK_BYTES - 1,
            }
        );
        assert_eq!(fs::read(artifact.path()).unwrap(), b"media");
        assert!(!format!("{artifact:?}").contains("token=secret"));
        let directory = artifact.directory().to_path_buf();
        drop(artifact);
        assert!(!directory.exists());
    }

    #[test]
    fn private_literal_and_mapped_ipv6_addresses_are_rejected_before_transport() {
        let acquirer = RemoteMediaAcquirer::default();
        for url in [
            "https://127.0.0.1/recording.mp4",
            "https://[::ffff:127.0.0.1]/recording.mp4",
        ] {
            let mut resolver = FakeResolver::default();
            let mut transport = FakeTransport::with_responses([]);
            assert_eq!(
                acquirer
                    .acquire_with(
                        &Url::parse(url).unwrap(),
                        &ExtractionCancellation::new(),
                        &mut resolver,
                        &mut transport,
                    )
                    .unwrap_err(),
                RemoteMediaAcquisitionError::Policy(MediaError::PrivateNetworkUrl)
            );
            assert!(resolver.calls.is_empty());
            assert!(transport.requests.is_empty());
        }
    }

    #[test]
    fn private_dns_result_is_rejected_before_a_pinned_request() {
        let acquirer = RemoteMediaAcquirer::default();
        let mut resolver = FakeResolver::with_answers([vec![public_v4([127, 0, 0, 1])]]);
        let mut transport = FakeTransport::with_responses([]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &ExtractionCancellation::new(),
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::Policy(MediaError::PrivateNetworkUrl)
        );
        assert_eq!(resolver.calls, vec!["media.example.test"]);
        assert!(transport.requests.is_empty());
    }

    #[test]
    fn mixed_dns_answers_fail_closed_before_transport_can_choose_an_address() {
        let acquirer = RemoteMediaAcquirer::default();
        let mut resolver = FakeResolver::with_answers([vec![
            public_v4([93, 184, 216, 34]),
            public_v4([10, 0, 0, 7]),
        ]]);
        let mut transport = FakeTransport::with_responses([]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &ExtractionCancellation::new(),
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::Policy(MediaError::PrivateNetworkUrl)
        );
        assert!(transport.requests.is_empty());
    }

    #[test]
    fn redirects_are_revalidated_and_private_followup_dns_never_reaches_transport() {
        let acquirer = RemoteMediaAcquirer::default();
        let mut resolver = FakeResolver::with_answers([
            vec![public_v4([93, 184, 216, 34])],
            vec![public_v4([10, 0, 0, 7])],
        ]);
        let mut transport = FakeTransport::with_responses([response(
            302,
            Some("https://cdn.example.test/recording.mp4"),
            b"",
        )]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &ExtractionCancellation::new(),
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::Policy(MediaError::PrivateNetworkUrl)
        );
        assert_eq!(
            resolver.calls,
            vec!["media.example.test", "cdn.example.test"]
        );
        assert_eq!(transport.requests.len(), 1);
    }

    #[test]
    fn redirect_to_private_literal_or_http_downgrade_is_rejected_before_next_request() {
        for location in [
            "https://127.0.0.1/recording.mp4",
            "http://cdn.example.test/recording.mp4",
        ] {
            let acquirer = RemoteMediaAcquirer::default();
            let mut resolver = FakeResolver::with_answers([vec![public_v4([93, 184, 216, 34])]]);
            let mut transport = FakeTransport::with_responses([response(302, Some(location), b"")]);

            assert!(matches!(
                acquirer
                    .acquire_with(
                        &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                        &ExtractionCancellation::new(),
                        &mut resolver,
                        &mut transport,
                    )
                    .unwrap_err(),
                RemoteMediaAcquisitionError::Policy(MediaError::PrivateNetworkUrl)
                    | RemoteMediaAcquisitionError::Policy(MediaError::UnsupportedScheme)
            ));
            assert_eq!(transport.requests.len(), 1);
        }
    }

    #[test]
    fn redirect_chain_is_bounded() {
        let acquirer = acquirer_with(RemoteDownloadOptions {
            max_redirects: 1,
            ..RemoteDownloadOptions::default()
        });
        let public = public_v4([93, 184, 216, 34]);
        let mut resolver = FakeResolver::with_answers([vec![public], vec![public]]);
        let mut transport = FakeTransport::with_responses([
            response(302, Some("https://cdn.example.test/one.mp4"), b""),
            response(302, Some("https://edge.example.test/two.mp4"), b""),
        ]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &ExtractionCancellation::new(),
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::RedirectLimitExceeded
        );
        assert_eq!(transport.requests.len(), 2);
        assert_eq!(
            resolver.calls,
            vec!["media.example.test", "cdn.example.test"]
        );
    }

    #[test]
    fn streamed_response_cap_removes_partial_private_data() {
        let root = tempfile::tempdir().unwrap();
        let options = RemoteDownloadOptions {
            max_download_bytes: 3,
            temporary_root: Some(root.path().to_path_buf()),
            ..RemoteDownloadOptions::default()
        };
        let acquirer = acquirer_with(options);
        let mut resolver = FakeResolver::with_answers([vec![public_v4([93, 184, 216, 34])]]);
        let mut response = response(200, None, b"oversized");
        // Simulate a server that omits or lies about Content-Length; the copy
        // loop must enforce the cap independently.
        response.content_length = None;
        let mut transport = FakeTransport::with_responses([response]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &ExtractionCancellation::new(),
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::ResponseTooLarge { maximum_bytes: 3 }
        );
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn finite_partial_ranges_are_validated_and_assembled_in_order() {
        let root = tempfile::tempdir().unwrap();
        let options = RemoteDownloadOptions {
            temporary_root: Some(root.path().to_path_buf()),
            ..RemoteDownloadOptions::default()
        };
        let acquirer = acquirer_with(options);
        let public = public_v4([93, 184, 216, 34]);
        let mut resolver = FakeResolver::with_answers([vec![public], vec![public]]);
        let mut transport = FakeTransport::with_responses([
            partial_response("bytes 0-2/6", b"med"),
            partial_response("bytes 3-5/6", b"ia!"),
        ]);

        let artifact = acquirer
            .acquire_with(
                &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                &ExtractionCancellation::new(),
                &mut resolver,
                &mut transport,
            )
            .unwrap();

        assert_eq!(fs::read(artifact.path()).unwrap(), b"media!");
        assert_eq!(transport.requests.len(), 2);
        assert_eq!(transport.requests[0].2.start, 0);
        assert_eq!(transport.requests[1].2.start, 3);
    }

    #[test]
    fn refreshed_url_is_used_only_for_a_verified_continuation_range() {
        let acquirer = RemoteMediaAcquirer::default();
        let public = public_v4([93, 184, 216, 34]);
        let mut resolver = FakeResolver::with_answers([vec![public], vec![public]]);
        let mut transport = FakeTransport::with_responses([
            partial_response("bytes 0-2/6", b"med"),
            partial_response("bytes 3-5/6", b"ia!"),
        ]);
        let initial = Url::parse("https://first.example.test/recording.mp4").unwrap();
        let mut refreshes = 0;
        let mut refresher = || {
            refreshes += 1;
            Ok(Url::parse("https://renewed.example.test/recording.mp4").unwrap())
        };

        let artifact = acquirer
            .acquire_with_refresher(
                &initial,
                &ExtractionCancellation::new(),
                &mut resolver,
                &mut transport,
                &mut refresher,
            )
            .unwrap();

        assert_eq!(fs::read(artifact.path()).unwrap(), b"media!");
        assert_eq!(refreshes, 1);
        assert_eq!(
            transport
                .requests
                .iter()
                .map(|request| request.0.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://first.example.test/recording.mp4",
                "https://renewed.example.test/recording.mp4"
            ]
        );
    }

    #[test]
    fn malformed_partial_range_is_rejected_and_private_data_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let options = RemoteDownloadOptions {
            temporary_root: Some(root.path().to_path_buf()),
            ..RemoteDownloadOptions::default()
        };
        let acquirer = acquirer_with(options);
        let mut resolver = FakeResolver::with_answers([vec![public_v4([93, 184, 216, 34])]]);
        let mut transport = FakeTransport::with_responses([response(206, None, b"media")]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &ExtractionCancellation::new(),
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::InvalidPartialResponse
        );
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn cancellation_prevents_the_first_network_request() {
        let acquirer = RemoteMediaAcquirer::default();
        let cancellation = ExtractionCancellation::new();
        cancellation.cancel();
        let mut resolver = FakeResolver::default();
        let mut transport = FakeTransport::with_responses([]);

        assert_eq!(
            acquirer
                .acquire_with(
                    &Url::parse("https://media.example.test/recording.mp4").unwrap(),
                    &cancellation,
                    &mut resolver,
                    &mut transport,
                )
                .unwrap_err(),
            RemoteMediaAcquisitionError::Cancelled
        );
        assert!(resolver.calls.is_empty());
        assert!(transport.requests.is_empty());
    }
}
