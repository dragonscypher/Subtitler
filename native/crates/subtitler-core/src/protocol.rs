use crate::{
    AcquisitionReport, JobId, JobKind, JobSpec, JobStatus, SubtitleCue, TimeRange,
    TranscriptSegment,
};
use serde::{Deserialize, Serialize};

pub const NATIVE_PROTOCOL_VERSION: u32 = 1;
/// Request identifiers are echoed in native-messaging responses. Keeping them
/// short prevents a caller-controlled correlation value from consuming the
/// native message budget.
pub const MAX_NATIVE_REQUEST_ID_BYTES: usize = 256;

/// Commands accepted by the native host. The JSON representation is flat for
/// Chrome Native Messaging: `{"request_id":"...","command":"status",...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum NativeCommand {
    Handshake {
        protocol_version: u32,
        #[serde(default)]
        extension_version: Option<String>,
    },
    Start {
        job: JobSpec,
    },
    Cancel {
        job_id: JobId,
    },
    Status {
        job_id: JobId,
    },
    /// Reattach a browser job to a completed private export after the
    /// ephemeral Native Messaging host or extension service worker restarted.
    /// The job identifier is an opaque UUID; no source URL or content crosses
    /// this request.
    Restore {
        job_id: JobId,
        kind: JobKind,
    },
    /// A lossy playback observation for an active generated-subtitle job.
    /// It contains only timeline metadata; media bytes, cookies, URLs, and
    /// caption text never cross this control-plane message.
    PlaybackUpdate {
        job_id: JobId,
        position_ms: u64,
        /// Playback speed multiplied by 1,000, avoiding floating-point wire
        /// comparisons in the native scheduler.
        playback_rate_milli: u16,
        is_paused: bool,
        seek_generation: u32,
    },
    /// Retrieves a bounded page of finalized generated subtitle cues. `cursor`
    /// is a zero-based cue offset from a stable completed job outcome; callers
    /// continue with `next_cursor` from the response until it is absent.
    GetSubtitleCues {
        job_id: JobId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u16>,
    },
    /// Retrieves a bounded page of finalized transcript segments. `cursor`
    /// is a zero-based segment offset from a stable completed job outcome;
    /// callers continue with `next_cursor` from the response until it is
    /// absent. The host deliberately does not make partial transcript text
    /// available while a job is running.
    GetTranscriptSegments {
        job_id: JobId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u16>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRequest {
    pub request_id: String,
    #[serde(flatten)]
    pub command: NativeCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCapabilities {
    pub protocol_version: u32,
    pub local_asr_available: bool,
    pub ffmpeg_available: bool,
    pub direct_media_acquisition: bool,
    pub browser_mediated_acquisition: bool,
    pub cloud_processing_requires_explicit_approval: bool,
    /// A deliberately coarse, non-sensitive local processing plan. It is
    /// present when the host can safely produce a local plan. The availability
    /// flags remain authoritative for whether a model is actually installed
    /// and runnable. The plan contains no paths, memory totals, operating-
    /// system details, device names, serial numbers, URLs, or credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_processing_advisory: Option<LocalProcessingAdvisory>,
}

impl Default for NativeCapabilities {
    fn default() -> Self {
        Self {
            protocol_version: NATIVE_PROTOCOL_VERSION,
            local_asr_available: false,
            ffmpeg_available: false,
            direct_media_acquisition: false,
            browser_mediated_acquisition: false,
            cloud_processing_requires_explicit_approval: true,
            local_processing_advisory: None,
        }
    }
}

/// A safe UI advisory derived from the native host's local model policy.
/// It is informational only: `cloud_helpful` never selects a provider,
/// initiates an upload, or changes a local-only job preference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalProcessingAdvisory {
    pub selection_source: LocalProcessingSelectionSource,
    pub model: LocalProcessingModel,
    pub quantization: LocalProcessingQuantization,
    pub backend: LocalProcessingBackend,
    pub local_performance: LocalProcessingPerformance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProcessingSelectionSource {
    Automatic,
    AdvancedEnvironment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProcessingModel {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3Turbo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProcessingQuantization {
    Q5_0,
    #[serde(rename = "q5_k_m")]
    Q5Km,
    Q8_0,
    F16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProcessingBackend {
    Cpu,
    Cuda,
    Metal,
    Vulkan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProcessingPerformance {
    Excellent,
    Good,
    MayBeSlow,
    CloudHelpful,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum NativeResponseBody {
    Handshake {
        native_host_name: String,
        protocol_version: u32,
        native_version: String,
        capabilities: NativeCapabilities,
    },
    JobStarted {
        job: JobStatus,
        acquisition: AcquisitionReport,
    },
    JobCancelled {
        job: JobStatus,
    },
    JobStatus {
        job: JobStatus,
    },
    /// A completed or stale result reconstructed from Subtitler's own private
    /// metadata/export directory after a reconnect.
    JobRestored {
        job: JobStatus,
    },
    /// Subtitle text intentionally crosses this boundary only as a small,
    /// explicit page for the overlay. It never includes source URLs, cookies,
    /// transcript exports, or native filesystem paths.
    SubtitleCues {
        job_id: JobId,
        cues: Vec<SubtitleCue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<u32>,
    },
    /// A bounded page of a completed transcript. This intentionally carries
    /// only display-ready segment timing, text, and an optional lightweight
    /// speaker label. Word timestamps, media information, export paths,
    /// source URLs, and translation metadata stay inside the native engine.
    TranscriptSegments {
        job_id: JobId,
        segments: Vec<TranscriptSegmentPageItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<u32>,
    },
    Error {
        code: ProtocolErrorCode,
        message: String,
        retryable: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeResponse {
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub body: NativeResponseBody,
}

impl NativeResponse {
    pub fn error(
        request_id: Option<String>,
        code: ProtocolErrorCode,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            request_id,
            body: NativeResponseBody::Error {
                code,
                message: message.into(),
                retryable,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    IncompatibleProtocol,
    InvalidRequest,
    UnknownJob,
    InvalidState,
    UnsupportedMedia,
    ProtectedMedia,
    EngineUnavailable,
    ResultTooLarge,
    Internal,
}

/// The deliberately minimal transcript segment shape permitted to cross the
/// native-messaging boundary. It is a DTO instead of exposing
/// [`TranscriptSegment`] directly because the canonical transcript also holds
/// word-level timestamps that the popup neither needs nor should receive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptSegmentPageItem {
    pub timing: TimeRange,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

impl From<&TranscriptSegment> for TranscriptSegmentPageItem {
    fn from(segment: &TranscriptSegment) -> Self {
        Self {
            timing: segment.timing,
            text: segment.text.clone(),
            speaker: segment.speaker.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_a_flat_native_messaging_shape() {
        let request = NativeRequest {
            request_id: "req-1".to_owned(),
            command: NativeCommand::Handshake {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                extension_version: Some("0.1.0".to_owned()),
            },
        };

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["command"], "handshake");
        assert_eq!(json["request_id"], "req-1");
        assert_eq!(json["protocol_version"], NATIVE_PROTOCOL_VERSION);
    }

    #[test]
    fn local_processing_advisory_is_additive_and_uses_safe_wire_values() {
        let mut capabilities = NativeCapabilities::default();
        let omitted = serde_json::to_value(&capabilities).unwrap();
        assert!(omitted.get("local_processing_advisory").is_none());

        capabilities.local_processing_advisory = Some(LocalProcessingAdvisory {
            selection_source: LocalProcessingSelectionSource::Automatic,
            model: LocalProcessingModel::Medium,
            quantization: LocalProcessingQuantization::Q5Km,
            backend: LocalProcessingBackend::Cpu,
            local_performance: LocalProcessingPerformance::Good,
        });
        let value = serde_json::to_value(capabilities).unwrap();
        assert_eq!(
            value["local_processing_advisory"],
            serde_json::json!({
                "selection_source": "automatic",
                "model": "medium",
                "quantization": "q5_k_m",
                "backend": "cpu",
                "local_performance": "good",
            })
        );
    }

    #[test]
    fn subtitle_cue_request_uses_a_flat_paged_shape() {
        let job_id = JobId::new();
        let request = NativeRequest {
            request_id: "cue-page".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id: job_id.clone(),
                cursor: Some(40),
                limit: Some(25),
            },
        };

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["command"], "get_subtitle_cues");
        assert_eq!(json["job_id"], serde_json::to_value(job_id).unwrap());
        assert_eq!(json["cursor"], 40);
        assert_eq!(json["limit"], 25);
    }

    #[test]
    fn transcript_segment_request_and_response_use_the_minimal_paged_shape() {
        let job_id = JobId::new();
        let request = NativeRequest {
            request_id: "transcript-page".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: job_id.clone(),
                cursor: Some(4),
                limit: Some(20),
            },
        };

        let request_json = serde_json::to_value(request).unwrap();
        assert_eq!(request_json["command"], "get_transcript_segments");
        assert_eq!(
            request_json["job_id"],
            serde_json::to_value(&job_id).unwrap()
        );
        assert_eq!(request_json["cursor"], 4);
        assert_eq!(request_json["limit"], 20);

        let response = NativeResponse {
            request_id: Some("transcript-page".to_owned()),
            body: NativeResponseBody::TranscriptSegments {
                job_id,
                segments: vec![TranscriptSegmentPageItem {
                    timing: TimeRange::new(1_000, 2_000).unwrap(),
                    text: "Visible transcript text.".to_owned(),
                    speaker: None,
                }],
                next_cursor: None,
            },
        };
        let response_json = serde_json::to_value(response).unwrap();
        let segment = &response_json["segments"][0];
        assert_eq!(response_json["response"], "transcript_segments");
        assert_eq!(segment["timing"]["start_ms"], 1_000);
        assert_eq!(segment["timing"]["end_ms"], 2_000);
        assert_eq!(segment["text"], "Visible transcript text.");
        assert!(segment.get("speaker").is_none());
        assert!(segment.get("words").is_none());
    }

    #[test]
    fn playback_update_uses_integer_timeline_metadata() {
        let job_id = JobId::new();
        let request = NativeRequest {
            request_id: "playback".to_owned(),
            command: NativeCommand::PlaybackUpdate {
                job_id: job_id.clone(),
                position_ms: 91_250,
                playback_rate_milli: 1_250,
                is_paused: false,
                seek_generation: 3,
            },
        };

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["command"], "playback_update");
        assert_eq!(json["job_id"], serde_json::to_value(job_id).unwrap());
        assert_eq!(json["position_ms"], 91_250);
        assert_eq!(json["playback_rate_milli"], 1_250);
        assert_eq!(json["is_paused"], false);
        assert_eq!(json["seek_generation"], 3);
    }
}
