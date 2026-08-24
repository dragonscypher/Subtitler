//! Narrow, local audio acquisition for supported YouTube recording pages.
//!
//! A modern YouTube media URL is frequently short-lived and can be bound to
//! the downloader's request behavior. Asking `yt-dlp` for that URL and then
//! re-fetching it with a different HTTP client causes legitimate 403 failures.
//! This adapter therefore lets the bundled `yt-dlp` process retrieve one
//! audio-only artifact itself. It uses a fixed argument vector, ignores all
//! user configuration, writes under a private RAII directory, and passes only
//! a local path to FFmpeg. It never reads browser cookies or profiles.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use subtitler_media::{AudioInput, ExtractionCancellation};
use tempfile::{Builder as TempDirBuilder, TempDir};
use thiserror::Error;
use url::Url;

const YOUTUBE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const YOUTUBE_DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_YOUTUBE_AUDIO_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const YOUTUBE_MAX_FILESIZE_ARGUMENT: &str = "4G";
const YOUTUBE_OUTPUT_BASENAME: &str = "source.%(ext)s";
/// Prefer a true audio stream, but permit a bounded combined representation
/// when YouTube's currently selected client exposes no audio-only format.
/// Downstream FFmpeg still decodes audio only; this never requests 4K video.
const YOUTUBE_AUDIO_FORMAT_SELECTOR: &str =
    "bestaudio[acodec!=none]/best[acodec!=none][ext=mp4][height<=720]/best[acodec!=none][height<=720]";
/// yt-dlp diagnostics can contain transient request details. Capture only a
/// bounded amount in memory to classify a safe error category; never persist
/// or return the raw bytes.
const MAX_YOUTUBE_DIAGNOSTIC_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug)]
pub struct YoutubePageResolver {
    executable: PathBuf,
    deno_executable: Option<PathBuf>,
    po_token_provider: Option<YoutubePoTokenProvider>,
}

/// An explicitly installed, local yt-dlp PO-token provider. The provider
/// remains an implementation detail of page acquisition: short-lived data is
/// directed into the job's private temporary directory and never reaches the
/// extension, an export, or a durable cache.
#[derive(Clone, Debug)]
pub enum YoutubePoTokenProvider {
    BgutilScript {
        plugin_directory: PathBuf,
        server_home: PathBuf,
    },
    /// WPC starts an isolated, minimized Chrome instance solely to mint a
    /// per-video PO token. It does not attach to, inspect, or copy the user's
    /// active Chrome profile or cookies.
    WebPoClient {
        plugin_directory: PathBuf,
        browser_path: PathBuf,
    },
}

/// Owns the private local media retrieved by the fixed YouTube adapter.
/// Dropping it removes partial or complete media before the artifact can leak
/// into FFmpeg command lines, job status, or exports.
pub struct DownloadedYoutubeMedia {
    directory: TempDir,
    input: AudioInput,
}

impl DownloadedYoutubeMedia {
    pub fn input(&self) -> &AudioInput {
        // Keep the private directory demonstrably owned for as long as the
        // decoder borrows its local path.
        let _ = self.directory.path();
        &self.input
    }
}

impl std::fmt::Debug for DownloadedYoutubeMedia {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DownloadedYoutubeMedia")
            .field("path", &"<private downloaded media>")
            .finish()
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum YoutubeResolutionError {
    #[error("the YouTube media resolver is not installed")]
    ToolUnavailable,
    #[error("the supplied page is not a supported YouTube recording page")]
    UnsupportedPage,
    #[error("the YouTube media resolver timed out")]
    TimedOut,
    #[error("the YouTube media resolver could not create private temporary media storage")]
    TemporaryStorage,
    #[error("the YouTube audio exceeds Subtitler's private temporary-media cache limit")]
    OutputTooLarge,
    #[error("the YouTube media retrieval was cancelled")]
    Cancelled,
    #[error("the YouTube media resolver did not produce one usable audio file")]
    InvalidOutput,
    #[error("the YouTube media resolver could not access this recording")]
    ResolutionFailed(YoutubeResolverFailureReason),
}

/// A deliberately non-sensitive classification of a local yt-dlp failure.
/// It contains neither a response body nor a request URL, token, or cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YoutubeResolverFailureReason {
    BotCheck,
    AccessDenied,
    FormatUnavailable,
    RuntimeUnavailable,
    Network,
    Unknown,
}

