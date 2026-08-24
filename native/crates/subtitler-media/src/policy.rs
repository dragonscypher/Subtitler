use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};
use subtitler_core::{AcquisitionReport, AcquisitionStrategy, MediaRequest, MediaSource};
use thiserror::Error;
use url::Url;

/// Conservative media-source policy. It accepts user-authorized HTTP(S)
/// media and local files, but never special browser schemes or URL-embedded
/// basic credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPolicy {
    pub allow_local_files: bool,
    /// Release builds process HTTPS media only. Explicit development fixture
    /// policies may enable ordinary HTTP without granting private-network
    /// access.
    pub allow_insecure_http: bool,
    /// Remote decoder input must not be able to probe loopback, LAN, or
    /// link-local services merely because a user pasted a media URL.
    pub allow_private_network_urls: bool,
}

impl Default for MediaPolicy {
    fn default() -> Self {
        Self {
            allow_local_files: true,
            allow_insecure_http: false,
            allow_private_network_urls: false,
        }
    }
}

/// A preflight validator. It reports the safe acquisition route but does not
/// itself download media, persist session state, or invoke FFmpeg.
#[derive(Clone, Debug, Default)]
pub struct MediaSourceValidator {
    policy: MediaPolicy,
}

impl MediaSourceValidator {
    pub fn new(policy: MediaPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &MediaPolicy {
        &self.policy
    }

    /// Select an acquisition strategy while enforcing explicit restrictions.
    ///
    /// When existing reliable captions are present, they are selected unless
    /// the caller expressly chooses "Generate with Subtitler." A page source
    /// always requires a browser-mediated, least-privilege discovery handoff;
    /// the native host must never scrape the browser cookie store.
    pub fn plan(
        &self,
        request: &MediaRequest,
        force_generate_with_subtitler: bool,
    ) -> Result<AcquisitionReport, MediaError> {
        if request.hints.drm_detected {
            return Err(MediaError::ProtectedMedia);
        }

        if !force_generate_with_subtitler
            && request
                .hints
                .existing_captions
                .iter()
                .any(|track| track.reliable)
        {
            return Ok(AcquisitionReport {
                strategy: AcquisitionStrategy::ExistingCaptions,
                summary: "Reliable captions are available and can be used immediately.".to_owned(),
                requires_user_action: false,
            });
        }

        match &request.source {
            MediaSource::Page { page_url } => {
                self.validate_remote_url(page_url)?;
                Ok(AcquisitionReport {
                    strategy: AcquisitionStrategy::BrowserMediated,
                    summary: "The extension must explicitly discover and hand off an accessible media stream; browser session secrets are not copied to the native engine."
                        .to_owned(),
                    requires_user_action: false,
                })
            }
            MediaSource::DirectUrl { media_url } => {
                self.validate_remote_url(media_url)?;
                if request.hints.requires_browser_session {
                    Ok(AcquisitionReport {
                        strategy: AcquisitionStrategy::BrowserMediated,
                        summary: "The media requires the existing browser session, so acquisition must remain browser-mediated without persisting credentials."
                            .to_owned(),
                        requires_user_action: false,
                    })
                } else {
                    Ok(AcquisitionReport {
                        strategy: AcquisitionStrategy::DirectMedia,
                        summary: "An accessible direct media URL can be processed locally."
                            .to_owned(),
                        requires_user_action: false,
                    })
                }
            }
            MediaSource::LocalFile { path } => {
                self.validate_local_path(path)?;
                Ok(AcquisitionReport {
                    strategy: AcquisitionStrategy::LocalFile,
                    summary: "A local media file can be decoded without uploading it.".to_owned(),
                    requires_user_action: false,
                })
            }
        }
    }

    /// Parse a remote location without retaining its potentially sensitive
    /// query string in an error/report. HTTPS is required by default; an
    /// explicit development policy may permit ordinary HTTP fixtures.
    pub fn validate_remote_url(&self, value: &str) -> Result<Url, MediaError> {
        let url = Url::parse(value).map_err(|_| MediaError::InvalidRemoteUrl)?;
        let secure_or_development_http =
            url.scheme() == "https" || (url.scheme() == "http" && self.policy.allow_insecure_http);
        if !secure_or_development_http || url.host_str().is_none() {
            return Err(MediaError::UnsupportedScheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(MediaError::EmbeddedCredentials);
        }
        if !self.policy.allow_private_network_urls && is_private_network_host(&url) {
            return Err(MediaError::PrivateNetworkUrl);
        }
        Ok(url)
    }

    /// Validate an IP address produced by DNS immediately before an outbound
    /// request. URL parsing can reject only literal private addresses; callers
    /// that resolve hostnames must use this method and pin the actual connection
    /// to the approved answer to avoid a validate-then-resolve race.
    pub fn validate_resolved_address(&self, address: IpAddr) -> Result<(), MediaError> {
        if !self.policy.allow_private_network_urls && is_private_network_address(address) {
            return Err(MediaError::PrivateNetworkUrl);
        }
        Ok(())
    }

    /// Local paths are allowed only when the user has provided an absolute
    /// path. The decoder must still open it with normal OS access checks.
    pub fn validate_local_path(&self, value: &str) -> Result<PathBuf, MediaError> {
        if !self.policy.allow_local_files {
            return Err(MediaError::LocalFilesDisabled);
        }
        if value.trim().is_empty() {
            return Err(MediaError::InvalidLocalPath);
        }
        // A native-messaging caller must not be able to turn an ostensibly
        // local-file job into SMB/device access. Reject UNC, extended-device,
        // and slash-form network paths at this boundary on every platform.
        if value.starts_with(r"\\") || value.starts_with("//") {
            return Err(MediaError::NetworkLocalPath);
        }
        let path = Path::new(value);
        if !path.is_absolute() {
            return Err(MediaError::InvalidLocalPath);
        }
        Ok(path.to_path_buf())
    }
}

fn is_private_network_host(url: &Url) -> bool {
    let Some(host) = url.host() else {
        return true;
    };
    match host {
        url::Host::Ipv4(address) => is_private_network_address(IpAddr::V4(address)),
        url::Host::Ipv6(address) => is_private_network_address(IpAddr::V6(address)),
        url::Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost") || domain.ends_with(".local")
        }
    }
}

fn is_private_network_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_private_ipv4(address),
        IpAddr::V6(address) => is_private_ipv6(address),
    }
}