impl YoutubePageResolver {
    pub fn new(
        executable: PathBuf,
        deno_executable: Option<PathBuf>,
    ) -> Result<Self, YoutubeResolutionError> {
        if executable.is_file() {
            Ok(Self {
                executable,
                deno_executable,
                po_token_provider: None,
            })
        } else {
            Err(YoutubeResolutionError::ToolUnavailable)
        }
    }

    /// Enable a maintained provider only when both of its local, installer
    /// controlled roots exist. The provider itself is supplied to yt-dlp via
    /// its normal plugin interface; no browser token, cookie, or profile is
    /// ever imported.
    pub fn with_po_token_provider(
        mut self,
        plugin_directory: PathBuf,
        server_home: PathBuf,
    ) -> Result<Self, YoutubeResolutionError> {
        if !plugin_directory.is_dir() || !server_home.is_dir() {
            return Err(YoutubeResolutionError::ToolUnavailable);
        }
        self.po_token_provider = Some(YoutubePoTokenProvider::BgutilScript {
            plugin_directory,
            server_home,
        });
        Ok(self)
    }

    /// Prefer a locally packaged WebPoClient provider after a BotGuard based
    /// provider has proven unable to retrieve the requested recording. The
    /// provider is allowed to launch only the configured Chrome executable,
    /// and gets no browser profile, cookie, or native-messaging access.
    pub fn with_webpo_client_provider(
        mut self,
        plugin_directory: PathBuf,
        browser_path: PathBuf,
    ) -> Result<Self, YoutubeResolutionError> {
        if !plugin_directory.is_dir() || !browser_path.is_file() {
            return Err(YoutubeResolutionError::ToolUnavailable);
        }
        self.po_token_provider = Some(YoutubePoTokenProvider::WebPoClient {
            plugin_directory,
            browser_path,
        });
        Ok(self)
    }

    /// Acquire one audio-only artifact into a private directory. `yt-dlp` is
    /// deliberately kept at the direct-media boundary: it follows the signed
    /// URL behavior it negotiated, while every downstream component receives
    /// only `AudioInput::LocalPath`.
    pub fn download_audio(
        &self,
        page_url: &str,
        temporary_root: &Path,
        cancellation: &ExtractionCancellation,
    ) -> Result<DownloadedYoutubeMedia, YoutubeResolutionError> {
        let page_url = canonical_youtube_page_url(page_url)?;
        fs::create_dir_all(temporary_root).map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        let directory = TempDirBuilder::new()
            .prefix("subtitler-youtube-")
            .tempdir_in(temporary_root)
            .map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        // The PO-token script creates its own cache. Keep it separate from the
        // audio output so cache entries can never be mistaken for a downloaded
        // media artifact (and so the audio-size limit measures audio only).
        let media_directory = directory.path().join("media");
        let provider_cache_directory = directory.path().join("provider-cache");
        fs::create_dir_all(&media_directory)
            .map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        fs::create_dir_all(&provider_cache_directory)
            .map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        let output_template = media_directory.join(YOUTUBE_OUTPUT_BASENAME);
        let arguments = fixed_download_arguments(
            &page_url,
            &output_template,
            self.deno_executable.as_deref(),
            self.po_token_provider.as_ref(),
        );
        let mut child = Command::new(&self.executable)
            .args(arguments)
            .stdin(Stdio::null())
            // bgutil's script mode normally caches PO tokens under the user
            // profile. Scope that cache to this RAII directory instead so the
            // short-lived token is removed alongside the private audio.
            .env("XDG_CACHE_HOME", &provider_cache_directory)
            // The product does not display yt-dlp output. Keeping its pipes
            // detached also prevents an inherited pipe from holding a job open
            // after a completed download.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| YoutubeResolutionError::ToolUnavailable)?;
        let stderr = child
            .stderr
            .take()
            .ok_or(YoutubeResolutionError::ToolUnavailable)?;
        let diagnostic_reader = thread::spawn(move || capture_resolver_diagnostics(stderr));

        let deadline = Instant::now() + YOUTUBE_DOWNLOAD_TIMEOUT;
        let status = loop {
            if cancellation.is_cancelled() {
                stop_child(&mut child);
                let _ = diagnostic_reader.join();
                return Err(YoutubeResolutionError::Cancelled);
            }
            if downloaded_size(&media_directory)? > MAX_YOUTUBE_AUDIO_BYTES {
                stop_child(&mut child);
                let _ = diagnostic_reader.join();
                return Err(YoutubeResolutionError::OutputTooLarge);
            }
            if let Some(status) = child.try_wait().map_err(|_| {
                YoutubeResolutionError::ResolutionFailed(YoutubeResolverFailureReason::Unknown)
            })? {
                break status;
            }
            if Instant::now() >= deadline {
                stop_child(&mut child);
                let _ = diagnostic_reader.join();
                return Err(YoutubeResolutionError::TimedOut);
            }
            thread::sleep(YOUTUBE_DOWNLOAD_POLL_INTERVAL);
        };
        if !status.success() {
            let diagnostics = diagnostic_reader.join().unwrap_or_default();
            return Err(YoutubeResolutionError::ResolutionFailed(
                classify_resolver_failure(&diagnostics),
            ));
        }
        let _ = diagnostic_reader.join();
        let path = single_downloaded_audio_file(&media_directory)?;
        Ok(DownloadedYoutubeMedia {
            input: AudioInput::LocalPath(path),
            directory,
        })
    }
}

fn capture_resolver_diagnostics(mut stderr: impl Read) -> Vec<u8> {
    let mut diagnostics = Vec::new();
    let _ = stderr
        .by_ref()
        .take(MAX_YOUTUBE_DIAGNOSTIC_BYTES)
        .read_to_end(&mut diagnostics);
    diagnostics
}

fn classify_resolver_failure(diagnostics: &[u8]) -> YoutubeResolverFailureReason {
    let text = String::from_utf8_lossy(diagnostics).to_ascii_lowercase();
    if text.contains("sign in to confirm")
        || text.contains("not a bot")
        || text.contains("bot check")
    {
        return YoutubeResolverFailureReason::BotCheck;
    }
    if text.contains("http error 403")
        || text.contains("403 forbidden")
        || text.contains("access denied")
    {
        return YoutubeResolverFailureReason::AccessDenied;
    }
    if text.contains("requested format is not available")
        || text.contains("format is not available")
    {
        return YoutubeResolverFailureReason::FormatUnavailable;
    }
    if (text.contains("deno") || text.contains("javascript runtime"))
        && (text.contains("not found") || text.contains("failed") || text.contains("unavailable"))
    {
        return YoutubeResolverFailureReason::RuntimeUnavailable;
    }
    if text.contains("timed out")
        || text.contains("network is unreachable")
        || text.contains("connection reset")
        || text.contains("connection refused")
        || text.contains("unable to download")
    {
        return YoutubeResolverFailureReason::Network;
    }
    YoutubeResolverFailureReason::Unknown
}