fn is_private_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
        // Shared carrier-grade NAT, unspecified/reserved address space, and
        // non-public documentation/benchmark ranges must not become native
        // decoder destinations through a hostname answer.
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

fn is_private_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped_ipv4) = address.to_ipv4_mapped() {
        return is_private_ipv4(mapped_ipv4);
    }
    let octets = address.octets();
    let is_ipv4_compatible =
        octets[..12] == [0; 12] && !address.is_unspecified() && !address.is_loopback();
    if is_ipv4_compatible {
        return is_private_ipv4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    // The well-known NAT64 prefix embeds an IPv4 destination. Reject a
    // private embedded IPv4 value just as we do an IPv4-mapped literal.
    let is_well_known_nat64 = octets[..12]
        == [
            0x00, 0x64, 0xff, 0x9b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
    if is_well_known_nat64 {
        return is_private_ipv4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    let leading_segment = address.segments()[0];
    // `Ipv6Addr::is_unique_local` and `is_unicast_link_local` were added
    // after this workspace's Rust 1.78 MSRV. These RFC prefix checks preserve
    // the same policy without raising the compiler floor.
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

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("Subtitler cannot process this protected recording.")]
    ProtectedMedia,
    #[error("The media address is not a valid HTTP or HTTPS URL.")]
    InvalidRemoteUrl,
    #[error("This media address uses a browser-only or unsupported URL scheme.")]
    UnsupportedScheme,
    #[error("Media addresses must not contain embedded credentials.")]
    EmbeddedCredentials,
    #[error("Subtitler will not retrieve media from a private or local network address.")]
    PrivateNetworkUrl,
    #[error("Local file processing is disabled by policy.")]
    LocalFilesDisabled,
    #[error("A local media path must be absolute.")]
    InvalidLocalPath,
    #[error("Subtitler will not retrieve media from a network or device file path.")]
    NetworkLocalPath,
    #[error("The audio decoder is not available yet.")]
    DecoderUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use subtitler_core::{MediaAccessHints, MediaRequest, MediaSource};

    fn direct(url: &str) -> MediaRequest {
        MediaRequest {
            source: MediaSource::DirectUrl {
                media_url: url.to_owned(),
            },
            hints: MediaAccessHints::default(),
        }
    }

    #[test]
    fn direct_https_media_is_selected_without_logging_its_url() {
        let report = MediaSourceValidator::default()
            .plan(
                &direct("https://media.example.test/recording.mp4?signature=secret"),
                false,
            )
            .unwrap();

        assert_eq!(report.strategy, AcquisitionStrategy::DirectMedia);
        assert!(!report.summary.contains("signature"));
    }

    #[test]
    fn browser_schemes_and_embedded_credentials_are_rejected() {
        let validator = MediaSourceValidator::default();
        assert_eq!(
            validator
                .validate_remote_url("javascript:alert(1)")
                .unwrap_err(),
            MediaError::UnsupportedScheme
        );
        assert_eq!(
            validator
                .validate_remote_url("https://alice:secret@example.test/video")
                .unwrap_err(),
            MediaError::EmbeddedCredentials
        );
    }

    #[test]
    fn private_network_destinations_are_rejected_before_ffmpeg_can_reach_them() {
        let validator = MediaSourceValidator::default();
        for url in [
            "https://127.0.0.1/recording.mp4",
            "https://192.168.1.5/recording.mp4",
            "https://[::1]/recording.mp4",
            "https://[::ffff:127.0.0.1]/recording.mp4",
            "https://100.64.0.1/recording.mp4",
            "https://localhost/recording.mp4",
            "https://meeting.local/recording.mp4",
        ] {
            assert_eq!(
                validator.validate_remote_url(url).unwrap_err(),
                MediaError::PrivateNetworkUrl
            );
        }
    }

    #[test]
    fn resolved_private_destinations_are_rejected_at_the_dns_boundary() {
        let validator = MediaSourceValidator::default();
        for address in [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()),
            IpAddr::V6("fc00::1".parse().unwrap()),
        ] {
            assert_eq!(
                validator.validate_resolved_address(address).unwrap_err(),
                MediaError::PrivateNetworkUrl
            );
        }
        assert!(validator
            .validate_resolved_address(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)))
            .is_ok());
    }

    #[test]
    fn network_and_device_local_paths_are_rejected_before_ffmpeg() {
        let validator = MediaSourceValidator::default();
        for path in [
            r"\\server\share\recording.mp4",
            r"\\?\UNC\server\share\recording.mp4",
            "//server/share/recording.mp4",
        ] {
            assert_eq!(
                validator.validate_local_path(path).unwrap_err(),
                MediaError::NetworkLocalPath
            );
        }
    }

    #[test]
    fn ordinary_http_requires_an_explicit_development_policy() {
        let url = "http://media.example.test/recording.mp4";
        assert_eq!(
            MediaSourceValidator::default()
                .validate_remote_url(url)
                .unwrap_err(),
            MediaError::UnsupportedScheme
        );
        let validator = MediaSourceValidator::new(MediaPolicy {
            allow_insecure_http: true,
            ..MediaPolicy::default()
        });
        assert!(validator.validate_remote_url(url).is_ok());
    }

    #[test]
    fn protected_media_is_never_planned() {
        let mut request = direct("https://media.example.test/recording.mp4");
        request.hints.drm_detected = true;
        assert_eq!(
            MediaSourceValidator::default()
                .plan(&request, false)
                .unwrap_err(),
            MediaError::ProtectedMedia
        );
    }
}