fn fixed_download_arguments(
    page_url: &Url,
    output_template: &Path,
    deno_executable: Option<&Path>,
    po_token_provider: Option<&YoutubePoTokenProvider>,
) -> Vec<OsString> {
    let mut arguments = vec![
        "--ignore-config".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--no-progress".into(),
        "--no-cache-dir".into(),
        "--no-part".into(),
        "--no-continue".into(),
        "--no-overwrites".into(),
        "--no-write-info-json".into(),
        "--no-write-playlist-metafiles".into(),
        "--no-write-comments".into(),
        "--format".into(),
        YOUTUBE_AUDIO_FORMAT_SELECTOR.into(),
        "--max-filesize".into(),
        YOUTUBE_MAX_FILESIZE_ARGUMENT.into(),
        "--output".into(),
        output_template.as_os_str().to_os_string(),
    ];
    if let Some(provider) = po_token_provider {
        match provider {
            YoutubePoTokenProvider::BgutilScript {
                plugin_directory,
                server_home,
            } => {
                arguments.push("--plugin-dirs".into());
                arguments.push(plugin_directory.as_os_str().to_os_string());
                arguments.push("--extractor-args".into());
                let mut provider_argument = OsString::from("youtubepot-bgutilscript:server_home=");
                provider_argument.push(server_home.as_os_str());
                arguments.push(provider_argument);
            }
            YoutubePoTokenProvider::WebPoClient {
                plugin_directory,
                browser_path,
            } => {
                arguments.push("--plugin-dirs".into());
                arguments.push(plugin_directory.as_os_str().to_os_string());
                arguments.push("--extractor-args".into());
                // Current yt-dlp guidance recommends mweb with a provider for
                // GVS requests. Keep the browser path separate from the
                // user-facing Chrome session and do not pass cookies.
                let mut provider_argument =
                    OsString::from("youtube:player_client=mweb;youtubepot-wpc:browser_path=");
                provider_argument.push(browser_path.as_os_str());
                arguments.push(provider_argument);
            }
        }
    }
    if let Some(deno_executable) = deno_executable {
        arguments.push("--js-runtimes".into());
        let mut runtime = OsString::from("deno:");
        runtime.push(deno_executable.as_os_str());
        arguments.push(runtime);
        // yt-dlp's maintained EJS component performs the current YouTube
        // JavaScript challenge with Deno's restricted permissions. It is not
        // a browser cookie/profile handoff or a caption path.
        arguments.push("--remote-components".into());
        arguments.push("ejs:github".into());
    }
    arguments.extend(["--".into(), page_url.as_str().into()]);
    arguments
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn downloaded_size(directory: &Path) -> Result<u64, YoutubeResolutionError> {
    let mut total = 0_u64;
    for entry in fs::read_dir(directory).map_err(|_| YoutubeResolutionError::TemporaryStorage)? {
        let entry = entry.map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        let file_type = entry
            .file_type()
            .map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        if file_type.is_file() {
            total = total
                .checked_add(
                    entry
                        .metadata()
                        .map_err(|_| YoutubeResolutionError::TemporaryStorage)?
                        .len(),
                )
                .ok_or(YoutubeResolutionError::OutputTooLarge)?;
        }
    }
    Ok(total)
}

fn single_downloaded_audio_file(directory: &Path) -> Result<PathBuf, YoutubeResolutionError> {
    let total = downloaded_size(directory)?;
    if total == 0 || total > MAX_YOUTUBE_AUDIO_BYTES {
        return Err(YoutubeResolutionError::OutputTooLarge);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).map_err(|_| YoutubeResolutionError::TemporaryStorage)? {
        let entry = entry.map_err(|_| YoutubeResolutionError::TemporaryStorage)?;
        if entry
            .file_type()
            .map_err(|_| YoutubeResolutionError::TemporaryStorage)?
            .is_file()
        {
            files.push(entry.path());
        }
    }
    if files.len() != 1 {
        return Err(YoutubeResolutionError::InvalidOutput);
    }
    let path = files.into_iter().next().expect("length verified");
    let extension = path.extension().and_then(|extension| extension.to_str());
    if extension.map_or(true, str::is_empty)
        || path
            .extension()
            .is_some_and(|extension| extension == "part")
    {
        return Err(YoutubeResolutionError::InvalidOutput);
    }
    Ok(path)
}

/// True only for the small set of normal YouTube video page routes that this
/// adapter accepts. It exposes no URL normalization or signed media data.
pub fn supports_youtube_page(value: &str) -> bool {
    canonical_youtube_page_url(value).is_ok()
}

fn canonical_youtube_page_url(value: &str) -> Result<Url, YoutubeResolutionError> {
    if value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        return Err(YoutubeResolutionError::UnsupportedPage);
    }
    let mut url = Url::parse(value).map_err(|_| YoutubeResolutionError::UnsupportedPage)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(YoutubeResolutionError::UnsupportedPage);
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let supported = if host == "youtu.be" {
        let segments = url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        segments.len() == 1
    } else if host == "youtube.com" || host.ends_with(".youtube.com") {
        match url.path() {
            "/watch" => url
                .query_pairs()
                .any(|(key, value)| key == "v" && !value.is_empty()),
            path if path.starts_with("/embed/") || path.starts_with("/shorts/") => url
                .path_segments()
                .map(|segments| segments.filter(|segment| !segment.is_empty()).count() == 2)
                .unwrap_or(false),
            _ => false,
        }
    } else {
        false
    };
    if !supported {
        return Err(YoutubeResolutionError::UnsupportedPage);
    }
    url.set_fragment(None);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_supported_credential_free_youtube_recording_pages() {
        assert!(canonical_youtube_page_url("https://youtu.be/ESjPc7I5h_Q?si=test").is_ok());
        assert!(canonical_youtube_page_url("https://www.youtube.com/watch?v=abc").is_ok());
        assert!(canonical_youtube_page_url("https://m.youtube.com/embed/abc").is_ok());
        assert!(canonical_youtube_page_url("https://www.youtube.com/shorts/abc").is_ok());
        assert!(canonical_youtube_page_url("https://youtube.com.evil.test/watch?v=abc").is_err());
        assert!(canonical_youtube_page_url("https://user:secret@youtube.com/watch?v=abc").is_err());
        assert!(canonical_youtube_page_url("https://www.youtube.com/channel/abc").is_err());
        assert!(supports_youtube_page(
            "https://youtu.be/ESjPc7I5h_Q?si=test"
        ));
    }

    #[test]
    fn uses_a_fixed_private_audio_download_command() {
        let page_url = canonical_youtube_page_url("https://youtu.be/ESjPc7I5h_Q?si=test").unwrap();
        let provider = YoutubePoTokenProvider::BgutilScript {
            plugin_directory: PathBuf::from("C:/private/plugins"),
            server_home: PathBuf::from("C:/private/provider/server"),
        };
        let arguments = fixed_download_arguments(
            &page_url,
            Path::new("C:/private/source.%(ext)s"),
            Some(Path::new("C:/private/deno.exe")),
            Some(&provider),
        );
        let values = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(values.contains(&"--ignore-config".to_owned()));
        assert!(values.contains(&"--no-cache-dir".to_owned()));
        assert!(values.contains(&"--no-part".to_owned()));
        assert!(values.contains(&"--max-filesize".to_owned()));
        assert!(values.contains(&YOUTUBE_AUDIO_FORMAT_SELECTOR.to_owned()));
        assert!(values.contains(&"--js-runtimes".to_owned()));
        assert!(values.contains(&"deno:C:/private/deno.exe".to_owned()));
        assert!(values.contains(&"--plugin-dirs".to_owned()));
        assert!(values.contains(&"C:/private/plugins".to_owned()));
        assert!(values.contains(
            &"youtubepot-bgutilscript:server_home=C:/private/provider/server".to_owned()
        ));
        assert!(values.contains(&"--remote-components".to_owned()));
        assert!(values.contains(&"ejs:github".to_owned()));
        assert_eq!(values.last(), Some(&page_url.to_string()));
        assert!(!values.iter().any(|value| value.contains("--cookies")));
    }

    #[test]
    fn uses_the_isolated_wpc_provider_with_mweb_and_no_browser_profile() {
        let page_url = canonical_youtube_page_url("https://youtu.be/ESjPc7I5h_Q?si=test").unwrap();
        let provider = YoutubePoTokenProvider::WebPoClient {
            plugin_directory: PathBuf::from("C:/private/wpc-plugin"),
            browser_path: PathBuf::from("C:/Program Files/Google/Chrome/Application/chrome.exe"),
        };
        let values = fixed_download_arguments(
            &page_url,
            Path::new("C:/private/source.%(ext)s"),
            Some(Path::new("C:/private/deno.exe")),
            Some(&provider),
        )
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

        assert!(values.contains(&"C:/private/wpc-plugin".to_owned()));
        assert!(values.contains(
            &"youtube:player_client=mweb;youtubepot-wpc:browser_path=C:/Program Files/Google/Chrome/Application/chrome.exe"
                .to_owned()
        ));
        assert!(!values.iter().any(|value| value.contains("--cookies")));
        assert!(!values.iter().any(|value| value.contains("bgutilscript")));
    }

    #[test]
    fn optional_provider_is_not_implied_by_the_youtube_page() {
        let page_url = canonical_youtube_page_url("https://youtu.be/ESjPc7I5h_Q?si=test").unwrap();
        let values = fixed_download_arguments(
            &page_url,
            Path::new("C:/private/source.%(ext)s"),
            None,
            None,
        )
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert!(!values.iter().any(|value| value == "--plugin-dirs"));
        assert!(!values
            .iter()
            .any(|value| value.contains("youtubepot-bgutilscript")));
    }

    #[test]
    fn accepts_exactly_one_nonempty_downloaded_audio_file() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("source.m4a");
        fs::write(&expected, b"audio").unwrap();
        assert_eq!(
            single_downloaded_audio_file(directory.path()).unwrap(),
            expected
        );
    }

    #[test]
    fn rejects_missing_or_multiple_downloaded_files() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            single_downloaded_audio_file(empty.path()).unwrap_err(),
            YoutubeResolutionError::OutputTooLarge
        );
        let multiple = tempfile::tempdir().unwrap();
        fs::write(multiple.path().join("source.m4a"), b"audio").unwrap();
        fs::write(multiple.path().join("source.webm"), b"audio").unwrap();
        assert_eq!(
            single_downloaded_audio_file(multiple.path()).unwrap_err(),
            YoutubeResolutionError::InvalidOutput
        );
    }

    #[test]
    fn audio_selection_ignores_the_separate_provider_cache() {
        let working_directory = tempfile::tempdir().unwrap();
        let media_directory = working_directory.path().join("media");
        let provider_cache_directory = working_directory.path().join("provider-cache");
        fs::create_dir_all(&media_directory).unwrap();
        fs::create_dir_all(&provider_cache_directory).unwrap();
        let expected = media_directory.join("source.m4a");
        fs::write(&expected, b"audio").unwrap();
        fs::write(
            provider_cache_directory.join("short-lived-token-cache"),
            b"private",
        )
        .unwrap();

        assert_eq!(
            single_downloaded_audio_file(&media_directory).unwrap(),
            expected
        );
    }

    #[test]
    fn classifies_resolver_failures_without_retaining_their_text() {
        assert_eq!(
            classify_resolver_failure(b"ERROR: [youtube] Sign in to confirm you're not a bot"),
            YoutubeResolverFailureReason::BotCheck
        );
        assert_eq!(
            classify_resolver_failure(b"HTTP Error 403: Forbidden"),
            YoutubeResolverFailureReason::AccessDenied
        );
        assert_eq!(
            classify_resolver_failure(b"Requested format is not available"),
            YoutubeResolverFailureReason::FormatUnavailable
        );
        assert_eq!(
            classify_resolver_failure(b"Deno runtime not found"),
            YoutubeResolverFailureReason::RuntimeUnavailable
        );
        assert_eq!(
            classify_resolver_failure(b"network is unreachable"),
            YoutubeResolverFailureReason::Network
        );
        assert_eq!(
            classify_resolver_failure(b"unclassified failure"),
            YoutubeResolverFailureReason::Unknown
        );
    }
}
