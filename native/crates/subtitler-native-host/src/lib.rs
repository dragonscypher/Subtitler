//! Native Messaging host for the Subtitler browser extension.
//!
//! Chrome Native Messaging frames JSON with a four-byte little-endian length.
//! This host keeps stdout protocol-only, limits inbound message size, and owns
//! a small in-process job registry for the generic local-media path. Release
//! packaging will move long-lived job ownership behind the documented private
//! engine IPC; this bridge never reads browser cookies or exposes a network
//! listener.

mod persistence;
mod youtube;

use persistence::{JobPersistence, RestoreError};
use serde::Serialize;
use std::{
    collections::HashMap,
    env,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use subtitler_asr::{
    recommend_local_model, AsrError, CancellationToken, ComputeBackend, HardwareProfile,
    LanguageMode, LocalModel, LocalPerformance, Quantization, TranscriptionRequest,
    WhisperCppCliEngine, WhisperCppConfig, WhisperCppExecutionControl,
};
use subtitler_core::{
    transition_job, AcquisitionReport, AcquisitionStrategy, InitialPlayback, JobFailure,
    JobFailureCode, JobId, JobKind, JobPhase, JobProgress, JobSpec, JobState, JobStatus,
    LocalProcessingAdvisory, LocalProcessingBackend, LocalProcessingModel,
    LocalProcessingPerformance, LocalProcessingQuantization, LocalProcessingSelectionSource,
    MediaSource, NativeCapabilities, NativeCommand, NativeRequest, NativeResponse,
    NativeResponseBody, PlaybackUpdate, PlaybackUpdateResult, ProtocolErrorCode, ScheduledRange,
    SchedulerError, SubtitleBufferScheduler, SubtitleCue, SubtitlePacingState,
    SubtitleSchedulerConfig, TimeRange, Transcript, TranscriptSegment, TranscriptSegmentPageItem,
    WorkerStatus, MAX_NATIVE_REQUEST_ID_BYTES, NATIVE_PROTOCOL_VERSION,
};
use subtitler_media::{
    AudioExtractionRange, AudioInput, DownloadedRemoteMedia, ExtractionCancellation, FfmpegDecoder,
    FfmpegExtractionError, FfmpegExtractionOptions, MediaError, MediaSourceValidator,
    RemoteDownloadOptions, RemoteMediaAcquirer, RemoteMediaAcquisitionError,
};
use subtitler_subtitles::{
    segment_words, write_export_bundle, ExportBundle, SubtitleSegmentationConfig,
};
use sysinfo::System;
use thiserror::Error;
use youtube::{
    supports_youtube_page, DownloadedYoutubeMedia, YoutubePageResolver, YoutubeResolutionError,
    YoutubeResolverFailureReason,
};

pub const NATIVE_HOST_NAME: &str = "com.subtitler.native_host";
pub const MAX_NATIVE_MESSAGE_BYTES: u32 = 1_048_576;
/// A subtitle page is intentionally much smaller than the Chrome Native
/// Messaging frame cap. This leaves margin for the response envelope and
/// ensures the overlay can stay responsive on long recordings.
pub const MAX_SUBTITLE_CUE_PAGE_BYTES: usize = 128 * 1024;
pub const MAX_SUBTITLE_CUES_PER_PAGE: usize = 200;
/// Transcript pages are deliberately smaller than the subtitle endpoint's
/// already-conservative budget. A completed transcript can contain much more
/// text than an overlay cue, so this cap keeps the popup responsive and leaves
/// margin below Chrome Native Messaging's frame limit.
pub const MAX_TRANSCRIPT_SEGMENT_PAGE_BYTES: usize = 120 * 1024;
pub const MAX_TRANSCRIPT_SEGMENTS_PER_PAGE: usize = 100;
const MAX_TRANSCRIPT_SEGMENT_TEXT_BYTES: usize = 16 * 1024;
const MAX_TRANSCRIPT_SEGMENT_SPEAKER_BYTES: usize = 512;
/// Bounds genuine worker silence rather than a recording's total duration.
/// A running whisper child refreshes its own heartbeat, so long healthy work
/// is not marked stale merely because it exceeds a wall-clock expectation.
const MAX_SILENT_LOCAL_ACTIVITY_MS: u64 = 10 * 60 * 1_000;

#[derive(Default)]
struct HostState {
    jobs: HashMap<JobId, ManagedJob>,
}

struct ManagedJob {
    status: JobStatus,
    /// Browser-generated opaque correlation only. It is checkpointed so a
    /// reconnect can map a retained local result without retaining the media
    /// request that produced it.
    client_job_id: Option<String>,
    #[allow(dead_code)]
    acquisition: AcquisitionReport,
    cancellation: JobCancellation,
    activity_heartbeat: Arc<AtomicU64>,
    subtitle_runtime: Option<Arc<SubtitleRuntime>>,
    outcome: Option<JobOutcome>,
}

/// A completed result deliberately keeps filesystem export paths private while
/// retaining the canonical transcript for the explicit, bounded transcript
/// paging endpoint and generated cues for the overlay. It is stored only in
/// the in-process job registry in this phase.
struct JobOutcome {
    #[allow(dead_code)]
    exports: ExportBundle,
    /// Finalized once, sorted by media time, and never exposed directly: the
    /// wire response maps it to `TranscriptSegmentPageItem` so word timing,
    /// language metadata, and other private fields cannot cross this boundary.
    transcript: Transcript,
    cues: Vec<SubtitleCue>,
}

/// Shared, in-process state for a generated-subtitle job. The scheduler and
/// partial output are intentionally separate from `HostState` so a long
/// FFmpeg/ASR invocation never holds the job-registry mutex. This process is
/// still intentionally ephemeral; durable subtitle state belongs to the
/// future private engine service rather than Chrome Native Messaging.
struct SubtitleRuntime {
    scheduler: Mutex<SubtitleBufferScheduler>,
    /// The one current 30-second chunk can be cooperatively stopped after a
    /// newer seek generation preempts its scheduler lease.
    active_chunk: Mutex<Option<ActiveSubtitleChunk>>,
    /// Cues are append-only while a job runs. That makes page cursors stable
    /// for progressive browser polling even when a seek causes later media
    /// ranges to finish before earlier ones. Consumers sort by timestamps for
    /// rendering; final exports use a sorted copy.
    cues: Mutex<Vec<SubtitleCue>>,
    transcripts: Mutex<Vec<Transcript>>,
}

struct ActiveSubtitleChunk {
    reservation_id: u64,
    cancellation: JobCancellation,
}

#[derive(Clone)]
struct JobExecutionContext {
    subtitle_runtime: Option<Arc<SubtitleRuntime>>,
    activity_heartbeat: Arc<AtomicU64>,
}

impl JobExecutionContext {
    fn subtitle_runtime(&self) -> Option<&Arc<SubtitleRuntime>> {
        self.subtitle_runtime.as_ref()
    }

    fn touch_activity(&self) {
        self.activity_heartbeat
            .store(now_unix_ms(), Ordering::Release);
    }
}

#[derive(Clone, Debug)]
struct SubtitleRuntimeStatus {
    progress: JobProgress,
    message: String,
}

impl SubtitleRuntime {
    fn for_job(spec: &JobSpec) -> Result<Option<Arc<Self>>, SchedulerError> {
        if spec.kind != JobKind::SubtitleGeneration {
            return Ok(None);
        }
        let Some(duration_ms) = spec
            .media
            .hints
            .duration_ms
            .filter(|duration| *duration > 0)
        else {
            // A durationless source keeps the existing complete-media path.
            // A timestamp-buffer scheduler cannot safely choose bounded
            // source ranges without a reliable duration.
            return Ok(None);
        };

        // Keep one FFmpeg/ASR task bounded to the deliberate V1 30-second
        // scheduling window even if a future scheduler default changes.
        let mut config = SubtitleSchedulerConfig {
            processing_chunk_ms: subtitler_core::DEFAULT_PROCESSING_CHUNK_MS,
            ..SubtitleSchedulerConfig::default()
        };
        if let Some(requested) = spec.settings.preferred_subtitle_buffer_ms {
            config.targets.preferred_ahead_ms = requested.clamp(
                config.targets.minimum_ahead_ms,
                config.targets.maximum_ahead_ms,
            );
        }
        let mut scheduler = SubtitleBufferScheduler::new(
            subtitler_core::SchedulingMode::SubtitleBuffer,
            duration_ms,
            config,
        )?;
        if let Some(initial) = spec.settings.initial_playback.as_ref() {
            apply_initial_playback(&mut scheduler, initial)?;
        }

        Ok(Some(Arc::new(Self {
            scheduler: Mutex::new(scheduler),
            active_chunk: Mutex::new(None),
            cues: Mutex::new(Vec::new()),
            transcripts: Mutex::new(Vec::new()),
        })))
    }

    fn apply_playback(
        &self,
        update: PlaybackUpdate,
    ) -> Result<PlaybackUpdateResult, SchedulerError> {
        let result = self
            .scheduler
            .lock()
            .map_err(|_| SchedulerError::EmptyProcessingSample)?
            .update_playback(update)?;
        self.cancel_preempted(&result.preempted);
        Ok(result)
    }

    fn next_range(&self) -> Option<ScheduledRange> {
        self.scheduler
            .lock()
            .ok()
            .and_then(|mut scheduler| scheduler.next_processing_range())
    }

    fn begin_chunk(&self, scheduled: &ScheduledRange, cancellation: JobCancellation) {
        if let Ok(mut active) = self.active_chunk.lock() {
            *active = Some(ActiveSubtitleChunk {
                reservation_id: scheduled.reservation_id,
                cancellation,
            });
        }
    }

    fn finish_chunk(&self, reservation_id: u64) {
        if let Ok(mut active) = self.active_chunk.lock() {
            if active
                .as_ref()
                .is_some_and(|chunk| chunk.reservation_id == reservation_id)
            {
                *active = None;
            }
        }
    }

    fn cancel_preempted(&self, preempted: &[ScheduledRange]) {
        if preempted.is_empty() {
            return;
        }
        let Ok(active) = self.active_chunk.lock() else {
            return;
        };
        if let Some(chunk) = active.as_ref() {
            if preempted
                .iter()
                .any(|range| range.reservation_id == chunk.reservation_id)
            {
                chunk.cancellation.cancel();
            }
        }
    }

    fn cancel_active_chunk(&self) {
        if let Ok(active) = self.active_chunk.lock() {
            if let Some(chunk) = active.as_ref() {
                chunk.cancellation.cancel();
            }
        }
    }

    fn chunk_is_stale(&self, scheduled: &ScheduledRange) -> bool {
        self.scheduler
            .lock()
            .map(|scheduler| scheduler.playback().seek_generation > scheduled.seek_generation)
            .unwrap_or(true)
    }

    fn release_range(&self, reservation_id: u64) {
        if let Ok(mut scheduler) = self.scheduler.lock() {
            scheduler.release_processing_range(reservation_id);
        }
    }

    fn complete_range(
        &self,
        scheduled: &ScheduledRange,
        wall_elapsed_ms: u64,
    ) -> Result<(), SchedulerError> {
        let mut scheduler = self
            .scheduler
            .lock()
            .map_err(|_| SchedulerError::EmptyProcessingSample)?;
        match scheduler
            .complete_processing_range_with_sample(scheduled.reservation_id, wall_elapsed_ms)
        {
            Ok(_) => Ok(()),
            // A seek may have preempted the reservation just as the local
            // process completed. The decoded/transcribed range remains valid
            // and can safely count as completed coverage.
            Err(SchedulerError::UnknownReservation { .. }) => {
                scheduler.record_processed_range(scheduled.timing)?;
                scheduler.record_processing_sample(scheduled.timing.duration_ms(), wall_elapsed_ms)
            }
            Err(error) => Err(error),
        }
    }

    fn publish_chunk(&self, transcript: Transcript, cues: Vec<SubtitleCue>) {
        if let Ok(mut transcripts) = self.transcripts.lock() {
            transcripts.push(transcript);
        }
        if let Ok(mut all_cues) = self.cues.lock() {
            all_cues.extend(cues);
        }
    }

    fn cue_snapshot(&self) -> Vec<SubtitleCue> {
        self.cues
            .lock()
            .map(|cues| cues.clone())
            .unwrap_or_default()
    }

    fn completion_snapshot(&self) -> (Transcript, Vec<SubtitleCue>) {
        let mut transcripts = self
            .transcripts
            .lock()
            .map(|transcripts| transcripts.clone())
            .unwrap_or_default();
        transcripts.sort_by_key(|transcript| {
            transcript
                .segments
                .first()
                .map(|segment| segment.timing.start_ms)
                .unwrap_or(u64::MAX)
        });
        let mut segments = transcripts
            .into_iter()
            .flat_map(|transcript| transcript.segments)
            .collect::<Vec<_>>();
        segments.sort_by_key(|segment| segment.timing.start_ms);

        let transcript = Transcript {
            language: "en".to_owned(),
            translated_from: None,
            segments,
        };
        let cues = self.cue_snapshot();
        (transcript, cues)
    }

    fn is_complete(&self) -> bool {
        let Ok(scheduler) = self.scheduler.lock() else {
            return false;
        };
        scheduler.is_range_processed(TimeRange {
            start_ms: 0,
            end_ms: scheduler.media_duration_ms(),
        })
    }

    fn status_snapshot(&self) -> Option<SubtitleRuntimeStatus> {
        let scheduler = self.scheduler.lock().ok()?;
        let status = scheduler.status();
        let processed_ms = scheduler
            .processed_coverage()
            .iter()
            .fold(0_u64, |total, range| {
                total.saturating_add(range.duration_ms())
            });
        let buffer_ahead_ms = status.subtitle_buffer_ahead_ms.unwrap_or_default();
        let target_ahead_ms = status.target_buffer_ahead_ms.unwrap_or_default();
        Some(SubtitleRuntimeStatus {
            progress: JobProgress {
                media_duration_ms: Some(status.media_duration_ms),
                processed_ms,
                subtitle_buffer_ahead_ms: status.subtitle_buffer_ahead_ms,
                phase: Some(JobPhase::Transcribing),
                audio_seconds_decoded_ms: processed_ms,
                audio_seconds_transcribed_ms: processed_ms,
                completed_intervals: scheduler.processed_coverage().len().min(u32::MAX as usize)
                    as u32,
                ..JobProgress::default()
            },
            message: subtitle_runtime_message(status.pacing, buffer_ahead_ms, target_ahead_ms),
        })
    }
}

fn apply_initial_playback(
    scheduler: &mut SubtitleBufferScheduler,
    initial: &InitialPlayback,
) -> Result<(), SchedulerError> {
    scheduler.update_playback(PlaybackUpdate {
        position_ms: initial.position_ms,
        playback_rate: f64::from(initial.playback_rate_milli) / 1_000.0,
        is_playing: !initial.is_paused,
        seek_generation: 0,
    })?;
    Ok(())
}

fn subtitle_runtime_message(
    pacing: SubtitlePacingState,
    buffer_ahead_ms: u64,
    target_ahead_ms: u64,
) -> String {
    let buffer_seconds = buffer_ahead_ms / 1_000;
    let target_seconds = target_ahead_ms / 1_000;
    match pacing {
        SubtitlePacingState::PlaybackPaused => format!(
            "Generating subtitles locally. {buffer_seconds}s buffered ahead while playback is paused (target {target_seconds}s)."
        ),
        SubtitlePacingState::Measuring | SubtitlePacingState::KeepingUp => format!(
            "Generating subtitles locally. {buffer_seconds}s buffered ahead (target {target_seconds}s)."
        ),
        SubtitlePacingState::AtRisk => format!(
            "Subtitler is keeping a {buffer_seconds}s subtitle buffer, but local processing is close to playback speed."
        ),
        SubtitlePacingState::CannotKeepUp => format!(
            "Subtitler is processing more slowly than playback. {buffer_seconds}s of subtitles remain buffered ahead."
        ),
        SubtitlePacingState::PauseRecommended => {
            "Subtitler needs a brief playback pause to rebuild the subtitle buffer.".to_owned()
        }
        SubtitlePacingState::FullTranscriptIndependent => {
            "Creating a local transcript independently of playback.".to_owned()
        }
    }
}

#[derive(Clone, Debug, Default)]
struct JobCancellation {
    extraction: ExtractionCancellation,
    asr: CancellationToken,
}

impl JobCancellation {
    fn cancel(&self) {
        self.extraction.cancel();
        self.asr.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.extraction.is_cancelled() || self.asr.is_cancelled()
    }
}

/// The native execution boundary. It is injectable for deterministic host
/// tests, while the local runner composes only safe media/ASR/export crates.
trait JobRunner: Send + Sync {
    fn run(
        &self,
        job_id: &JobId,
        spec: &JobSpec,
        context: &JobExecutionContext,
        cancellation: &JobCancellation,
        report_progress: &mut dyn FnMut(JobProgress),
    ) -> Result<JobOutcome, JobFailure>;
}

#[derive(Clone, Copy, Debug, Default)]
struct UnavailableJobRunner;

impl JobRunner for UnavailableJobRunner {
    fn run(
        &self,
        _job_id: &JobId,
        _spec: &JobSpec,
        _context: &JobExecutionContext,
        _cancellation: &JobCancellation,
        _report_progress: &mut dyn FnMut(JobProgress),
    ) -> Result<JobOutcome, JobFailure> {
        Err(failure(
            JobFailureCode::ModelUnavailable,
            "Subtitler needs its installed local speech model before it can process this recording.",
            true,
        ))
    }
}

/// Actual Phase 3 local media -> normalized WAV -> whisper.cpp -> export path.
struct LocalTranscriptionRunner {
    decoder: FfmpegDecoder,
    remote_acquirer: RemoteMediaAcquirer,
    youtube_resolver: Option<YoutubePageResolver>,
    whisper: WhisperCppCliEngine,
    whisper_config: WhisperCppConfig,
    export_root: PathBuf,
}

/// Keeps a downloaded remote source alive for the whole job while presenting
/// the same local-only decoder interface used by an explicit local file.
enum AcquiredAudioInput {
    Local(AudioInput),
    Downloaded(DownloadedRemoteMedia),
    Youtube(DownloadedYoutubeMedia),
}

impl AcquiredAudioInput {
    fn input(&self) -> &AudioInput {
        match self {
            Self::Local(input) => input,
            Self::Downloaded(media) => media.input(),
            Self::Youtube(media) => media.input(),
        }
    }
}

const BYTES_PER_MIB: u64 = 1_024 * 1_024;
/// The installer-owned developer layout currently contains the conventional
/// `ggml-base.en.bin` asset. Its verified working set is substantially lower
/// than the generic planner's 4 GiB quality recommendation, but still needs a
/// one-GiB safety floor before the browser, decoder, and ASR can coexist.
const DEVELOPER_BASE_MIN_AVAILABLE_MEMORY_MB: u64 = 1_024;

/// The source of a local model plan. The normal path is automatic; the
/// environment variant is intentionally an advanced development/installer
/// escape hatch and must provide all three selection values together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalModelSelectionSource {
    Automatic,
    AdvancedEnvironment,
}

/// A selected local ASR configuration plus its intentionally coarse UI
/// advisory. This type never stores a model path, raw hardware readings, or
/// any page/job information.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalModelSelection {
    model: LocalModel,
    quantization: Quantization,
    backend: ComputeBackend,
    source: LocalModelSelectionSource,
    local_performance: LocalPerformance,
}

impl LocalModelSelection {
    fn advisory(&self) -> LocalProcessingAdvisory {
        LocalProcessingAdvisory {
            selection_source: match self.source {
                LocalModelSelectionSource::Automatic => LocalProcessingSelectionSource::Automatic,
                LocalModelSelectionSource::AdvancedEnvironment => {
                    LocalProcessingSelectionSource::AdvancedEnvironment
                }
            },
            model: match self.model {
                LocalModel::Tiny => LocalProcessingModel::Tiny,
                LocalModel::Base => LocalProcessingModel::Base,
                LocalModel::Small => LocalProcessingModel::Small,
                LocalModel::Medium => LocalProcessingModel::Medium,
                LocalModel::LargeV3Turbo => LocalProcessingModel::LargeV3Turbo,
            },
            quantization: match self.quantization {
                Quantization::Q5_0 => LocalProcessingQuantization::Q5_0,
                Quantization::Q5Km => LocalProcessingQuantization::Q5Km,
                Quantization::Q8_0 => LocalProcessingQuantization::Q8_0,
                Quantization::F16 => LocalProcessingQuantization::F16,
            },
            backend: match self.backend {
                ComputeBackend::Cpu => LocalProcessingBackend::Cpu,
                ComputeBackend::Cuda => LocalProcessingBackend::Cuda,
                ComputeBackend::Metal => LocalProcessingBackend::Metal,
                ComputeBackend::Vulkan => LocalProcessingBackend::Vulkan,
            },
            local_performance: match self.local_performance {
                LocalPerformance::Excellent => LocalProcessingPerformance::Excellent,
                LocalPerformance::Good => LocalProcessingPerformance::Good,
                LocalPerformance::MayBeSlow => LocalProcessingPerformance::MayBeSlow,
                LocalPerformance::CloudHelpful => LocalProcessingPerformance::CloudHelpful,
            },
        }
    }
}

/// The concrete runner and the safe metadata returned by the handshake. The
/// advisory is separated from `WhisperCppConfig` so private model paths cannot
/// accidentally be serialized into extension-facing capabilities.
struct ConfiguredLocalRunner {
    runner: LocalTranscriptionRunner,
    advisory: LocalProcessingAdvisory,
    persistence: JobPersistence,
}

/// Performs only conservative local hardware observation. `sysinfo` supplies
/// available system memory; failure or a zero reading remains zero so the ASR
/// planner chooses its smallest safe local fallback. No GPU/device enumeration
/// is attempted here: accelerator support is exposed only when an installer or
/// development environment explicitly declares the compiled backend.
fn collect_hardware_profile() -> Result<HardwareProfile, ()> {
    let mut system = System::new();
    system.refresh_memory();

    let logical_cpu_count = thread::available_parallelism()
        .ok()
        .map(|count| count.get());
    let supported_backends = declared_compiled_backends_from_environment()?;

    Ok(hardware_profile_from_observation(
        logical_cpu_count,
        Some(system.available_memory()),
        supported_backends,
    ))
}

/// Converts raw host readings into the minimal profile consumed by the ASR
/// planner. It is kept pure so host-policy tests do not depend on the current
/// machine's CPU count or memory pressure.
fn hardware_profile_from_observation(
    logical_cpu_count: Option<usize>,
    available_memory_bytes: Option<u64>,
    supported_backends: Vec<ComputeBackend>,
) -> HardwareProfile {
    let logical_cpu_count = logical_cpu_count.unwrap_or_default().min(u16::MAX as usize) as u16;
    let available_memory_mb = available_memory_bytes.unwrap_or_default() / BYTES_PER_MIB;

    let mut unique_backends = Vec::new();
    for backend in supported_backends {
        if backend != ComputeBackend::Cpu && !unique_backends.contains(&backend) {
            unique_backends.push(backend);
        }
    }

    HardwareProfile {
        logical_cpu_count,
        available_memory_mb,
        supported_backends: unique_backends,
    }
}

/// Reads compiler/installer-owned backend declarations. This is deliberately
/// not GPU discovery: an omitted declaration means CPU only, while a declared
/// accelerator says only that the packaged local engine supports that backend.
/// The installer is responsible for setting this after checking its selected
/// distribution; browser input can never influence it.
fn declared_compiled_backends_from_environment() -> Result<Vec<ComputeBackend>, ()> {
    let declared = optional_environment_value("SUBTITLER_COMPILED_BACKENDS")?;
    parse_declared_backends(env::consts::OS, declared.as_deref())
}

fn parse_declared_backends(
    operating_system: &str,
    declared: Option<&str>,
) -> Result<Vec<ComputeBackend>, ()> {
    let Some(declared) = declared else {
        return Ok(Vec::new());
    };
    if declared.is_empty() {
        return Err(());
    }

    let mut backends = Vec::new();
    for value in declared.split(',') {
        let backend = match value {
            "cpu" => ComputeBackend::Cpu,
            "cuda" => ComputeBackend::Cuda,
            "metal" => ComputeBackend::Metal,
            "vulkan" => ComputeBackend::Vulkan,
            _ => return Err(()),
        };
        if backend == ComputeBackend::Metal && operating_system != "macos" {
            return Err(());
        }
        if backend == ComputeBackend::Cuda && !matches!(operating_system, "windows" | "linux") {
            return Err(());
        }
        if !backends.contains(&backend) {
            backends.push(backend);
        } else {
            return Err(());
        }
    }
    Ok(backends)
}

/// Environment overrides are advanced configuration. Partial overrides are
/// rejected so the host never combines a user-selected model with an automatic
/// quantization/backend and accidentally claims a configuration that was not
/// installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdvancedModelOverride {
    model: LocalModel,
    quantization: Quantization,
    backend: ComputeBackend,
}

impl AdvancedModelOverride {
    fn from_environment() -> Result<Option<Self>, ()> {
        let model = optional_environment_value("SUBTITLER_LOCAL_MODEL")?;
        let quantization = optional_environment_value("SUBTITLER_MODEL_QUANTIZATION")?;
        let backend = optional_environment_value("SUBTITLER_COMPUTE_BACKEND")?;
        match (model, quantization, backend) {
            (None, None, None) => Ok(None),
            (Some(model), Some(quantization), Some(backend)) => Ok(Some(Self {
                model: parse_local_model(&model)?,
                quantization: parse_quantization(&quantization)?,
                backend: parse_compute_backend(&backend)?,
            })),
            _ => Err(()),
        }
    }
}

fn optional_environment_value(name: &str) -> Result<Option<String>, ()> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(env::VarError::NotUnicode(_)) => Err(()),
        Err(env::VarError::NotPresent) => Ok(None),
    }
}

/// Uses an explicit installer/developer override when supplied. For the
/// checked developer build, falls back only to an exact private tool layout
/// under `%LOCALAPPDATA%\\Subtitler\\developer`; this never searches PATH or
/// accepts a browser-controlled location.
fn configured_tool_path(name: &str, developer_default: PathBuf) -> Result<PathBuf, ()> {
    if let Some(value) = optional_environment_value(name)? {
        let path = PathBuf::from(value);
        return path.is_file().then_some(path).ok_or(());
    }
    developer_default
        .is_file()
        .then_some(developer_default)
        .ok_or(())
}

fn configured_optional_tool_path(
    name: &str,
    developer_default: PathBuf,
) -> Result<Option<PathBuf>, ()> {
    if let Some(value) = optional_environment_value(name)? {
        let path = PathBuf::from(value);
        return path.is_file().then_some(path).map(Some).ok_or(());
    }
    Ok(developer_default.is_file().then_some(developer_default))
}

/// The WebPoClient provider uses a separate browser instance and must never
/// derive a path from an extension message or an existing browser profile.
fn developer_wpc_browser_path() -> PathBuf {
    env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("Google")
        .join("Chrome")
        .join("Application")
        .join("chrome.exe")
}

/// Optional directory counterpart to the private tool lookup above. An
/// explicit override is strict; the implicit developer layout simply leaves
/// the optional component disabled until its installer-owned directory exists.
fn configured_optional_tool_directory(
    name: &str,
    developer_default: PathBuf,
) -> Result<Option<PathBuf>, ()> {
    if let Some(value) = optional_environment_value(name)? {
        let path = PathBuf::from(value);
        return path.is_dir().then_some(path).map(Some).ok_or(());
    }
    Ok(developer_default.is_dir().then_some(developer_default))
}

fn parse_local_model(value: &str) -> Result<LocalModel, ()> {
    match value {
        "tiny" => Ok(LocalModel::Tiny),
        "base" => Ok(LocalModel::Base),
        "small" => Ok(LocalModel::Small),
        "medium" => Ok(LocalModel::Medium),
        "large_v3_turbo" => Ok(LocalModel::LargeV3Turbo),
        _ => Err(()),
    }
}

fn parse_quantization(value: &str) -> Result<Quantization, ()> {
    match value {
        "q5_0" => Ok(Quantization::Q5_0),
        "q5_k_m" => Ok(Quantization::Q5Km),
        "q8_0" => Ok(Quantization::Q8_0),
        "f16" => Ok(Quantization::F16),
        _ => Err(()),
    }
}

fn parse_compute_backend(value: &str) -> Result<ComputeBackend, ()> {
    match value {
        "cpu" => Ok(ComputeBackend::Cpu),
        "cuda" => Ok(ComputeBackend::Cuda),
        "metal" => Ok(ComputeBackend::Metal),
        "vulkan" => Ok(ComputeBackend::Vulkan),
        _ => Err(()),
    }
}

/// Use the deterministic ASR policy unless the user/installer deliberately
/// supplied all advanced selection fields. An advanced plan still has to name
/// a declared compiled backend and cannot exceed a complete detected profile's
/// conservative minimums; partial hardware readings remain an explicit-user
/// choice rather than a false automatic feasibility claim.
///
/// This host can safely use the automatic branch for a planning advisory, but
/// not to launch an arbitrary `SUBTITLER_WHISPER_MODEL_PATH`: the development
/// path has no signed model manifest that proves the filename's model and
/// quantization. `LocalTranscriptionRunner::from_environment` therefore
/// requires a complete explicit triple before it launches a local model. A
/// release model manager can remove that limitation only after it owns a
/// verified asset-to-metadata mapping.
fn select_local_model(
    profile: &HardwareProfile,
    advanced_override: Option<AdvancedModelOverride>,
) -> Result<LocalModelSelection, ()> {
    let recommendation = recommend_local_model(profile);
    let Some(advanced_override) = advanced_override else {
        return Ok(LocalModelSelection {
            model: recommendation.model,
            quantization: recommendation.quantization,
            backend: recommendation.backend,
            source: LocalModelSelectionSource::Automatic,
            local_performance: recommendation.local_performance,
        });
    };

    if !profile.supports_backend(advanced_override.backend)
        || (advanced_override.model.requires_accelerated_backend()
            && !advanced_override.backend.is_accelerated())
        || (profile.logical_cpu_count > 0
            && profile.available_memory_mb > 0
            && !advanced_override
                .model
                .is_feasible_on(profile, advanced_override.backend))
    {
        return Err(());
    }

    Ok(LocalModelSelection {
        model: advanced_override.model,
        quantization: advanced_override.quantization,
        backend: advanced_override.backend,
        source: LocalModelSelectionSource::AdvancedEnvironment,
        // This is deliberately the host's broad local-performance advisory,
        // not a promise that a custom advanced model will match a benchmark.
        local_performance: recommendation.local_performance,
    })
}

/// The developer engine's conventional base-English model is an installer
/// owned, known asset rather than an arbitrary browser-supplied model path.
/// It may start on a system below the generic *quality recommendation* floor
/// when one GiB remains available; the advisory still reports that cloud could
/// help and no cloud route is selected. Explicit environment selections retain
/// the stricter model-planner feasibility rules.
fn select_developer_layout_model(
    profile: &HardwareProfile,
    advanced_override: Option<AdvancedModelOverride>,
) -> Result<LocalModelSelection, ()> {
    if let Some(advanced_override) = advanced_override {
        return select_local_model(profile, Some(advanced_override));
    }
    if profile.logical_cpu_count < 2
        || profile.available_memory_mb < DEVELOPER_BASE_MIN_AVAILABLE_MEMORY_MB
    {
        return Err(());
    }
    let recommendation = recommend_local_model(profile);
    Ok(LocalModelSelection {
        model: LocalModel::Base,
        // `ggml-base.en.bin` is whisper.cpp's conventional full-precision
        // filename. This exact developer asset is known; release automatic
        // selection remains gated on a signed asset manifest.
        quantization: Quantization::F16,
        backend: ComputeBackend::Cpu,
        source: LocalModelSelectionSource::Automatic,
        local_performance: recommendation.local_performance,
    })
}

impl LocalTranscriptionRunner {
    /// Development configuration is intentionally explicit. A release
    /// installer will write a protected manifest/config rather than relying on
    /// ambient environment variables. The extension never supplies these.
    fn from_environment() -> Result<ConfiguredLocalRunner, ()> {
        let developer_tools = private_engine_root().join("developer").join("tools");
        let ffmpeg_path = configured_tool_path(
            "SUBTITLER_FFMPEG_PATH",
            developer_tools.join("ffmpeg").join("ffmpeg.exe"),
        )?;
        let executable = configured_tool_path(
            "SUBTITLER_WHISPER_CPP_PATH",
            developer_tools.join("whisper").join("whisper-cli.exe"),
        )?;
        let model_path = configured_tool_path(
            "SUBTITLER_WHISPER_MODEL_PATH",
            developer_tools.join("models").join("ggml-base.en.bin"),
        )?;
        let profile = collect_hardware_profile()?;
        let selection =
            select_developer_layout_model(&profile, AdvancedModelOverride::from_environment()?)?;
        let whisper_config = WhisperCppConfig {
            executable,
            model_path,
            model: selection.model,
            quantization: selection.quantization,
            backend: selection.backend,
            thread_count: WhisperCppConfig::recommended_thread_count(),
        };
        whisper_config.validate().map_err(|_| ())?;

        let engine_root = private_engine_root();
        let cache_root = env::var_os("SUBTITLER_CACHE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| engine_root.join("cache"));
        let options = FfmpegExtractionOptions {
            temporary_root: Some(cache_root.clone()),
            ..FfmpegExtractionOptions::default()
        };
        let export_root = env::var_os("SUBTITLER_EXPORT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| engine_root.join("exports"));
        let checkpoint_root = env::var_os("SUBTITLER_JOB_STATE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| engine_root.join("jobs"));
        Ok(ConfiguredLocalRunner {
            advisory: selection.advisory(),
            persistence: JobPersistence::new(checkpoint_root, export_root.clone()),
            runner: Self {
                decoder: FfmpegDecoder::with_options(ffmpeg_path, options),
                remote_acquirer: RemoteMediaAcquirer::new(
                    MediaSourceValidator::default(),
                    RemoteDownloadOptions {
                        temporary_root: Some(cache_root),
                        ..RemoteDownloadOptions::default()
                    },
                ),
                youtube_resolver: configured_optional_tool_path(
                    "SUBTITLER_YTDLP_PATH",
                    developer_tools.join("yt-dlp").join("yt-dlp.exe"),
                )?
                .map(|yt_dlp| {
                    let deno = configured_optional_tool_path(
                        "SUBTITLER_DENO_PATH",
                        developer_tools.join("deno").join("deno.exe"),
                    )?;
                    let resolver = YoutubePageResolver::new(yt_dlp, deno).map_err(|_| ())?;
                    let wpc_plugin_directory = configured_optional_tool_directory(
                        "SUBTITLER_YTDLP_WPC_PLUGIN_DIR",
                        developer_tools.join("yt-dlp").join("plugins").join("wpc"),
                    )?;
                    if let Some(wpc_plugin_directory) = wpc_plugin_directory {
                        let browser_path = configured_optional_tool_path(
                            "SUBTITLER_YTDLP_WPC_BROWSER_PATH",
                            developer_wpc_browser_path(),
                        )?
                        .ok_or(())?;
                        resolver
                            .with_webpo_client_provider(wpc_plugin_directory, browser_path)
                            .map_err(|_| ())
                    } else {
                        let plugin_directory = configured_optional_tool_directory(
                            "SUBTITLER_YTDLP_POT_PLUGIN_DIR",
                            developer_tools.join("yt-dlp").join("plugins"),
                        )?;
                        let server_home = configured_optional_tool_directory(
                            "SUBTITLER_YTDLP_POT_SERVER_HOME",
                            developer_tools.join("youtube-pot-provider").join("server"),
                        )?;
                        match (plugin_directory, server_home) {
                            (None, None) => Ok(resolver),
                            (Some(plugin_directory), Some(server_home)) => resolver
                                .with_po_token_provider(plugin_directory, server_home)
                                .map_err(|_| ()),
                            // Installing only half a provider is an installer
                            // error, not a reason to import cookies or fall back
                            // to platform captions.
                            _ => Err(()),
                        }
                    }
                })
                .transpose()?,
                whisper: WhisperCppCliEngine::default(),
                whisper_config,
                export_root,
            },
        })
    }
}

impl JobRunner for LocalTranscriptionRunner {
    fn run(
        &self,
        job_id: &JobId,
        spec: &JobSpec,
        context: &JobExecutionContext,
        cancellation: &JobCancellation,
        report_progress: &mut dyn FnMut(JobProgress),
    ) -> Result<JobOutcome, JobFailure> {
        if let Some(runtime) = context.subtitle_runtime() {
            return self.run_subtitle_buffer(
                job_id,
                spec,
                context,
                runtime,
                cancellation,
                report_progress,
            );
        }
        self.run_complete_media(job_id, spec, context, cancellation, report_progress)
    }
}

impl LocalTranscriptionRunner {
    fn transcription_request(&self, spec: &JobSpec) -> TranscriptionRequest {
        TranscriptionRequest {
            // V1 fixes its output language to English. A conventional
            // whisper.cpp `*.en.*` asset is English-only, so it must run the
            // English transcription task. Passing `-tr` to that model can
            // degrade its recognition and token timestamps. Multilingual
            // assets keep the translation task for V1 foreign-language input.
            language_mode: if self.whisper_config.is_english_only_model() {
                LanguageMode::English
            } else {
                LanguageMode::TranslateInputToEnglish
            },
            word_timestamps: true,
            speaker_diarization: spec.settings.speaker_diarization,
            model: self.whisper_config.model,
            quantization: self.whisper_config.quantization,
            backend: self.whisper_config.backend,
        }
    }

    /// The full-recording path remains deliberately independent of browser
    /// playback. It is used for transcript jobs and subtitle jobs that do not
    /// have a trustworthy positive duration for safe range scheduling.
    fn run_complete_media(
        &self,
        job_id: &JobId,
        spec: &JobSpec,
        context: &JobExecutionContext,
        cancellation: &JobCancellation,
        report_progress: &mut dyn FnMut(JobProgress),
    ) -> Result<JobOutcome, JobFailure> {
        context.touch_activity();
        if cancellation.is_cancelled() {
            return Err(cancelled_failure());
        }
        report_progress(JobProgress {
            media_duration_ms: spec.media.hints.duration_ms,
            processed_ms: 0,
            subtitle_buffer_ahead_ms: None,
            phase: Some(JobPhase::Acquiring),
            ..JobProgress::default()
        });
        let acquired_input = self.acquire_audio_input_from_spec(spec, cancellation)?;
        context.touch_activity();
        let input = acquired_input.input();

        let audio = self
            .decoder
            .extract_to_wav(input, &cancellation.extraction)
            .map_err(media_failure)?;
        context.touch_activity();
        if cancellation.is_cancelled() {
            return Err(cancelled_failure());
        }
        report_progress(progress_for_fraction(
            spec.media.hints.duration_ms,
            10,
            JobPhase::Transcribing,
        ));

        let request = self.transcription_request(spec);
        let control = WhisperCppExecutionControl::with_activity_heartbeat(
            cancellation.asr.clone(),
            self.whisper.options().clone(),
            Arc::clone(&context.activity_heartbeat),
        );
        let transcript = self
            .whisper
            .transcribe_file_with_control(&self.whisper_config, &request, audio.path(), &control)
            .map_err(asr_failure)?;
        context.touch_activity();
        if cancellation.is_cancelled() {
            return Err(cancelled_failure());
        }
        report_progress(progress_for_fraction(
            spec.media.hints.duration_ms,
            92,
            JobPhase::Segmenting,
        ));

        let transcript = canonicalize_transcript(transcript);
        let words = transcript
            .segments
            .iter()
            .flat_map(|segment| segment.words.iter().cloned())
            .collect::<Vec<_>>();
        let cues = segment_words(&words, &SubtitleSegmentationConfig::default()).map_err(|_| {
            failure(
                JobFailureCode::EngineFailure,
                "Subtitler received invalid timestamps from the local speech engine.",
                false,
            )
        })?;
        context.touch_activity();
        report_progress(progress_for_fraction(
            spec.media.hints.duration_ms,
            96,
            JobPhase::Finalizing,
        ));
        let exports = write_export_bundle(&self.export_root, job_id, &transcript, &cues).map_err(
            |error| {
                failure(
                    JobFailureCode::InsufficientStorage,
                    format!("Subtitler could not save the completed transcript exports: {error}"),
                    true,
                )
            },
        )?;
        context.touch_activity();
        report_progress(progress_for_fraction(
            spec.media.hints.duration_ms,
            100,
            JobPhase::Finalizing,
        ));
        Ok(JobOutcome {
            exports,
            transcript,
            cues,
        })
    }

    /// Process one bounded source-audio range at a time, prioritizing the
    /// current playback window supplied through `PlaybackUpdate`. This is not
    /// a live-ASR path: every range is extracted directly from prerecorded
    /// media and transcribed as fast as the local engine can finish it.
    fn run_subtitle_buffer(
        &self,
        job_id: &JobId,
        spec: &JobSpec,
        context: &JobExecutionContext,
        runtime: &Arc<SubtitleRuntime>,
        cancellation: &JobCancellation,
        report_progress: &mut dyn FnMut(JobProgress),
    ) -> Result<JobOutcome, JobFailure> {
        context.touch_activity();
        if cancellation.is_cancelled() {
            return Err(cancelled_failure());
        }
        let request = self.transcription_request(spec);
        report_subtitle_runtime_status(runtime, report_progress);
        let acquired_input = self.acquire_audio_input_from_spec(spec, cancellation)?;
        context.touch_activity();
        let input = acquired_input.input();

        loop {
            if cancellation.is_cancelled() {
                return Err(cancelled_failure());
            }

            if runtime.is_complete() {
                let (transcript, cues) = runtime.completion_snapshot();
                let transcript = canonicalize_transcript(transcript);
                transcript.validate().map_err(|_| {
                    failure(
                        JobFailureCode::EngineFailure,
                        "Subtitler received invalid timestamps while preparing subtitle exports.",
                        false,
                    )
                })?;
                let mut export_cues = cues.clone();
                export_cues.sort_by_key(|cue| (cue.timing.start_ms, cue.timing.end_ms));
                let exports =
                    write_export_bundle(&self.export_root, job_id, &transcript, &export_cues)
                        .map_err(|error| {
                            failure(
                                JobFailureCode::InsufficientStorage,
                                format!(
                                "Subtitler could not save the completed transcript exports: {error}"
                            ),
                                true,
                            )
                        })?;
                context.touch_activity();
                report_subtitle_runtime_status(runtime, report_progress);
                return Ok(JobOutcome {
                    exports,
                    transcript,
                    cues,
                });
            }

            let Some(scheduled) = runtime.next_range() else {
                // The adaptive buffer target is currently covered. Wait for
                // a new playback observation rather than decoding irrelevant
                // media behind or far beyond the user’s current playhead.
                report_subtitle_runtime_status(runtime, report_progress);
                thread::sleep(Duration::from_millis(50));
                continue;
            };

            let chunk_cancellation = JobCancellation::default();
            runtime.begin_chunk(&scheduled, chunk_cancellation.clone());
            if runtime.chunk_is_stale(&scheduled) {
                // A seek arrived after the lease was reserved but before its
                // FFmpeg process started. Do not spend work on that obsolete
                // range; the next loop selects around the new playhead.
                chunk_cancellation.cancel();
            }
            let range =
                AudioExtractionRange::new(scheduled.timing.start_ms, scheduled.timing.end_ms)
                    .map_err(|_| {
                        failure(
                            JobFailureCode::EngineFailure,
                            "Subtitler could not schedule a safe subtitle-audio range.",
                            false,
                        )
                    })?;
            let started = Instant::now();
            let audio = match self.decoder.extract_range_to_wav(
                input,
                range,
                &chunk_cancellation.extraction,
            ) {
                Ok(audio) => audio,
                Err(FfmpegExtractionError::Cancelled) => {
                    runtime.finish_chunk(scheduled.reservation_id);
                    runtime.release_range(scheduled.reservation_id);
                    if cancellation.is_cancelled() {
                        return Err(cancelled_failure());
                    }
                    continue;
                }
                Err(error) => {
                    runtime.finish_chunk(scheduled.reservation_id);
                    runtime.release_range(scheduled.reservation_id);
                    return Err(media_failure(error));
                }
            };
            context.touch_activity();
            if chunk_cancellation.is_cancelled() {
                runtime.finish_chunk(scheduled.reservation_id);
                runtime.release_range(scheduled.reservation_id);
                if cancellation.is_cancelled() {
                    return Err(cancelled_failure());
                }
                continue;
            }

            let control = WhisperCppExecutionControl::with_activity_heartbeat(
                chunk_cancellation.asr.clone(),
                self.whisper.options().clone(),
                Arc::clone(&context.activity_heartbeat),
            );
            let chunk_transcript = match self.whisper.transcribe_file_with_control(
                &self.whisper_config,
                &request,
                audio.path(),
                &control,
            ) {
                Ok(transcript) => transcript,
                Err(AsrError::Cancelled) => {
                    runtime.finish_chunk(scheduled.reservation_id);
                    runtime.release_range(scheduled.reservation_id);
                    if cancellation.is_cancelled() {
                        return Err(cancelled_failure());
                    }
                    continue;
                }
                Err(error) => {
                    runtime.finish_chunk(scheduled.reservation_id);
                    runtime.release_range(scheduled.reservation_id);
                    return Err(asr_failure(error));
                }
            };
            context.touch_activity();
            if chunk_cancellation.is_cancelled() {
                runtime.finish_chunk(scheduled.reservation_id);
                runtime.release_range(scheduled.reservation_id);
                if cancellation.is_cancelled() {
                    return Err(cancelled_failure());
                }
                continue;
            }

            let transcript = offset_chunk_transcript(
                chunk_transcript,
                scheduled.timing.start_ms,
                scheduled.timing.end_ms,
            )?;
            let words = transcript
                .segments
                .iter()
                .flat_map(|segment| segment.words.iter().cloned())
                .collect::<Vec<_>>();
            let cues =
                segment_words(&words, &SubtitleSegmentationConfig::default()).map_err(|_| {
                    failure(
                        JobFailureCode::EngineFailure,
                        "Subtitler received invalid timestamps from the local speech engine.",
                        false,
                    )
                })?;
            let wall_elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            runtime
                .complete_range(&scheduled, wall_elapsed_ms)
                .map_err(|_| {
                    failure(
                        JobFailureCode::EngineFailure,
                        "Subtitler could not record completed subtitle timing safely.",
                        true,
                    )
                })?;
            runtime.finish_chunk(scheduled.reservation_id);
            runtime.publish_chunk(transcript, cues);
            context.touch_activity();
            report_subtitle_runtime_status(runtime, report_progress);
        }
    }
}

fn report_subtitle_runtime_status(
    runtime: &SubtitleRuntime,
    report_progress: &mut dyn FnMut(JobProgress),
) {
    if let Some(status) = runtime.status_snapshot() {
        report_progress(status.progress);
    }
}

/// whisper.cpp reports chunk-local timestamps. Preserve the transcript shape
/// while moving every segment and word onto the source-media timeline.
fn offset_chunk_transcript(
    mut transcript: Transcript,
    source_start_ms: u64,
    source_end_ms: u64,
) -> Result<Transcript, JobFailure> {
    for segment in &mut transcript.segments {
        segment.timing = offset_time_range(segment.timing, source_start_ms, source_end_ms)?;
        for word in &mut segment.words {
            word.timing = offset_time_range(word.timing, source_start_ms, source_end_ms)?;
        }
    }
    transcript.validate().map_err(|_| {
        failure(
            JobFailureCode::EngineFailure,
            "Subtitler received invalid timestamps from the local speech engine.",
            false,
        )
    })?;
    Ok(transcript)
}

/// Freeze a completed transcript into the cursor order used by the native
/// paging protocol. `Vec::sort_by_key` is stable, so equal-time segments retain
/// their ASR/source order while all ordinary segments are ordered by media
/// time. This happens before the outcome enters the registry; cursor values
/// therefore never depend on later seeks, polling, or export-file ordering.
fn canonicalize_transcript(mut transcript: Transcript) -> Transcript {
    transcript
        .segments
        .sort_by_key(|segment| (segment.timing.start_ms, segment.timing.end_ms));
    transcript
}

fn offset_time_range(
    timing: TimeRange,
    source_start_ms: u64,
    source_end_ms: u64,
) -> Result<TimeRange, JobFailure> {
    let start_ms = timing
        .start_ms
        .checked_add(source_start_ms)
        .ok_or_else(|| {
            failure(
                JobFailureCode::EngineFailure,
                "Subtitler received an out-of-range timestamp from the local speech engine.",
                false,
            )
        })?;
    let end_ms = timing.end_ms.checked_add(source_start_ms).ok_or_else(|| {
        failure(
            JobFailureCode::EngineFailure,
            "Subtitler received an out-of-range timestamp from the local speech engine.",
            false,
        )
    })?;
    // Whisper timestamps are normally bounded by the extracted WAV. Clamp at
    // the known source range anyway so a backend rounding quirk cannot make
    // two adjacent chunks produce overlapping export cues.
    TimeRange::new(
        start_ms.clamp(source_start_ms, source_end_ms),
        end_ms.clamp(source_start_ms, source_end_ms),
    )
    .map_err(|_| {
        failure(
            JobFailureCode::EngineFailure,
            "Subtitler received invalid timestamps from the local speech engine.",
            false,
        )
    })
}

fn private_engine_root() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("Subtitler")
}

impl LocalTranscriptionRunner {
    /// Create the local-only FFmpeg input for a job. A direct URL is never
    /// handed to the decoder: it is acquired once into a job-private file and
    /// kept alive by `AcquiredAudioInput` through the whole job.
    fn acquire_audio_input_from_spec(
        &self,
        spec: &JobSpec,
        cancellation: &JobCancellation,
    ) -> Result<AcquiredAudioInput, JobFailure> {
        let validator = MediaSourceValidator::default();
        match &spec.media.source {
            MediaSource::DirectUrl { media_url } => {
                let url = validator.validate_remote_url(media_url).map_err(|error| {
                    remote_media_failure(RemoteMediaAcquisitionError::Policy(error))
                })?;
                let media = self
                    .remote_acquirer
                    .acquire(&url, &cancellation.extraction)
                    .map_err(remote_media_failure)?;
                if cancellation.is_cancelled() {
                    return Err(cancelled_failure());
                }
                Ok(AcquiredAudioInput::Downloaded(media))
            }
            MediaSource::LocalFile { path } => validator
                .validate_local_path(path)
                .map(AudioInput::LocalPath)
                .map(AcquiredAudioInput::Local)
                .map_err(|_| {
                    failure(
                        JobFailureCode::UnsupportedMedia,
                        "Subtitler needs an absolute local media file path.",
                        false,
                    )
                }),
            MediaSource::Page { page_url } => {
                let resolver = self.youtube_resolver.as_ref().ok_or_else(|| {
                    failure(
                        JobFailureCode::UnsupportedMedia,
                        "Subtitler needs its installed YouTube media resolver before it can process this page.",
                        true,
                    )
                })?;
                let temporary_root = self
                    .remote_acquirer
                    .options()
                    .temporary_root
                    .clone()
                    .unwrap_or_else(env::temp_dir);
                let media = resolver
                    .download_audio(page_url, &temporary_root, &cancellation.extraction)
                    .map_err(youtube_resolution_failure)?;
                if cancellation.is_cancelled() {
                    return Err(cancelled_failure());
                }
                Ok(AcquiredAudioInput::Youtube(media))
            }
        }
    }
}

fn youtube_resolution_failure(error: YoutubeResolutionError) -> JobFailure {
    match error {
        YoutubeResolutionError::ToolUnavailable => failure(
            JobFailureCode::UnsupportedMedia,
            "Subtitler needs its installed YouTube media resolver before it can process this page.",
            true,
        ),
        YoutubeResolutionError::UnsupportedPage => failure(
            JobFailureCode::UnsupportedMedia,
            "Subtitler can use this page only when it is a supported YouTube video URL.",
            false,
        ),
        YoutubeResolutionError::TimedOut => failure(
            JobFailureCode::NetworkFailure,
            "Subtitler could not obtain this YouTube recording before the request timed out. Try again.",
            true,
        ),
        YoutubeResolutionError::Cancelled => cancelled_failure(),
        YoutubeResolutionError::TemporaryStorage | YoutubeResolutionError::OutputTooLarge => failure(
            JobFailureCode::InsufficientStorage,
            "This YouTube recording exceeds Subtitler's private temporary-media cache limit.",
            true,
        ),
        YoutubeResolutionError::InvalidOutput => failure(
            JobFailureCode::NetworkFailure,
            "Subtitler could not obtain an accessible media stream from this YouTube recording. It will not bypass access protections.",
            true,
        ),
        YoutubeResolutionError::ResolutionFailed(reason) => match reason {
            YoutubeResolverFailureReason::BotCheck => failure(
                JobFailureCode::NetworkFailure,
                "YouTube requested additional playback verification for this recording. Subtitler's local resolver could not complete it and will not bypass access protections.",
                true,
            ),
            YoutubeResolverFailureReason::AccessDenied => failure(
                JobFailureCode::NetworkFailure,
                "YouTube refused the local audio-media request. Subtitler will not bypass access protections.",
                true,
            ),
            YoutubeResolverFailureReason::FormatUnavailable => failure(
                JobFailureCode::UnsupportedMedia,
                "YouTube did not offer a compatible audio-only representation for this recording.",
                true,
            ),
            YoutubeResolverFailureReason::RuntimeUnavailable => failure(
                JobFailureCode::UnsupportedMedia,
                "Subtitler's installed YouTube JavaScript challenge runtime is unavailable.",
                true,
            ),
            YoutubeResolverFailureReason::Network => failure(
                JobFailureCode::NetworkFailure,
                "Subtitler could not reach YouTube's media service. Check the connection and try again.",
                true,
            ),
            YoutubeResolverFailureReason::Unknown => failure(
                JobFailureCode::NetworkFailure,
                "Subtitler could not obtain an accessible media stream from this YouTube recording. It will not bypass access protections.",
                true,
            ),
        },
    }
}

fn progress_for_fraction(duration_ms: Option<u64>, percent: u64, phase: JobPhase) -> JobProgress {
    let processed_ms = duration_ms
        .unwrap_or_default()
        .saturating_mul(percent.min(100))
        / 100;
    JobProgress {
        media_duration_ms: duration_ms,
        processed_ms,
        subtitle_buffer_ahead_ms: None,
        phase: Some(phase),
        audio_seconds_decoded_ms: if matches!(
            phase,
            JobPhase::Transcribing | JobPhase::Segmenting | JobPhase::Finalizing
        ) {
            duration_ms.unwrap_or_default()
        } else {
            0
        },
        audio_seconds_transcribed_ms: if matches!(
            phase,
            JobPhase::Segmenting | JobPhase::Finalizing
        ) {
            duration_ms.unwrap_or_default()
        } else {
            0
        },
        completed_intervals: if matches!(phase, JobPhase::Segmenting | JobPhase::Finalizing) {
            1
        } else {
            0
        },
        ..JobProgress::default()
    }
}

fn merge_progress(current: &mut JobProgress, update: &JobProgress) {
    if update.media_duration_ms.is_some() {
        current.media_duration_ms = update.media_duration_ms;
    }
    current.processed_ms = current.processed_ms.max(update.processed_ms);
    if update.subtitle_buffer_ahead_ms.is_some() {
        current.subtitle_buffer_ahead_ms = update.subtitle_buffer_ahead_ms;
    }
    if update.phase.is_some() {
        current.phase = update.phase;
    }
    current.media_bytes_processed = current
        .media_bytes_processed
        .max(update.media_bytes_processed);
    current.audio_seconds_decoded_ms = current
        .audio_seconds_decoded_ms
        .max(update.audio_seconds_decoded_ms);
    current.audio_seconds_transcribed_ms = current
        .audio_seconds_transcribed_ms
        .max(update.audio_seconds_transcribed_ms);
    current.completed_intervals = current.completed_intervals.max(update.completed_intervals);
    if update.worker_pid.is_some() {
        current.worker_pid = update.worker_pid;
    }
    if update.worker_status != WorkerStatus::NotStarted {
        current.worker_status = update.worker_status;
    }
}

fn cancelled_failure() -> JobFailure {
    failure(JobFailureCode::Cancelled, "Cancelled by user.", false)
}

fn media_failure(error: FfmpegExtractionError) -> JobFailure {
    match error {
        FfmpegExtractionError::RemoteInputRequiresAcquisition => failure(
            JobFailureCode::UnsupportedMedia,
            "Subtitler must safely download this remote recording before local decoding can begin.",
            false,
        ),
        FfmpegExtractionError::Cancelled => cancelled_failure(),
        FfmpegExtractionError::FfmpegUnavailable => failure(
            JobFailureCode::MediaDecodeFailure,
            "Subtitler's local audio decoder is unavailable. Repair the Subtitler Engine and try again.",
            true,
        ),
        FfmpegExtractionError::OutputTooLarge { .. } => failure(
            JobFailureCode::InsufficientStorage,
            "This recording's normalized audio exceeds the configured temporary-cache limit.",
            true,
        ),
        FfmpegExtractionError::TemporaryStorage => failure(
            JobFailureCode::InsufficientStorage,
            "Subtitler could not create private temporary audio storage.",
            true,
        ),
        FfmpegExtractionError::TimedOut => failure(
            JobFailureCode::MediaDecodeFailure,
            "Audio extraction took too long and was stopped.",
            true,
        ),
        FfmpegExtractionError::ProcessIo
        | FfmpegExtractionError::ProcessingFailed(_)
        | FfmpegExtractionError::OutputMissing
        | FfmpegExtractionError::InvalidOutputFormat => failure(
            JobFailureCode::MediaDecodeFailure,
            "Subtitler could not decode usable audio from this recording.",
            true,
        ),
    }
}

fn remote_media_failure(error: RemoteMediaAcquisitionError) -> JobFailure {
    match error {
        RemoteMediaAcquisitionError::Cancelled => cancelled_failure(),
        RemoteMediaAcquisitionError::Policy(_) | RemoteMediaAcquisitionError::InvalidRedirect => {
            failure(
                JobFailureCode::UnsupportedMedia,
                "Subtitler cannot safely retrieve this media address.",
                false,
            )
        }
        RemoteMediaAcquisitionError::ResponseTooLarge { .. }
        | RemoteMediaAcquisitionError::TemporaryStorage => failure(
            JobFailureCode::InsufficientStorage,
            "This recording exceeds the configured private temporary-media cache limit.",
            true,
        ),
        RemoteMediaAcquisitionError::DnsResolution => failure(
            JobFailureCode::NetworkFailure,
            "Subtitler could not resolve the recording's media host. Check that the link is still accessible and try again.",
            true,
        ),
        RemoteMediaAcquisitionError::Network => failure(
            JobFailureCode::NetworkFailure,
            "Subtitler could not establish a safe connection to this recording's media host. Check that the link is still accessible and try again.",
            true,
        ),
        RemoteMediaAcquisitionError::RedirectLimitExceeded => failure(
            JobFailureCode::NetworkFailure,
            "This recording's media server redirected too many times. Try the recording again from its original page.",
            true,
        ),
        RemoteMediaAcquisitionError::UnexpectedResponse => failure(
            JobFailureCode::NetworkFailure,
            "This recording's media server returned an unsupported response. Check that the link is still accessible and try again.",
            true,
        ),
        RemoteMediaAcquisitionError::InvalidPartialResponse => failure(
            JobFailureCode::NetworkFailure,
            "This recording's media server returned partial data Subtitler could not validate. Check that the link is still accessible and try again.",
            true,
        ),
    }
}

fn asr_failure(error: AsrError) -> JobFailure {
    match error {
        AsrError::Cancelled => cancelled_failure(),
        AsrError::EngineUnavailable | AsrError::InvalidConfiguration(_) => failure(
            JobFailureCode::ModelUnavailable,
            "Subtitler's selected local speech model is unavailable or incompatible.",
            true,
        ),
        AsrError::TimedOut { .. } => failure(
            JobFailureCode::EngineFailure,
            "Local speech recognition took too long and was stopped.",
            true,
        ),
        AsrError::InvalidOutput(_) | AsrError::ProcessingFailed(_) => failure(
            JobFailureCode::EngineFailure,
            "The local speech engine could not produce a valid timestamped transcript.",
            true,
        ),
    }
}

fn failure(code: JobFailureCode, message: impl Into<String>, retryable: bool) -> JobFailure {
    JobFailure {
        code,
        message: message.into(),
        retryable,
    }
}

/// Thread-safe dispatcher for one native-host process. The local worker
/// keeps running independently of the popup while this Chrome Native Messaging
/// port remains alive. The subsequent durable-engine phase replaces this
/// in-process registry with persisted private IPC state.
pub struct HostDispatcher {
    state: Arc<Mutex<HostState>>,
    validator: MediaSourceValidator,
    capabilities: NativeCapabilities,
    runner: Arc<dyn JobRunner>,
    persistence: Option<Arc<JobPersistence>>,
}

impl Default for HostDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl HostDispatcher {
    pub fn new() -> Self {
        match LocalTranscriptionRunner::from_environment() {
            Ok(ConfiguredLocalRunner {
                runner,
                advisory,
                persistence,
            }) => Self::with_runner_and_persistence(
                MediaSourceValidator::default(),
                NativeCapabilities {
                    protocol_version: NATIVE_PROTOCOL_VERSION,
                    local_asr_available: true,
                    ffmpeg_available: true,
                    direct_media_acquisition: true,
                    browser_mediated_acquisition: false,
                    cloud_processing_requires_explicit_approval: true,
                    local_processing_advisory: Some(advisory),
                },
                runner,
                Some(persistence),
            ),
            Err(()) => {
                // A planning advisory remains safe to show while the local
                // runtime is unavailable. It does not assert that a model
                // asset is installed or runnable, and it never changes the
                // false availability flags below.
                let local_processing_advisory = collect_hardware_profile()
                    .ok()
                    .and_then(|profile| select_local_model(&profile, None).ok())
                    .map(|selection| selection.advisory());
                Self::with_dependencies(
                    MediaSourceValidator::default(),
                    NativeCapabilities {
                        protocol_version: NATIVE_PROTOCOL_VERSION,
                        local_asr_available: false,
                        ffmpeg_available: false,
                        direct_media_acquisition: false,
                        browser_mediated_acquisition: false,
                        cloud_processing_requires_explicit_approval: true,
                        local_processing_advisory,
                    },
                )
            }
        }
    }

    pub fn with_dependencies(
        validator: MediaSourceValidator,
        capabilities: NativeCapabilities,
    ) -> Self {
        Self::with_runner(validator, capabilities, UnavailableJobRunner)
    }

    fn with_runner<R>(
        validator: MediaSourceValidator,
        capabilities: NativeCapabilities,
        runner: R,
    ) -> Self
    where
        R: JobRunner + 'static,
    {
        Self::with_runner_and_persistence(validator, capabilities, runner, None)
    }

    fn with_runner_and_persistence<R>(
        validator: MediaSourceValidator,
        capabilities: NativeCapabilities,
        runner: R,
        persistence: Option<JobPersistence>,
    ) -> Self
    where
        R: JobRunner + 'static,
    {
        Self {
            state: Arc::new(Mutex::new(HostState::default())),
            validator,
            capabilities,
            runner: Arc::new(runner),
            persistence: persistence.map(Arc::new),
        }
    }

    pub fn dispatch(&self, request: NativeRequest) -> NativeResponse {
        if request.request_id.len() > MAX_NATIVE_REQUEST_ID_BYTES {
            return NativeResponse::error(
                None,
                ProtocolErrorCode::InvalidRequest,
                "Subtitler received an invalid native-messaging request identifier.",
                false,
            );
        }
        let request_id = Some(request.request_id);
        match request.command {
            NativeCommand::Handshake {
                protocol_version,
                extension_version: _,
            } => self.handshake(request_id, protocol_version),
            NativeCommand::Start { job } => self.start(request_id, job),
            NativeCommand::Cancel { job_id } => self.cancel(request_id, job_id),
            NativeCommand::Status { job_id } => self.status(request_id, job_id),
            NativeCommand::Restore { job_id, kind } => self.restore(request_id, job_id, kind),
            NativeCommand::PlaybackUpdate {
                job_id,
                position_ms,
                playback_rate_milli,
                is_paused,
                seek_generation,
            } => self.playback_update(
                request_id,
                job_id,
                position_ms,
                playback_rate_milli,
                is_paused,
                seek_generation,
            ),
            NativeCommand::GetSubtitleCues {
                job_id,
                cursor,
                limit,
            } => self.subtitle_cues(request_id, job_id, cursor, limit),
            NativeCommand::GetTranscriptSegments {
                job_id,
                cursor,
                limit,
            } => self.transcript_segments(request_id, job_id, cursor, limit),
        }
    }

    fn handshake(&self, request_id: Option<String>, protocol_version: u32) -> NativeResponse {
        if protocol_version != NATIVE_PROTOCOL_VERSION {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::IncompatibleProtocol,
                format!(
                    "Subtitler requires native messaging protocol version {NATIVE_PROTOCOL_VERSION}."
                ),
                false,
            );
        }

        NativeResponse {
            request_id,
            body: NativeResponseBody::Handshake {
                native_host_name: NATIVE_HOST_NAME.to_owned(),
                protocol_version: NATIVE_PROTOCOL_VERSION,
                native_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: self.capabilities.clone(),
            },
        }
    }

    fn start(&self, request_id: Option<String>, spec: JobSpec) -> NativeResponse {
        let mut acquisition = match self
            .validator
            .plan(&spec.media, spec.settings.force_generate_with_subtitler)
        {
            Ok(report) => report,
            Err(error) => return media_error_response(request_id, error),
        };

        // A normal YouTube recording page is a narrow exception to the
        // generic page-source policy: the native adapter retrieves a private
        // local audio artifact without importing browser cookies or profiles.
        // All other pages remain browser-mediated and are rejected until a
        // platform adapter hands off a safe direct source.
        if matches!(&spec.media.source, MediaSource::Page { page_url } if supports_youtube_page(page_url))
        {
            acquisition = AcquisitionReport {
                strategy: AcquisitionStrategy::DirectMedia,
                summary: "A supported YouTube recording page will be retrieved locally as private audio for Subtitler processing."
                    .to_owned(),
                requires_user_action: false,
            };
        }

        match acquisition.strategy {
            AcquisitionStrategy::DirectMedia | AcquisitionStrategy::LocalFile => {}
            AcquisitionStrategy::ExistingCaptions => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::UnsupportedMedia,
                    "Reliable captions are available; Subtitler needs their explicit payload before creating a full local transcript.",
                    false,
                )
            }
            AcquisitionStrategy::BrowserMediated | AcquisitionStrategy::Unsupported => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::UnsupportedMedia,
                    "Subtitler needs an accessible direct media stream. It will not copy browser session secrets or bypass media protections.",
                    false,
                )
            }
        }

        if !self.capabilities.local_asr_available || !self.capabilities.ffmpeg_available {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::EngineUnavailable,
                "Subtitler needs its local processing engine, FFmpeg decoder, and selected speech model installed before processing this recording.",
                true,
            );
        }

        let subtitle_runtime = match SubtitleRuntime::for_job(&spec) {
            Ok(runtime) => runtime,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::InvalidRequest,
                    "Subtitler needs a valid positive media duration before it can schedule subtitle buffering.",
                    false,
                )
            }
        };

        let job_id = JobId::new();
        let status = JobStatus::queued(job_id.clone(), spec.kind, spec.media.hints.duration_ms);
        let cancellation = JobCancellation::default();
        let activity_heartbeat = Arc::new(AtomicU64::new(now_unix_ms()));
        let response_status = status.clone();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not access its local job registry.",
                    true,
                )
            }
        };
        state.jobs.insert(
            job_id.clone(),
            ManagedJob {
                status,
                client_job_id: spec.client_job_id.clone(),
                acquisition: acquisition.clone(),
                cancellation: cancellation.clone(),
                activity_heartbeat: Arc::clone(&activity_heartbeat),
                subtitle_runtime: subtitle_runtime.clone(),
                outcome: None,
            },
        );
        persist_job_snapshot(&self.persistence, &job_id, &state.jobs);
        drop(state);

        spawn_job(
            Arc::clone(&self.state),
            Arc::clone(&self.runner),
            self.persistence.clone(),
            job_id,
            spec,
            JobExecutionContext {
                subtitle_runtime,
                activity_heartbeat,
            },
            cancellation,
        );
        NativeResponse {
            request_id,
            body: NativeResponseBody::JobStarted {
                job: response_status,
                acquisition,
            },
        }
    }

    fn cancel(&self, request_id: Option<String>, job_id: JobId) -> NativeResponse {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not access its local job registry.",
                    true,
                )
            }
        };
        let Some(job) = state.jobs.get_mut(&job_id) else {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::UnknownJob,
                "This Subtitler job no longer exists.",
                false,
            );
        };
        if job.status.state.is_terminal() {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::InvalidState,
                "This Subtitler job has already finished and cannot be cancelled.",
                false,
            );
        }
        job.cancellation.cancel();
        if let Some(runtime) = job.subtitle_runtime.as_ref() {
            runtime.cancel_active_chunk();
        }
        if let Err(error) = transition_job(
            &mut job.status,
            JobState::Cancelled,
            Some("Cancelled by user.".to_owned()),
            None,
        ) {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::InvalidState,
                format!("This job cannot be cancelled now: {error}"),
                false,
            );
        }
        mark_state_activity(&mut job.status.progress, JobState::Cancelled);
        let response_status = job.status.clone();
        persist_job_snapshot(&self.persistence, &job_id, &state.jobs);
        NativeResponse {
            request_id,
            body: NativeResponseBody::JobCancelled {
                job: response_status,
            },
        }
    }

    fn status(&self, request_id: Option<String>, job_id: JobId) -> NativeResponse {
        self.apply_activity_watchdog(&job_id);
        self.refresh_worker_activity_status(&job_id);
        let subtitle_runtime = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not access its local job registry.",
                    true,
                )
            }
        };
        let Some(job) = subtitle_runtime.jobs.get(&job_id) else {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::UnknownJob,
                "This Subtitler job no longer exists.",
                false,
            );
        };
        let runtime = job.subtitle_runtime.clone();
        drop(subtitle_runtime);
        if let Some(runtime) = runtime {
            self.refresh_subtitle_runtime_status(&job_id, &runtime);
        }

        let state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not access its local job registry.",
                    true,
                )
            }
        };
        let Some(job) = state.jobs.get(&job_id) else {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::UnknownJob,
                "This Subtitler job no longer exists.",
                false,
            );
        };
        NativeResponse {
            request_id,
            body: NativeResponseBody::JobStatus {
                job: job.status.clone(),
            },
        }
    }

    /// Surface the same heartbeat that protects the job watchdog. Long ASR
    /// work can be legitimate, but the browser must be able to distinguish a
    /// live Whisper process from an unchanged, unresponsive job card.
    fn refresh_worker_activity_status(&self, job_id: &JobId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        {
            let Some(job) = state.jobs.get_mut(job_id) else {
                return;
            };
            if job.status.state.is_terminal() {
                return;
            }
            let heartbeat = job.activity_heartbeat.load(Ordering::Acquire);
            if job
                .status
                .progress
                .last_progress_at_ms
                .is_some_and(|previous| heartbeat <= previous)
            {
                return;
            }
            job.status.progress.last_progress_at_ms = Some(heartbeat);
            job.status.progress.worker_status = WorkerStatus::Active;
        }
        persist_job_snapshot(&self.persistence, job_id, &state.jobs);
    }

    fn apply_activity_watchdog(&self, job_id: &JobId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(job) = state.jobs.get_mut(job_id) else {
            return;
        };
        if job.status.state.is_terminal() {
            return;
        }
        let last_activity = job.activity_heartbeat.load(Ordering::Acquire);
        if now_unix_ms().saturating_sub(last_activity) <= MAX_SILENT_LOCAL_ACTIVITY_MS {
            return;
        }
        job.cancellation.cancel();
        if let Some(runtime) = job.subtitle_runtime.as_ref() {
            runtime.cancel_active_chunk();
        }
        if transition_job(
            &mut job.status,
            JobState::Stale,
            Some(
                "Subtitler's local worker stopped reporting activity. This job was marked stale instead of continuing to appear active."
                    .to_owned(),
            ),
            None,
        )
        .is_ok()
        {
            mark_state_activity(&mut job.status.progress, JobState::Stale);
        }
        persist_job_snapshot(&self.persistence, job_id, &state.jobs);
    }

    /// Rebuild a terminal outcome only from Subtitler's own private export
    /// bundle, keyed by the already-persisted opaque native job UUID. A source
    /// request is intentionally not retained or replayed here.
    fn restore(&self, request_id: Option<String>, job_id: JobId, kind: JobKind) -> NativeResponse {
        let Some(persistence) = self.persistence.as_ref() else {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::UnknownJob,
                "This Subtitler result is no longer available to the local engine.",
                true,
            );
        };

        if let Ok(state) = self.state.lock() {
            if let Some(job) = state.jobs.get(&job_id) {
                if job.status.kind != kind {
                    return NativeResponse::error(
                        request_id,
                        ProtocolErrorCode::InvalidRequest,
                        "Subtitler received a mismatched local job recovery request.",
                        false,
                    );
                }
                return NativeResponse {
                    request_id,
                    body: NativeResponseBody::JobRestored {
                        job: job.status.clone(),
                    },
                };
            }
        }

        let checkpoint = persistence.load(&job_id);
        if checkpoint
            .as_ref()
            .is_some_and(|record| record.kind != kind)
        {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::InvalidRequest,
                "Subtitler received a mismatched local job recovery request.",
                false,
            );
        }
        if let Some(record) = checkpoint
            .as_ref()
            .filter(|record| !record.status.state.is_terminal())
        {
            let mut status = record.status.clone();
            status.state = JobState::Stale;
            status.failure = None;
            status.message = Some(
                "Subtitler's previous local engine connection stopped without recent worker activity. Retry this job to continue."
                    .to_owned(),
            );
            mark_state_activity(&mut status.progress, JobState::Stale);
            let managed = ManagedJob {
                status: status.clone(),
                client_job_id: record.client_job_id.clone(),
                acquisition: AcquisitionReport {
                    strategy: AcquisitionStrategy::DirectMedia,
                    summary: "Recovered a stale local job checkpoint.".to_owned(),
                    requires_user_action: false,
                },
                cancellation: JobCancellation::default(),
                activity_heartbeat: Arc::new(AtomicU64::new(now_unix_ms())),
                subtitle_runtime: None,
                outcome: None,
            };
            if let Ok(mut state) = self.state.lock() {
                state.jobs.insert(job_id.clone(), managed);
                persist_job_snapshot(&self.persistence, &job_id, &state.jobs);
            }
            return NativeResponse {
                request_id,
                body: NativeResponseBody::JobRestored { job: status },
            };
        }

        let restored = match persistence.restore_outcome(&job_id) {
            Ok(outcome) => outcome,
            Err(RestoreError::Missing) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::UnknownJob,
                    "Subtitler could not find a completed local result for this job.",
                    true,
                )
            }
            Err(RestoreError::Invalid) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not safely reopen this local transcript result.",
                    false,
                )
            }
        };
        let words = restored
            .transcript
            .segments
            .iter()
            .flat_map(|segment| segment.words.iter().cloned())
            .collect::<Vec<_>>();
        let cues = match segment_words(&words, &SubtitleSegmentationConfig::default()) {
            Ok(cues) => cues,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not safely rebuild local subtitle timing for this result.",
                    false,
                )
            }
        };
        let duration_ms = restored
            .transcript
            .segments
            .iter()
            .map(|segment| segment.timing.end_ms)
            .max();
        let mut status = checkpoint
            .as_ref()
            .map(|record| record.status.clone())
            .unwrap_or_else(|| JobStatus::queued(job_id.clone(), kind, duration_ms));
        status.kind = kind;
        status.state = JobState::Completed;
        status.failure = None;
        status.message = Some("Transcript and exports are ready locally.".to_owned());
        status.progress.media_duration_ms = status.progress.media_duration_ms.or(duration_ms);
        status.progress.processed_ms = status.progress.media_duration_ms.unwrap_or_default();
        mark_state_activity(&mut status.progress, JobState::Completed);

        let client_job_id = checkpoint.and_then(|record| record.client_job_id);
        let managed = ManagedJob {
            status: status.clone(),
            client_job_id,
            acquisition: AcquisitionReport {
                strategy: AcquisitionStrategy::DirectMedia,
                summary: "Recovered a completed private local result.".to_owned(),
                requires_user_action: false,
            },
            cancellation: JobCancellation::default(),
            activity_heartbeat: Arc::new(AtomicU64::new(now_unix_ms())),
            subtitle_runtime: None,
            outcome: Some(JobOutcome {
                exports: restored.exports,
                transcript: canonicalize_transcript(restored.transcript),
                cues,
            }),
        };
        if let Ok(mut state) = self.state.lock() {
            state.jobs.insert(job_id.clone(), managed);
            persist_job_snapshot(&self.persistence, &job_id, &state.jobs);
        }
        NativeResponse {
            request_id,
            body: NativeResponseBody::JobRestored { job: status },
        }
    }

    /// Accept an untrusted but content-free browser playback observation. A
    /// full transcript intentionally has no subtitle runtime and simply
    /// returns its existing status; it must never become dependent on seeks.
    fn playback_update(
        &self,
        request_id: Option<String>,
        job_id: JobId,
        position_ms: u64,
        playback_rate_milli: u16,
        is_paused: bool,
        seek_generation: u32,
    ) -> NativeResponse {
        // The extension emits 0.25x through 4.0x. Native Messaging input is
        // still untrusted, so keep the scheduler's rate calculation within
        // the same deliberate, user-facing contract.
        if !(250..=4_000).contains(&playback_rate_milli) {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::InvalidRequest,
                "Subtitler received an invalid playback rate.",
                false,
            );
        }
        let runtime = match self.state.lock() {
            Ok(state) => {
                let Some(job) = state.jobs.get(&job_id) else {
                    return NativeResponse::error(
                        request_id,
                        ProtocolErrorCode::UnknownJob,
                        "This Subtitler job no longer exists.",
                        false,
                    );
                };
                if job.status.state.is_terminal() {
                    None
                } else {
                    job.subtitle_runtime.clone()
                }
            }
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not access its local job registry.",
                    true,
                )
            }
        };

        if let Some(runtime) = runtime {
            let update = PlaybackUpdate {
                position_ms,
                playback_rate: f64::from(playback_rate_milli) / 1_000.0,
                is_playing: !is_paused,
                seek_generation: u64::from(seek_generation),
            };
            if runtime.apply_playback(update).is_err() {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::InvalidRequest,
                    "Subtitler received a playback position outside this recording.",
                    false,
                );
            }
            self.refresh_subtitle_runtime_status(&job_id, &runtime);
        }

        self.status(request_id, job_id)
    }

    fn refresh_subtitle_runtime_status(&self, job_id: &JobId, runtime: &SubtitleRuntime) {
        let Some(snapshot) = runtime.status_snapshot() else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(job) = state.jobs.get_mut(job_id) else {
            return;
        };
        if job.status.state != JobState::Processing {
            return;
        }
        merge_progress(&mut job.status.progress, &snapshot.progress);
        job.status.message = Some(snapshot.message);
    }

    fn subtitle_cues(
        &self,
        request_id: Option<String>,
        job_id: JobId,
        cursor: Option<u32>,
        limit: Option<u16>,
    ) -> NativeResponse {
        enum CueSource {
            Completed(Vec<SubtitleCue>),
            Processing(Arc<SubtitleRuntime>),
            /// A durationless subtitle job uses the whole-media path, so it
            /// has no safe partial result yet. The extension still polls this
            /// endpoint while the job is processing; an empty page is the
            /// correct non-terminal response instead of a repeated error.
            ProcessingEmpty,
        }

        let source = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::Internal,
                    "Subtitler could not access its local job registry.",
                    true,
                )
            }
        };
        let Some(job) = source.jobs.get(&job_id) else {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::UnknownJob,
                "This Subtitler job no longer exists.",
                false,
            );
        };
        let cue_source =
            match job.status.state {
                JobState::Completed => match job.outcome.as_ref() {
                    Some(outcome) => CueSource::Completed(outcome.cues.clone()),
                    None => {
                        return NativeResponse::error(
                            request_id,
                            ProtocolErrorCode::Internal,
                            "Subtitler completed this job without a readable subtitle result.",
                            true,
                        )
                    }
                },
                JobState::Processing => match job.subtitle_runtime.clone() {
                    Some(runtime) => CueSource::Processing(runtime),
                    None if job.status.kind == JobKind::SubtitleGeneration => {
                        CueSource::ProcessingEmpty
                    }
                    None => return NativeResponse::error(
                        request_id,
                        ProtocolErrorCode::InvalidState,
                        "Generated subtitles are not available for this playback-independent job.",
                        false,
                    ),
                },
                _ => return NativeResponse::error(
                    request_id,
                    ProtocolErrorCode::InvalidState,
                    "Generated subtitles become available while this subtitle job is processing.",
                    true,
                ),
            };
        drop(source);
        let cues = match cue_source {
            CueSource::Completed(cues) => cues,
            CueSource::Processing(runtime) => runtime.cue_snapshot(),
            CueSource::ProcessingEmpty => Vec::new(),
        };

        let cursor = cursor.unwrap_or_default() as usize;
        if cursor > cues.len() {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::InvalidRequest,
                "The requested subtitle page is outside the generated subtitle result.",
                false,
            );
        }
        let page_limit = usize::from(limit.unwrap_or(MAX_SUBTITLE_CUES_PER_PAGE as u16))
            .clamp(1, MAX_SUBTITLE_CUES_PER_PAGE);
        match build_subtitle_cue_page(
            request_id.clone(),
            job_id.clone(),
            &cues,
            cursor,
            page_limit,
        ) {
            Ok(body) => NativeResponse { request_id, body },
            Err(CuePageError::SingleCueTooLarge) => NativeResponse::error(
                request_id,
                ProtocolErrorCode::ResultTooLarge,
                "A generated subtitle cue is too large to deliver safely to the browser overlay.",
                false,
            ),
            Err(CuePageError::CursorOverflow) => NativeResponse::error(
                request_id,
                ProtocolErrorCode::ResultTooLarge,
                "This subtitle result is too large to page safely.",
                false,
            ),
            Err(CuePageError::Serialization) => NativeResponse::error(
                request_id,
                ProtocolErrorCode::Internal,
                "Subtitler could not prepare the generated subtitle page.",
                true,
            ),
        }
    }

    /// Return only a stable, bounded view of a *completed* canonical
    /// transcript. Unlike subtitle cues, transcript text is not progressively
    /// published: a user opening the transcript panel should never receive a
    /// cursor whose contents can change as later ASR chunks finish.
    fn transcript_segments(
        &self,
        request_id: Option<String>,
        job_id: JobId,
        cursor: Option<u32>,
        limit: Option<u16>,
    ) -> NativeResponse {
        let transcript =
            match self.state.lock() {
                Ok(state) => {
                    let Some(job) = state.jobs.get(&job_id) else {
                        return NativeResponse::error(
                            request_id,
                            ProtocolErrorCode::UnknownJob,
                            "This Subtitler job no longer exists.",
                            false,
                        );
                    };
                    if job.status.state != JobState::Completed {
                        return NativeResponse::error(
                            request_id,
                            ProtocolErrorCode::InvalidState,
                            "The completed transcript becomes available after this job finishes.",
                            job.status.state == JobState::Processing,
                        );
                    }
                    match job.outcome.as_ref() {
                        Some(outcome) => outcome.transcript.clone(),
                        None => return NativeResponse::error(
                            request_id,
                            ProtocolErrorCode::Internal,
                            "Subtitler completed this job without a readable transcript result.",
                            true,
                        ),
                    }
                }
                Err(_) => {
                    return NativeResponse::error(
                        request_id,
                        ProtocolErrorCode::Internal,
                        "Subtitler could not access its local job registry.",
                        true,
                    )
                }
            };

        let cursor = cursor.unwrap_or_default() as usize;
        if cursor > transcript.segments.len() {
            return NativeResponse::error(
                request_id,
                ProtocolErrorCode::InvalidRequest,
                "The requested transcript page is outside the completed transcript.",
                false,
            );
        }
        let page_limit = usize::from(limit.unwrap_or(MAX_TRANSCRIPT_SEGMENTS_PER_PAGE as u16))
            .clamp(1, MAX_TRANSCRIPT_SEGMENTS_PER_PAGE);
        match build_transcript_segment_page(
            request_id.clone(),
            job_id.clone(),
            &transcript.segments,
            cursor,
            page_limit,
        ) {
            Ok(body) => NativeResponse { request_id, body },
            Err(TranscriptPageError::SingleSegmentTooLarge) => NativeResponse::error(
                request_id,
                ProtocolErrorCode::ResultTooLarge,
                "A transcript segment is too large to deliver safely to the browser.",
                false,
            ),
            Err(TranscriptPageError::CursorOverflow) => NativeResponse::error(
                request_id,
                ProtocolErrorCode::ResultTooLarge,
                "This completed transcript is too large to page safely.",
                false,
            ),
            Err(TranscriptPageError::Serialization) => NativeResponse::error(
                request_id,
                ProtocolErrorCode::Internal,
                "Subtitler could not prepare the completed transcript page.",
                true,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CuePageError {
    SingleCueTooLarge,
    CursorOverflow,
    Serialization,
}

fn build_subtitle_cue_page(
    request_id: Option<String>,
    job_id: JobId,
    all_cues: &[SubtitleCue],
    cursor: usize,
    limit: usize,
) -> Result<NativeResponseBody, CuePageError> {
    let upper_bound = cursor.saturating_add(limit).min(all_cues.len());
    let mut cues = Vec::with_capacity(upper_bound.saturating_sub(cursor));
    let mut next_index = cursor;

    while next_index < upper_bound {
        cues.push(all_cues[next_index].clone());
        let following_index = next_index.saturating_add(1);
        let next_cursor = if following_index < all_cues.len() {
            Some(u32::try_from(following_index).map_err(|_| CuePageError::CursorOverflow)?)
        } else {
            None
        };
        let candidate = NativeResponse {
            request_id: request_id.clone(),
            body: NativeResponseBody::SubtitleCues {
                job_id: job_id.clone(),
                cues: cues.clone(),
                next_cursor,
            },
        };
        let bytes = serde_json::to_vec(&candidate)
            .map_err(|_| CuePageError::Serialization)?
            .len();
        if bytes > MAX_SUBTITLE_CUE_PAGE_BYTES {
            cues.pop();
            break;
        }
        next_index = following_index;
    }

    if cues.is_empty() && cursor < all_cues.len() {
        return Err(CuePageError::SingleCueTooLarge);
    }
    let next_cursor = if next_index < all_cues.len() {
        Some(u32::try_from(next_index).map_err(|_| CuePageError::CursorOverflow)?)
    } else {
        None
    };
    Ok(NativeResponseBody::SubtitleCues {
        job_id,
        cues,
        next_cursor,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptPageError {
    SingleSegmentTooLarge,
    CursorOverflow,
    Serialization,
}

/// Build a response page without exposing canonical transcript internals. The
/// candidate response is serialized before each item is accepted, which makes
/// the byte bound cover the native-message envelope as well as UTF-8 text.
fn build_transcript_segment_page(
    request_id: Option<String>,
    job_id: JobId,
    all_segments: &[TranscriptSegment],
    cursor: usize,
    limit: usize,
) -> Result<NativeResponseBody, TranscriptPageError> {
    let upper_bound = cursor.saturating_add(limit).min(all_segments.len());
    let mut segments = Vec::with_capacity(upper_bound.saturating_sub(cursor));
    let mut next_index = cursor;

    while next_index < upper_bound {
        let source = &all_segments[next_index];
        if source.text.len() > MAX_TRANSCRIPT_SEGMENT_TEXT_BYTES
            || source
                .speaker
                .as_ref()
                .is_some_and(|speaker| speaker.len() > MAX_TRANSCRIPT_SEGMENT_SPEAKER_BYTES)
        {
            return Err(TranscriptPageError::SingleSegmentTooLarge);
        }
        segments.push(TranscriptSegmentPageItem::from(source));
        let following_index = next_index.saturating_add(1);
        let next_cursor = if following_index < all_segments.len() {
            Some(u32::try_from(following_index).map_err(|_| TranscriptPageError::CursorOverflow)?)
        } else {
            None
        };
        let candidate = NativeResponse {
            request_id: request_id.clone(),
            body: NativeResponseBody::TranscriptSegments {
                job_id: job_id.clone(),
                segments: segments.clone(),
                next_cursor,
            },
        };
        let bytes = serde_json::to_vec(&candidate)
            .map_err(|_| TranscriptPageError::Serialization)?
            .len();
        if bytes > MAX_TRANSCRIPT_SEGMENT_PAGE_BYTES {
            segments.pop();
            break;
        }
        next_index = following_index;
    }

    if segments.is_empty() && cursor < all_segments.len() {
        return Err(TranscriptPageError::SingleSegmentTooLarge);
    }
    let next_cursor = if next_index < all_segments.len() {
        Some(u32::try_from(next_index).map_err(|_| TranscriptPageError::CursorOverflow)?)
    } else {
        None
    };
    Ok(NativeResponseBody::TranscriptSegments {
        job_id,
        segments,
        next_cursor,
    })
}

fn spawn_job(
    state: Arc<Mutex<HostState>>,
    runner: Arc<dyn JobRunner>,
    persistence: Option<Arc<JobPersistence>>,
    job_id: JobId,
    spec: JobSpec,
    context: JobExecutionContext,
    cancellation: JobCancellation,
) {
    thread::spawn(move || {
        if !advance_job(
            &state,
            &persistence,
            &job_id,
            JobState::Discovering,
            "Inspecting the local media source.",
        ) {
            return;
        }
        if !advance_job(
            &state,
            &persistence,
            &job_id,
            JobState::Acquiring,
            "Extracting and normalizing audio locally.",
        ) {
            return;
        }
        let processing_message = if spec.kind == JobKind::SubtitleGeneration
            && context.subtitle_runtime().is_none()
        {
            "Creating local subtitles from the full recording because this player did not report a reliable duration."
        } else if spec.kind == JobKind::SubtitleGeneration {
            "Generating timestamped local subtitles ahead of playback."
        } else {
            "Creating a timestamped local transcript."
        };
        if !advance_job(
            &state,
            &persistence,
            &job_id,
            JobState::Processing,
            processing_message,
        ) {
            return;
        }

        let state_for_progress = Arc::clone(&state);
        let persistence_for_progress = persistence.clone();
        let progress_job_id = job_id.clone();
        let mut report_progress = move |progress: JobProgress| {
            let Ok(mut state) = state_for_progress.lock() else {
                return;
            };
            let Some(job) = state.jobs.get_mut(&progress_job_id) else {
                return;
            };
            if job.status.state != JobState::Processing {
                return;
            }
            merge_progress(&mut job.status.progress, &progress);
            mark_progress_activity(&mut job.status.progress);
            persist_job_snapshot(&persistence_for_progress, &progress_job_id, &state.jobs);
        };
        let result = runner.run(
            &job_id,
            &spec,
            &context,
            &cancellation,
            &mut report_progress,
        );
        finish_job(&state, &persistence, &job_id, &cancellation, result);
    });
}

fn advance_job(
    state: &Arc<Mutex<HostState>>,
    persistence: &Option<Arc<JobPersistence>>,
    job_id: &JobId,
    next: JobState,
    message: &str,
) -> bool {
    let Ok(mut state) = state.lock() else {
        return false;
    };
    let Some(job) = state.jobs.get_mut(job_id) else {
        return false;
    };
    if job.cancellation.is_cancelled() || job.status.state.is_terminal() {
        return false;
    }
    let transitioned =
        transition_job(&mut job.status, next, Some(message.to_owned()), None).is_ok();
    if transitioned {
        mark_state_activity(&mut job.status.progress, next);
        persist_job_snapshot(persistence, job_id, &state.jobs);
    }
    transitioned
}

fn finish_job(
    state: &Arc<Mutex<HostState>>,
    persistence: &Option<Arc<JobPersistence>>,
    job_id: &JobId,
    cancellation: &JobCancellation,
    result: Result<JobOutcome, JobFailure>,
) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    let Some(job) = state.jobs.get_mut(job_id) else {
        return;
    };
    if job.status.state != JobState::Processing {
        return;
    }
    if cancellation.is_cancelled() {
        let _ = transition_job(
            &mut job.status,
            JobState::Cancelled,
            Some("Cancelled by user.".to_owned()),
            None,
        );
        return;
    }
    match result {
        Ok(mut outcome) => {
            // Test/development runners and future providers all pass through
            // this final gate, so the registry invariant does not depend on a
            // particular ASR implementation remembering to sort its output.
            outcome.transcript = canonicalize_transcript(outcome.transcript);
            job.outcome = Some(outcome);
            if let Some(duration) = job.status.progress.media_duration_ms {
                job.status.progress.processed_ms = duration;
            }
            let _ = transition_job(
                &mut job.status,
                JobState::Completed,
                Some("Transcript and exports are ready locally.".to_owned()),
                None,
            );
            mark_state_activity(&mut job.status.progress, JobState::Completed);
        }
        Err(job_failure) if job_failure.code == JobFailureCode::Cancelled => {
            let _ = transition_job(
                &mut job.status,
                JobState::Cancelled,
                Some("Cancelled by user.".to_owned()),
                None,
            );
            mark_state_activity(&mut job.status.progress, JobState::Cancelled);
        }
        Err(job_failure) => {
            let _ = transition_job(
                &mut job.status,
                JobState::Failed,
                Some(job_failure.message.clone()),
                Some(job_failure),
            );
            mark_state_activity(&mut job.status.progress, JobState::Failed);
        }
    }
    persist_job_snapshot(persistence, job_id, &state.jobs);
}

fn persist_job_snapshot(
    persistence: &Option<Arc<JobPersistence>>,
    job_id: &JobId,
    jobs: &HashMap<JobId, ManagedJob>,
) {
    let Some(persistence) = persistence.as_ref() else {
        return;
    };
    let Some(job) = jobs.get(job_id) else {
        return;
    };
    persistence.write(job.client_job_id.as_deref(), job.status.kind, &job.status);
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn mark_state_activity(progress: &mut JobProgress, state: JobState) {
    progress.phase = Some(match state {
        JobState::Queued | JobState::Discovering => JobPhase::Resolving,
        JobState::Acquiring => JobPhase::Acquiring,
        JobState::Processing => JobPhase::Decoding,
        JobState::Completed => JobPhase::Complete,
        JobState::Cancelled => JobPhase::Cancelled,
        JobState::Stale => JobPhase::Stale,
        JobState::Failed => JobPhase::Failed,
    });
    progress.last_progress_at_ms = Some(now_unix_ms());
    progress.worker_status = match state {
        JobState::Queued => WorkerStatus::NotStarted,
        JobState::Stale => WorkerStatus::Unavailable,
        JobState::Completed | JobState::Cancelled | JobState::Failed => WorkerStatus::Finished,
        _ => WorkerStatus::Active,
    };
    if progress.worker_status == WorkerStatus::Finished {
        progress.worker_pid = None;
    }
}

fn mark_progress_activity(progress: &mut JobProgress) {
    progress.last_progress_at_ms = Some(now_unix_ms());
    progress.worker_status = WorkerStatus::Active;
}

fn media_error_response(request_id: Option<String>, error: MediaError) -> NativeResponse {
    let (code, retryable) = match error {
        MediaError::ProtectedMedia => (ProtocolErrorCode::ProtectedMedia, false),
        MediaError::DecoderUnavailable => (ProtocolErrorCode::EngineUnavailable, true),
        MediaError::InvalidRemoteUrl
        | MediaError::UnsupportedScheme
        | MediaError::EmbeddedCredentials
        | MediaError::PrivateNetworkUrl
        | MediaError::LocalFilesDisabled
        | MediaError::InvalidLocalPath
        | MediaError::NetworkLocalPath => (ProtocolErrorCode::UnsupportedMedia, false),
    };
    NativeResponse::error(request_id, code, error.to_string(), retryable)
}

/// Read one Chrome Native Messaging frame. An EOF before a new frame is a
/// normal host shutdown signal; truncated frames are errors.
pub fn read_native_message<R: Read>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, NativeMessagingError> {
    let mut header = [0_u8; 4];
    let bytes_read = reader.read(&mut header)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[bytes_read..])?;
    let payload_len = u32::from_le_bytes(header);
    if payload_len == 0 {
        return Err(NativeMessagingError::InvalidFrame(
            "native messaging payloads cannot be empty".to_owned(),
        ));
    }
    if payload_len > MAX_NATIVE_MESSAGE_BYTES {
        return Err(NativeMessagingError::MessageTooLarge {
            received: payload_len,
            maximum: MAX_NATIVE_MESSAGE_BYTES,
        });
    }

    let mut payload = vec![0_u8; payload_len as usize];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// Serialize and frame one response. Only this function writes protocol bytes
/// to stdout; diagnostic messages belong on stderr and omit sensitive media.
pub fn write_native_message<W: Write>(
    writer: &mut W,
    message: &impl Serialize,
) -> Result<(), NativeMessagingError> {
    let payload = serde_json::to_vec(message)
        .map_err(|error| NativeMessagingError::Serialization(error.to_string()))?;
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| NativeMessagingError::MessageTooLarge {
            received: u32::MAX,
            maximum: MAX_NATIVE_MESSAGE_BYTES,
        })?;
    if payload_len > MAX_NATIVE_MESSAGE_BYTES {
        return Err(NativeMessagingError::MessageTooLarge {
            received: payload_len,
            maximum: MAX_NATIVE_MESSAGE_BYTES,
        });
    }
    writer.write_all(&payload_len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Run a Native Messaging event loop using any I/O pair. Generic I/O makes the
/// framing and dispatcher layer independently testable without a browser.
pub fn run_native_host<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    dispatcher: &HostDispatcher,
) -> Result<(), NativeMessagingError> {
    while let Some(payload) = read_native_message(reader)? {
        let response = match serde_json::from_slice::<NativeRequest>(&payload) {
            Ok(request) => dispatcher.dispatch(request),
            Err(_) => NativeResponse::error(
                None,
                ProtocolErrorCode::InvalidRequest,
                "Subtitler received an invalid native-messaging request.",
                false,
            ),
        };
        write_native_message(writer, &response)?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum NativeMessagingError {
    #[error("native messaging I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid native messaging frame: {0}")]
    InvalidFrame(String),
    #[error("native messaging message is too large ({received} bytes; maximum is {maximum})")]
    MessageTooLarge { received: u32, maximum: u32 },
    #[error("could not serialize native messaging JSON: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };
    use subtitler_core::{
        JobKind, MediaAccessHints, MediaRequest, ProcessingPreference, TimeRange, Transcript,
        TranscriptSegment, WordTimestamp,
    };

    #[test]
    fn hardware_observation_is_conservative_and_does_not_invent_accelerators() {
        let profile = hardware_profile_from_observation(
            Some(16),
            Some(32 * 1_024 * BYTES_PER_MIB),
            Vec::new(),
        );
        assert_eq!(profile.logical_cpu_count, 16);
        assert_eq!(profile.available_memory_mb, 32 * 1_024);
        assert!(profile.supported_backends.is_empty());
        assert_eq!(profile.preferred_backend(), ComputeBackend::Cpu);

        let unknown = hardware_profile_from_observation(None, None, vec![ComputeBackend::Cpu]);
        assert_eq!(unknown.logical_cpu_count, 0);
        assert_eq!(unknown.available_memory_mb, 0);
        assert!(unknown.supported_backends.is_empty());
    }

    #[test]
    fn compiled_backend_declarations_require_explicit_valid_platform_values() {
        assert_eq!(
            parse_declared_backends("windows", Some("cpu,cuda,vulkan")).unwrap(),
            vec![
                ComputeBackend::Cpu,
                ComputeBackend::Cuda,
                ComputeBackend::Vulkan
            ]
        );
        assert_eq!(
            parse_declared_backends("macos", Some("metal")).unwrap(),
            vec![ComputeBackend::Metal]
        );
        assert!(parse_declared_backends("windows", Some("metal")).is_err());
        assert!(parse_declared_backends("windows", Some("cuda,cuda")).is_err());
        assert!(parse_declared_backends("windows", Some("cuda,unknown")).is_err());
        assert!(parse_declared_backends("windows", Some("CUDA")).is_err());
        assert_eq!(
            parse_declared_backends("windows", None).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn automatic_selection_uses_hardware_policy_but_only_as_safe_metadata() {
        let profile = hardware_profile_from_observation(
            Some(12),
            Some(32 * 1_024 * BYTES_PER_MIB),
            Vec::new(),
        );
        let selection = select_local_model(&profile, None).unwrap();
        assert_eq!(selection.source, LocalModelSelectionSource::Automatic);
        assert_eq!(selection.model, LocalModel::Medium);
        assert_eq!(selection.quantization, Quantization::Q8_0);
        assert_eq!(selection.backend, ComputeBackend::Cpu);
        assert_eq!(selection.local_performance, LocalPerformance::Excellent);

        let advisory = selection.advisory();
        let value = serde_json::to_value(advisory).unwrap();
        assert_eq!(value["selection_source"], "automatic");
        assert_eq!(value["model"], "medium");
        assert_eq!(value["quantization"], "q8_0");
        assert_eq!(value["backend"], "cpu");
        assert_eq!(value["local_performance"], "excellent");
        assert!(value.get("available_memory_mb").is_none());
        assert!(value.get("model_path").is_none());
    }

    #[test]
    fn known_developer_base_asset_runs_above_its_smaller_safety_floor() {
        let constrained =
            hardware_profile_from_observation(Some(8), Some(1_600 * BYTES_PER_MIB), Vec::new());
        let selection = select_developer_layout_model(&constrained, None).unwrap();
        assert_eq!(selection.source, LocalModelSelectionSource::Automatic);
        assert_eq!(selection.model, LocalModel::Base);
        assert_eq!(selection.quantization, Quantization::F16);
        assert_eq!(selection.backend, ComputeBackend::Cpu);
        assert_eq!(selection.local_performance, LocalPerformance::CloudHelpful);

        let unsafe_profile = hardware_profile_from_observation(
            Some(8),
            Some((DEVELOPER_BASE_MIN_AVAILABLE_MEMORY_MB - 1) * BYTES_PER_MIB),
            Vec::new(),
        );
        assert!(select_developer_layout_model(&unsafe_profile, None).is_err());
    }

    #[test]
    fn advanced_selection_requires_a_declared_backend_and_rejects_known_bad_fits() {
        let profile = hardware_profile_from_observation(
            Some(8),
            Some(16 * 1_024 * BYTES_PER_MIB),
            Vec::new(),
        );
        let selection = select_local_model(
            &profile,
            Some(AdvancedModelOverride {
                model: LocalModel::Small,
                quantization: Quantization::Q5Km,
                backend: ComputeBackend::Cpu,
            }),
        )
        .unwrap();
        assert_eq!(
            selection.source,
            LocalModelSelectionSource::AdvancedEnvironment
        );

        assert!(select_local_model(
            &profile,
            Some(AdvancedModelOverride {
                model: LocalModel::Small,
                quantization: Quantization::Q5Km,
                backend: ComputeBackend::Cuda,
            }),
        )
        .is_err());
        assert!(select_local_model(
            &profile,
            Some(AdvancedModelOverride {
                model: LocalModel::LargeV3Turbo,
                quantization: Quantization::Q5Km,
                backend: ComputeBackend::Cpu,
            }),
        )
        .is_err());
        let constrained =
            hardware_profile_from_observation(Some(4), Some(8 * 1_024 * BYTES_PER_MIB), Vec::new());
        assert!(select_local_model(
            &constrained,
            Some(AdvancedModelOverride {
                model: LocalModel::Medium,
                quantization: Quantization::Q5Km,
                backend: ComputeBackend::Cpu,
            }),
        )
        .is_err());
    }

    #[test]
    fn explicit_selection_can_remain_an_advanced_choice_when_hardware_readings_are_incomplete() {
        let incomplete = hardware_profile_from_observation(None, None, Vec::new());
        let selection = select_local_model(
            &incomplete,
            Some(AdvancedModelOverride {
                model: LocalModel::Tiny,
                quantization: Quantization::Q5_0,
                backend: ComputeBackend::Cpu,
            }),
        )
        .unwrap();
        assert_eq!(
            selection.source,
            LocalModelSelectionSource::AdvancedEnvironment
        );
        assert_eq!(selection.local_performance, LocalPerformance::CloudHelpful);
    }

    fn test_exports(job_id: &JobId) -> ExportBundle {
        ExportBundle {
            directory: PathBuf::from(format!("C:/Subtitler/exports/{job_id}")),
            transcript_txt: PathBuf::from("C:/Subtitler/exports/Transcript.txt"),
            timestamped_txt: PathBuf::from("C:/Subtitler/exports/Transcript-timestamped.txt"),
            subtitles_srt: PathBuf::from("C:/Subtitler/exports/Subtitles.srt"),
            subtitles_vtt: PathBuf::from("C:/Subtitler/exports/Subtitles.vtt"),
            transcript_json: PathBuf::from("C:/Subtitler/exports/Transcript.json"),
        }
    }

    fn test_cue(start_ms: u64, end_ms: u64, text: impl Into<String>) -> SubtitleCue {
        SubtitleCue {
            timing: TimeRange::new(start_ms, end_ms).unwrap(),
            lines: vec![text.into()],
            speaker: None,
        }
    }

    fn test_transcript_segment(
        start_ms: u64,
        end_ms: u64,
        text: impl Into<String>,
        speaker: Option<&str>,
    ) -> TranscriptSegment {
        TranscriptSegment {
            timing: TimeRange::new(start_ms, end_ms).unwrap(),
            text: text.into(),
            speaker: speaker.map(str::to_owned),
            words: vec![WordTimestamp {
                // This proves the paging DTO never leaks word-level ASR
                // payloads to the extension.
                text: "internal-word-timestamp".to_owned(),
                timing: TimeRange::new(start_ms, end_ms).unwrap(),
                speaker: Some("internal-word-speaker".to_owned()),
            }],
        }
    }

    fn test_transcript() -> Transcript {
        // Deliberately reverse the source order so outcome canonicalization is
        // covered by the public transcript-page tests below.
        Transcript {
            language: "en".to_owned(),
            translated_from: Some("fr".to_owned()),
            segments: vec![
                test_transcript_segment(
                    1_200,
                    2_200,
                    "Second completed transcript segment.",
                    Some("Speaker 2"),
                ),
                test_transcript_segment(
                    0,
                    1_000,
                    "First completed transcript segment.",
                    Some("Speaker 1"),
                ),
            ],
        }
    }

    fn empty_transcript() -> Transcript {
        Transcript {
            language: "en".to_owned(),
            translated_from: None,
            segments: Vec::new(),
        }
    }

    #[derive(Clone, Copy)]
    struct SuccessfulRunner;

    impl JobRunner for SuccessfulRunner {
        fn run(
            &self,
            job_id: &JobId,
            spec: &JobSpec,
            _context: &JobExecutionContext,
            _cancellation: &JobCancellation,
            report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            report_progress(progress_for_fraction(
                spec.media.hints.duration_ms,
                75,
                JobPhase::Transcribing,
            ));
            Ok(JobOutcome {
                exports: test_exports(job_id),
                transcript: canonicalize_transcript(test_transcript()),
                cues: vec![
                    test_cue(0, 1_000, "First generated subtitle."),
                    test_cue(1_200, 2_200, "Second generated subtitle."),
                ],
            })
        }
    }

    #[derive(Clone, Copy)]
    struct BlockingRunner;

    impl JobRunner for BlockingRunner {
        fn run(
            &self,
            _job_id: &JobId,
            _spec: &JobSpec,
            _context: &JobExecutionContext,
            cancellation: &JobCancellation,
            _report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            while !cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            Err(cancelled_failure())
        }
    }

    #[derive(Clone)]
    struct SeekAwareRunner {
        active: Arc<AtomicBool>,
        preempted: Arc<AtomicBool>,
    }

    impl JobRunner for SeekAwareRunner {
        fn run(
            &self,
            _job_id: &JobId,
            _spec: &JobSpec,
            context: &JobExecutionContext,
            cancellation: &JobCancellation,
            _report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            let runtime = context
                .subtitle_runtime()
                .expect("subtitle jobs with a duration receive a scheduler runtime");
            let scheduled = runtime
                .next_range()
                .expect("the initial subtitle buffer should lease a range");
            let chunk_cancellation = JobCancellation::default();
            runtime.begin_chunk(&scheduled, chunk_cancellation.clone());
            runtime.publish_chunk(
                Transcript {
                    language: "en".to_owned(),
                    translated_from: None,
                    segments: Vec::new(),
                },
                vec![test_cue(0, 900, "Partial generated subtitle.")],
            );
            self.active.store(true, Ordering::Release);

            while !cancellation.is_cancelled() && !chunk_cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            if chunk_cancellation.is_cancelled() && !cancellation.is_cancelled() {
                self.preempted.store(true, Ordering::Release);
                runtime.finish_chunk(scheduled.reservation_id);
                runtime.release_range(scheduled.reservation_id);
            }
            while !cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            Err(cancelled_failure())
        }
    }

    #[derive(Clone, Copy)]
    struct ManyCueRunner;

    impl JobRunner for ManyCueRunner {
        fn run(
            &self,
            job_id: &JobId,
            _spec: &JobSpec,
            _context: &JobExecutionContext,
            _cancellation: &JobCancellation,
            _report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            let cues = (0..(MAX_SUBTITLE_CUES_PER_PAGE + 10))
                .map(|index| {
                    let start_ms = (index as u64).saturating_mul(1_000);
                    test_cue(
                        start_ms,
                        start_ms.saturating_add(900),
                        format!("Generated cue {index}"),
                    )
                })
                .collect();
            Ok(JobOutcome {
                exports: test_exports(job_id),
                transcript: empty_transcript(),
                cues,
            })
        }
    }

    #[derive(Clone, Copy)]
    struct EmptyCueRunner;

    impl JobRunner for EmptyCueRunner {
        fn run(
            &self,
            job_id: &JobId,
            _spec: &JobSpec,
            _context: &JobExecutionContext,
            _cancellation: &JobCancellation,
            _report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            Ok(JobOutcome {
                exports: test_exports(job_id),
                transcript: empty_transcript(),
                cues: Vec::new(),
            })
        }
    }

    #[derive(Clone, Copy)]
    struct ManyTranscriptRunner;

    impl JobRunner for ManyTranscriptRunner {
        fn run(
            &self,
            job_id: &JobId,
            _spec: &JobSpec,
            _context: &JobExecutionContext,
            _cancellation: &JobCancellation,
            _report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            let segments = (0..(MAX_TRANSCRIPT_SEGMENTS_PER_PAGE + 10))
                .rev()
                .map(|index| {
                    let start_ms = (index as u64).saturating_mul(1_000);
                    test_transcript_segment(
                        start_ms,
                        start_ms.saturating_add(900),
                        format!("Completed transcript segment {index}."),
                        None,
                    )
                })
                .collect();
            Ok(JobOutcome {
                exports: test_exports(job_id),
                transcript: Transcript {
                    language: "en".to_owned(),
                    translated_from: None,
                    segments,
                },
                cues: Vec::new(),
            })
        }
    }

    #[derive(Clone, Copy)]
    struct OversizedTranscriptRunner;

    impl JobRunner for OversizedTranscriptRunner {
        fn run(
            &self,
            job_id: &JobId,
            _spec: &JobSpec,
            _context: &JobExecutionContext,
            _cancellation: &JobCancellation,
            _report_progress: &mut dyn FnMut(JobProgress),
        ) -> Result<JobOutcome, JobFailure> {
            Ok(JobOutcome {
                exports: test_exports(job_id),
                transcript: Transcript {
                    language: "en".to_owned(),
                    translated_from: None,
                    segments: vec![test_transcript_segment(
                        0,
                        1_000,
                        "x".repeat(MAX_TRANSCRIPT_SEGMENT_TEXT_BYTES + 1),
                        None,
                    )],
                },
                cues: Vec::new(),
            })
        }
    }

    fn capable_dispatcher<R: JobRunner + 'static>(runner: R) -> HostDispatcher {
        capable_dispatcher_with_advisory(runner, None)
    }

    fn capable_dispatcher_with_advisory<R: JobRunner + 'static>(
        runner: R,
        local_processing_advisory: Option<LocalProcessingAdvisory>,
    ) -> HostDispatcher {
        HostDispatcher::with_runner(
            MediaSourceValidator::default(),
            NativeCapabilities {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                local_asr_available: true,
                ffmpeg_available: true,
                direct_media_acquisition: true,
                browser_mediated_acquisition: false,
                cloud_processing_requires_explicit_approval: true,
                local_processing_advisory,
            },
            runner,
        )
    }

    fn direct_job() -> JobSpec {
        JobSpec {
            client_job_id: Some("extension-correlation".to_owned()),
            kind: JobKind::FullTranscript,
            media: MediaRequest {
                source: MediaSource::DirectUrl {
                    media_url: "https://media.example.test/recording.mp4".to_owned(),
                },
                hints: MediaAccessHints {
                    duration_ms: Some(60_000),
                    ..MediaAccessHints::default()
                },
            },
            settings: subtitler_core::JobSettings {
                processing_preference: ProcessingPreference::LocalOnly,
                ..subtitler_core::JobSettings::default()
            },
        }
    }

    fn youtube_page_job() -> JobSpec {
        let mut job = direct_job();
        job.media.source = MediaSource::Page {
            page_url: "https://youtu.be/ESjPc7I5h_Q?si=test".to_owned(),
        };
        job.settings.force_generate_with_subtitler = true;
        job
    }

    fn subtitle_job() -> JobSpec {
        let mut job = direct_job();
        job.kind = JobKind::SubtitleGeneration;
        job.media.hints.duration_ms = Some(120_000);
        job
    }

    #[test]
    fn initial_subtitle_playback_starts_the_first_window_near_the_playhead() {
        let mut job = subtitle_job();
        job.settings.initial_playback = Some(InitialPlayback {
            position_ms: 60_000,
            playback_rate_milli: 1_000,
            is_paused: true,
        });

        let runtime = SubtitleRuntime::for_job(&job)
            .expect("initial playback is valid")
            .expect("subtitle jobs with duration use the scheduler");
        let range = runtime
            .next_range()
            .expect("the initial subtitle range is scheduled");

        // The configured five-second context is intentional. Crucially, this
        // is not the default 0:00-0:30 window that used to introduce an
        // avoidable wait after opening Subtitler at a later point.
        assert_eq!(range.timing.start_ms, 55_000);
        assert_eq!(range.timing.end_ms, 85_000);
    }

    fn durationless_subtitle_job() -> JobSpec {
        let mut job = subtitle_job();
        job.media.hints.duration_ms = None;
        job
    }

    fn status_for(dispatcher: &HostDispatcher, job_id: JobId) -> JobStatus {
        match dispatcher
            .dispatch(NativeRequest {
                request_id: "status".to_owned(),
                command: NativeCommand::Status { job_id },
            })
            .body
        {
            NativeResponseBody::JobStatus { job } => job,
            other => panic!("expected job status, received {other:?}"),
        }
    }

    fn wait_for_terminal_status(dispatcher: &HostDispatcher, job_id: JobId) -> JobStatus {
        for _ in 0..100 {
            let status = status_for(dispatcher, job_id.clone());
            if status.state.is_terminal() {
                return status;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("job did not reach a terminal state in time");
    }

    fn wait_for_processing_status(dispatcher: &HostDispatcher, job_id: JobId) -> JobStatus {
        for _ in 0..100 {
            let status = status_for(dispatcher, job_id.clone());
            if status.state == JobState::Processing {
                return status;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("job did not reach processing state in time");
    }

    fn wait_for_flag(flag: &AtomicBool, description: &str) {
        for _ in 0..100 {
            if flag.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("{description} did not occur in time");
    }

    #[test]
    fn dispatches_handshake_start_status_and_completion() {
        let dispatcher = capable_dispatcher(SuccessfulRunner);
        let handshake = dispatcher.dispatch(NativeRequest {
            request_id: "one".to_owned(),
            command: NativeCommand::Handshake {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                extension_version: Some("0.1.0".to_owned()),
            },
        });
        assert!(matches!(
            handshake.body,
            NativeResponseBody::Handshake { ref native_host_name, .. }
                if native_host_name == NATIVE_HOST_NAME
        ));

        let started = dispatcher.dispatch(NativeRequest {
            request_id: "two".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        let status = wait_for_terminal_status(&dispatcher, job_id);
        assert_eq!(status.state, JobState::Completed);
        assert_eq!(status.progress.percent(), Some(100));
    }

    #[test]
    fn supported_youtube_page_is_preflighted_as_local_direct_media() {
        let dispatcher = capable_dispatcher(SuccessfulRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "youtube-page".to_owned(),
            command: NativeCommand::Start {
                job: youtube_page_job(),
            },
        });
        match started.body {
            NativeResponseBody::JobStarted { acquisition, .. } => {
                assert_eq!(acquisition.strategy, AcquisitionStrategy::DirectMedia);
                assert!(acquisition.summary.contains("YouTube"));
            }
            other => panic!("expected a YouTube page start response, received {other:?}"),
        }
    }

    #[test]
    fn handshake_exposes_only_the_coarse_local_processing_advisory_not_runtime_details() {
        let advisory = LocalModelSelection {
            model: LocalModel::Small,
            quantization: Quantization::Q5Km,
            backend: ComputeBackend::Cpu,
            source: LocalModelSelectionSource::Automatic,
            local_performance: LocalPerformance::Good,
        }
        .advisory();
        let dispatcher = capable_dispatcher_with_advisory(SuccessfulRunner, Some(advisory));
        let response = dispatcher.dispatch(NativeRequest {
            request_id: "hardware-plan".to_owned(),
            command: NativeCommand::Handshake {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                extension_version: None,
            },
        });
        let serialized = serde_json::to_value(response).unwrap();
        assert_eq!(
            serialized["capabilities"]["local_processing_advisory"],
            serde_json::json!({
                "selection_source": "automatic",
                "model": "small",
                "quantization": "q5_k_m",
                "backend": "cpu",
                "local_performance": "good",
            })
        );
        assert!(serialized.to_string().contains("small"));
        assert!(serialized.get("available_memory_mb").is_none());
        assert!(!serialized.to_string().contains("model_path"));
    }

    #[test]
    fn completed_job_returns_private_bounded_subtitle_cue_pages() {
        let dispatcher = capable_dispatcher(SuccessfulRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id.clone()).state,
            JobState::Completed
        );

        let first_page = dispatcher.dispatch(NativeRequest {
            request_id: "cues-one".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id: job_id.clone(),
                cursor: None,
                limit: Some(1),
            },
        });
        let serialized = serde_json::to_string(&first_page).unwrap();
        assert!(serialized.len() <= MAX_SUBTITLE_CUE_PAGE_BYTES);
        assert!(!serialized.contains("recording.mp4"));
        assert!(!serialized.contains("C:/Subtitler/exports"));
        let next_cursor = match first_page.body {
            NativeResponseBody::SubtitleCues {
                job_id: returned_job_id,
                cues,
                next_cursor,
            } => {
                assert_eq!(returned_job_id, job_id);
                assert_eq!(cues.len(), 1);
                assert_eq!(cues[0].text(), "First generated subtitle.");
                next_cursor.expect("a second cue should produce a next cursor")
            }
            other => panic!("expected subtitle_cues response, received {other:?}"),
        };

        let second_page = dispatcher.dispatch(NativeRequest {
            request_id: "cues-two".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id,
                cursor: Some(next_cursor),
                limit: Some(10),
            },
        });
        assert!(matches!(
            second_page.body,
            NativeResponseBody::SubtitleCues { cues, next_cursor: None, .. }
                if cues.len() == 1 && cues[0].text() == "Second generated subtitle."
        ));
    }

    #[test]
    fn completed_job_returns_private_canonical_transcript_pages() {
        let dispatcher = capable_dispatcher(SuccessfulRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-transcript".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id.clone()).state,
            JobState::Completed
        );

        let first_page = dispatcher.dispatch(NativeRequest {
            request_id: "transcript-one".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: job_id.clone(),
                cursor: None,
                limit: Some(1),
            },
        });
        let serialized = serde_json::to_string(&first_page).unwrap();
        assert!(serialized.len() <= MAX_TRANSCRIPT_SEGMENT_PAGE_BYTES);
        assert!(serialized.len() < 128 * 1024);
        assert!(!serialized.contains("recording.mp4"));
        assert!(!serialized.contains("C:/Subtitler/exports"));
        assert!(!serialized.contains("internal-word-timestamp"));
        assert!(!serialized.contains("internal-word-speaker"));
        assert!(!serialized.contains("translated_from"));
        assert!(!serialized.contains("\"language\""));
        let next_cursor = match first_page.body {
            NativeResponseBody::TranscriptSegments {
                job_id: returned_job_id,
                segments,
                next_cursor,
            } => {
                assert_eq!(returned_job_id, job_id);
                assert_eq!(segments.len(), 1);
                // The test runner returns reverse input order; the completed
                // outcome must instead expose stable media-time ordering.
                assert_eq!(segments[0].timing.start_ms, 0);
                assert_eq!(segments[0].timing.end_ms, 1_000);
                assert_eq!(segments[0].text, "First completed transcript segment.");
                assert_eq!(segments[0].speaker.as_deref(), Some("Speaker 1"));
                next_cursor.expect("a second segment should produce a next cursor")
            }
            other => panic!("expected transcript_segments response, received {other:?}"),
        };

        let second_page = dispatcher.dispatch(NativeRequest {
            request_id: "transcript-two".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id,
                cursor: Some(next_cursor),
                limit: Some(10),
            },
        });
        assert!(matches!(
            second_page.body,
            NativeResponseBody::TranscriptSegments { segments, next_cursor: None, .. }
                if segments.len() == 1
                    && segments[0].timing.start_ms == 1_200
                    && segments[0].text == "Second completed transcript segment."
                    && segments[0].speaker.as_deref() == Some("Speaker 2")
        ));
    }

    #[test]
    fn transcript_pages_require_a_completed_known_job_and_handle_empty_results() {
        let blocking_dispatcher = capable_dispatcher(BlockingRunner);
        let started = blocking_dispatcher.dispatch(NativeRequest {
            request_id: "start-pending-transcript".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let blocking_job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        wait_for_processing_status(&blocking_dispatcher, blocking_job_id.clone());
        let pending = blocking_dispatcher.dispatch(NativeRequest {
            request_id: "pending-transcript".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: blocking_job_id.clone(),
                cursor: None,
                limit: None,
            },
        });
        assert!(matches!(
            pending.body,
            NativeResponseBody::Error {
                code: ProtocolErrorCode::InvalidState,
                retryable: true,
                ..
            }
        ));
        let _ = blocking_dispatcher.dispatch(NativeRequest {
            request_id: "cancel-pending-transcript".to_owned(),
            command: NativeCommand::Cancel {
                job_id: blocking_job_id.clone(),
            },
        });
        assert_eq!(
            wait_for_terminal_status(&blocking_dispatcher, blocking_job_id).state,
            JobState::Cancelled
        );

        let empty_dispatcher = capable_dispatcher(EmptyCueRunner);
        let started = empty_dispatcher.dispatch(NativeRequest {
            request_id: "start-empty-transcript".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let empty_job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&empty_dispatcher, empty_job_id.clone()).state,
            JobState::Completed
        );
        let empty_page = empty_dispatcher.dispatch(NativeRequest {
            request_id: "empty-transcript".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: empty_job_id.clone(),
                cursor: None,
                limit: None,
            },
        });
        assert!(matches!(
            empty_page.body,
            NativeResponseBody::TranscriptSegments { segments, next_cursor: None, .. }
                if segments.is_empty()
        ));

        let bad_cursor = empty_dispatcher.dispatch(NativeRequest {
            request_id: "bad-empty-transcript-cursor".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: empty_job_id,
                cursor: Some(1),
                limit: None,
            },
        });
        assert!(matches!(
            bad_cursor.body,
            NativeResponseBody::Error {
                code: ProtocolErrorCode::InvalidRequest,
                retryable: false,
                ..
            }
        ));

        let unknown_job = empty_dispatcher.dispatch(NativeRequest {
            request_id: "unknown-transcript-job".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: JobId::new(),
                cursor: None,
                limit: None,
            },
        });
        assert!(matches!(
            unknown_job.body,
            NativeResponseBody::Error {
                code: ProtocolErrorCode::UnknownJob,
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn transcript_pages_clamp_count_and_bytes_and_report_oversized_segments() {
        let dispatcher = capable_dispatcher(ManyTranscriptRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-many-transcript".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id.clone()).state,
            JobState::Completed
        );
        let count_clamped = dispatcher.dispatch(NativeRequest {
            request_id: "many-transcript-count".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: job_id.clone(),
                cursor: None,
                limit: Some(u16::MAX),
            },
        });
        assert!(
            serde_json::to_vec(&count_clamped).unwrap().len() <= MAX_TRANSCRIPT_SEGMENT_PAGE_BYTES
        );
        let next_cursor = match count_clamped.body {
            NativeResponseBody::TranscriptSegments {
                segments,
                next_cursor,
                ..
            } => {
                assert_eq!(segments.len(), MAX_TRANSCRIPT_SEGMENTS_PER_PAGE);
                assert_eq!(segments[0].timing.start_ms, 0);
                next_cursor.expect("remaining segments need a cursor")
            }
            other => panic!("expected transcript_segments response, received {other:?}"),
        };

        let minimum_limit = dispatcher.dispatch(NativeRequest {
            request_id: "many-transcript-minimum-limit".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id,
                cursor: Some(next_cursor),
                limit: Some(0),
            },
        });
        assert!(matches!(
            minimum_limit.body,
            NativeResponseBody::TranscriptSegments { segments, next_cursor: Some(101), .. }
                if segments.len() == 1 && segments[0].timing.start_ms == 100_000
        ));

        let byte_limited = (0..MAX_TRANSCRIPT_SEGMENTS_PER_PAGE)
            .map(|index| {
                test_transcript_segment(index as u64, index as u64 + 1, "x".repeat(2 * 1_024), None)
            })
            .collect::<Vec<_>>();
        let byte_page = build_transcript_segment_page(
            Some("byte-limited".to_owned()),
            JobId::new(),
            &byte_limited,
            0,
            MAX_TRANSCRIPT_SEGMENTS_PER_PAGE,
        )
        .unwrap();
        let byte_response = NativeResponse {
            request_id: Some("byte-limited".to_owned()),
            body: byte_page,
        };
        assert!(
            serde_json::to_vec(&byte_response).unwrap().len() <= MAX_TRANSCRIPT_SEGMENT_PAGE_BYTES
        );
        assert!(matches!(
            byte_response.body,
            NativeResponseBody::TranscriptSegments { segments, next_cursor: Some(_), .. }
                if segments.len() < MAX_TRANSCRIPT_SEGMENTS_PER_PAGE
        ));

        let oversized = vec![test_transcript_segment(
            0,
            1_000,
            "x".repeat(MAX_TRANSCRIPT_SEGMENT_TEXT_BYTES + 1),
            None,
        )];
        assert_eq!(
            build_transcript_segment_page(
                Some("oversized".to_owned()),
                JobId::new(),
                &oversized,
                0,
                1
            )
            .unwrap_err(),
            TranscriptPageError::SingleSegmentTooLarge
        );

        let oversized_dispatcher = capable_dispatcher(OversizedTranscriptRunner);
        let started = oversized_dispatcher.dispatch(NativeRequest {
            request_id: "start-oversized-transcript".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let oversized_job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&oversized_dispatcher, oversized_job_id.clone()).state,
            JobState::Completed
        );
        let oversized_response = oversized_dispatcher.dispatch(NativeRequest {
            request_id: "oversized-transcript-page".to_owned(),
            command: NativeCommand::GetTranscriptSegments {
                job_id: oversized_job_id,
                cursor: None,
                limit: None,
            },
        });
        assert!(matches!(
            oversized_response.body,
            NativeResponseBody::Error {
                code: ProtocolErrorCode::ResultTooLarge,
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn subtitle_pages_clamp_cue_count_and_reject_an_oversized_single_cue() {
        let dispatcher = capable_dispatcher(ManyCueRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-many".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id.clone()).state,
            JobState::Completed
        );
        let page = dispatcher.dispatch(NativeRequest {
            request_id: "many-cues".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id,
                cursor: None,
                limit: Some(u16::MAX),
            },
        });
        let page_bytes = serde_json::to_vec(&page).unwrap().len();
        assert!(page_bytes <= MAX_SUBTITLE_CUE_PAGE_BYTES);
        assert!(matches!(
            page.body,
            NativeResponseBody::SubtitleCues { cues, next_cursor: Some(_), .. }
                if cues.len() == MAX_SUBTITLE_CUES_PER_PAGE
        ));

        let oversized = vec![test_cue(0, 1_000, "x".repeat(MAX_SUBTITLE_CUE_PAGE_BYTES))];
        assert_eq!(
            build_subtitle_cue_page(Some("oversized".to_owned()), JobId::new(), &oversized, 0, 1,)
                .unwrap_err(),
            CuePageError::SingleCueTooLarge
        );
    }

    #[test]
    fn cue_endpoint_handles_empty_results_and_rejects_out_of_range_cursors() {
        let dispatcher = capable_dispatcher(EmptyCueRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-empty".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id.clone()).state,
            JobState::Completed
        );

        let empty_page = dispatcher.dispatch(NativeRequest {
            request_id: "empty-cues".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id: job_id.clone(),
                cursor: None,
                limit: None,
            },
        });
        assert!(matches!(
            empty_page.body,
            NativeResponseBody::SubtitleCues { cues, next_cursor: None, .. } if cues.is_empty()
        ));

        let out_of_range = dispatcher.dispatch(NativeRequest {
            request_id: "bad-cursor".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id,
                cursor: Some(1),
                limit: None,
            },
        });
        assert!(matches!(
            out_of_range.body,
            NativeResponseBody::Error {
                code: ProtocolErrorCode::InvalidRequest,
                ..
            }
        ));
    }

    #[test]
    fn durationless_subtitle_jobs_return_an_empty_non_terminal_cue_page() {
        let dispatcher = capable_dispatcher(BlockingRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-durationless-subtitles".to_owned(),
            command: NativeCommand::Start {
                job: durationless_subtitle_job(),
            },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        wait_for_processing_status(&dispatcher, job_id.clone());

        let page = dispatcher.dispatch(NativeRequest {
            request_id: "durationless-subtitle-cues".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id: job_id.clone(),
                cursor: None,
                limit: None,
            },
        });
        assert!(matches!(
            page.body,
            NativeResponseBody::SubtitleCues {
                cues,
                next_cursor: None,
                ..
            } if cues.is_empty()
        ));

        let _ = dispatcher.dispatch(NativeRequest {
            request_id: "cancel-durationless-subtitles".to_owned(),
            command: NativeCommand::Cancel {
                job_id: job_id.clone(),
            },
        });
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id).state,
            JobState::Cancelled
        );
    }

    #[test]
    fn playback_updates_drive_the_subtitle_scheduler_but_do_not_affect_full_transcripts() {
        let dispatcher = capable_dispatcher(BlockingRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-subtitles".to_owned(),
            command: NativeCommand::Start {
                job: subtitle_job(),
            },
        });
        let subtitle_job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        wait_for_processing_status(&dispatcher, subtitle_job_id.clone());

        let playback_status = dispatcher.dispatch(NativeRequest {
            request_id: "playback".to_owned(),
            command: NativeCommand::PlaybackUpdate {
                job_id: subtitle_job_id.clone(),
                position_ms: 15_000,
                playback_rate_milli: 1_000,
                is_paused: false,
                seek_generation: 0,
            },
        });
        assert!(matches!(
            playback_status.body,
            NativeResponseBody::JobStatus { ref job }
                if job.state == JobState::Processing
                    && job.progress.subtitle_buffer_ahead_ms == Some(0)
                    && job.message.as_deref().is_some_and(|message| message.contains("subtitle"))
        ));

        let seek_status = dispatcher.dispatch(NativeRequest {
            request_id: "seek".to_owned(),
            command: NativeCommand::PlaybackUpdate {
                job_id: subtitle_job_id.clone(),
                position_ms: 95_000,
                playback_rate_milli: 1_250,
                is_paused: false,
                seek_generation: 1,
            },
        });
        assert!(matches!(
            seek_status.body,
            NativeResponseBody::JobStatus { ref job }
                if job.state == JobState::Processing && job.progress.subtitle_buffer_ahead_ms == Some(0)
        ));
        let runtime = dispatcher
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&subtitle_job_id)
            .and_then(|job| job.subtitle_runtime.clone())
            .expect("subtitle job should own a scheduler runtime");
        let scheduler = runtime.scheduler.lock().unwrap();
        assert_eq!(scheduler.playback().position_ms, 95_000);
        assert_eq!(scheduler.playback().seek_generation, 1);
        assert_eq!(scheduler.playback().playback_rate, 1.25);
        drop(scheduler);

        let cancelled = dispatcher.dispatch(NativeRequest {
            request_id: "cancel-subtitles".to_owned(),
            command: NativeCommand::Cancel {
                job_id: subtitle_job_id.clone(),
            },
        });
        assert!(matches!(
            cancelled.body,
            NativeResponseBody::JobCancelled { .. }
        ));
        assert_eq!(
            wait_for_terminal_status(&dispatcher, subtitle_job_id).state,
            JobState::Cancelled
        );

        let full_started = dispatcher.dispatch(NativeRequest {
            request_id: "start-full".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let full_job_id = match full_started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        let full_status = dispatcher.dispatch(NativeRequest {
            request_id: "full-playback".to_owned(),
            command: NativeCommand::PlaybackUpdate {
                job_id: full_job_id.clone(),
                position_ms: 30_000,
                playback_rate_milli: 1_000,
                is_paused: false,
                seek_generation: 7,
            },
        });
        assert!(matches!(
            full_status.body,
            NativeResponseBody::JobStatus { ref job } if job.kind == JobKind::FullTranscript
        ));
        let _ = dispatcher.dispatch(NativeRequest {
            request_id: "cancel-full".to_owned(),
            command: NativeCommand::Cancel {
                job_id: full_job_id.clone(),
            },
        });
        assert_eq!(
            wait_for_terminal_status(&dispatcher, full_job_id).state,
            JobState::Cancelled
        );
    }

    #[test]
    fn playback_update_rejects_rates_outside_the_extension_contract() {
        let dispatcher = capable_dispatcher(BlockingRunner);

        for playback_rate_milli in [0, 249, 4_001] {
            let response = dispatcher.dispatch(NativeRequest {
                request_id: format!("invalid-rate-{playback_rate_milli}"),
                command: NativeCommand::PlaybackUpdate {
                    job_id: JobId::new(),
                    position_ms: 0,
                    playback_rate_milli,
                    is_paused: false,
                    seek_generation: 0,
                },
            });
            assert!(matches!(
                response.body,
                NativeResponseBody::Error {
                    code: ProtocolErrorCode::InvalidRequest,
                    ..
                }
            ));
        }
    }

    #[test]
    fn seek_preempts_the_active_chunk_and_partial_cues_are_pageable_while_processing() {
        let active = Arc::new(AtomicBool::new(false));
        let preempted = Arc::new(AtomicBool::new(false));
        let dispatcher = capable_dispatcher(SeekAwareRunner {
            active: Arc::clone(&active),
            preempted: Arc::clone(&preempted),
        });
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-seek-aware".to_owned(),
            command: NativeCommand::Start {
                job: subtitle_job(),
            },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        wait_for_processing_status(&dispatcher, job_id.clone());
        wait_for_flag(&active, "active subtitle chunk");

        let partial_page = dispatcher.dispatch(NativeRequest {
            request_id: "partial-cues".to_owned(),
            command: NativeCommand::GetSubtitleCues {
                job_id: job_id.clone(),
                cursor: None,
                limit: Some(10),
            },
        });
        assert!(matches!(
            partial_page.body,
            NativeResponseBody::SubtitleCues { ref cues, next_cursor: None, .. }
                if cues.len() == 1 && cues[0].text() == "Partial generated subtitle."
        ));
        assert!(serde_json::to_vec(&partial_page).unwrap().len() <= MAX_SUBTITLE_CUE_PAGE_BYTES);

        let seek_response = dispatcher.dispatch(NativeRequest {
            request_id: "preempt".to_owned(),
            command: NativeCommand::PlaybackUpdate {
                job_id: job_id.clone(),
                position_ms: 90_000,
                playback_rate_milli: 1_000,
                is_paused: false,
                seek_generation: 1,
            },
        });
        assert!(matches!(
            seek_response.body,
            NativeResponseBody::JobStatus { ref job } if job.state == JobState::Processing
        ));
        wait_for_flag(&preempted, "seek preemption");

        let _ = dispatcher.dispatch(NativeRequest {
            request_id: "cancel-seek-aware".to_owned(),
            command: NativeCommand::Cancel {
                job_id: job_id.clone(),
            },
        });
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id).state,
            JobState::Cancelled
        );
    }

    #[test]
    fn chunk_local_asr_timestamps_are_offset_and_bounded_to_the_source_range() {
        let chunk = Transcript {
            language: "en".to_owned(),
            translated_from: None,
            segments: vec![TranscriptSegment {
                timing: TimeRange::new(29_500, 30_500).unwrap(),
                text: "Boundary word.".to_owned(),
                speaker: None,
                words: vec![WordTimestamp {
                    text: "Boundary".to_owned(),
                    timing: TimeRange::new(29_500, 30_500).unwrap(),
                    speaker: None,
                }],
            }],
        };
        let shifted = offset_chunk_transcript(chunk, 30_000, 60_000).unwrap();
        assert_eq!(shifted.segments[0].timing.start_ms, 59_500);
        assert_eq!(shifted.segments[0].timing.end_ms, 60_000);
        assert_eq!(shifted.segments[0].words[0].timing.start_ms, 59_500);
        assert_eq!(shifted.segments[0].words[0].timing.end_ms, 60_000);
    }

    #[test]
    fn terminal_jobs_cannot_be_cancelled_after_completion() {
        let dispatcher = capable_dispatcher(SuccessfulRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start-completed".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id.clone()).state,
            JobState::Completed
        );

        let response = dispatcher.dispatch(NativeRequest {
            request_id: "cancel-completed".to_owned(),
            command: NativeCommand::Cancel { job_id },
        });
        assert!(matches!(
            response.body,
            NativeResponseBody::Error {
                code: ProtocolErrorCode::InvalidState,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_reaches_the_runner_and_prevents_completion() {
        let dispatcher = capable_dispatcher(BlockingRunner);
        let started = dispatcher.dispatch(NativeRequest {
            request_id: "start".to_owned(),
            command: NativeCommand::Start { job: direct_job() },
        });
        let job_id = match started.body {
            NativeResponseBody::JobStarted { job, .. } => job.job_id,
            other => panic!("expected job_started response, received {other:?}"),
        };

        let cancelled = dispatcher.dispatch(NativeRequest {
            request_id: "cancel".to_owned(),
            command: NativeCommand::Cancel {
                job_id: job_id.clone(),
            },
        });
        assert!(matches!(
            cancelled.body,
            NativeResponseBody::JobCancelled { job } if job.state == JobState::Cancelled
        ));
        assert_eq!(
            wait_for_terminal_status(&dispatcher, job_id).state,
            JobState::Cancelled
        );
    }

    #[test]
    fn framing_round_trip_uses_little_endian_length_prefix() {
        let response = NativeResponse::error(
            Some("request-1".to_owned()),
            ProtocolErrorCode::InvalidRequest,
            "Invalid request.",
            false,
        );
        let mut bytes = Vec::new();
        write_native_message(&mut bytes, &response).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
            bytes.len() - 4
        );

        let payload = read_native_message(&mut bytes.as_slice()).unwrap().unwrap();
        let decoded: NativeResponse = serde_json::from_slice(&payload).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn event_loop_processes_a_framed_handshake() {
        let request = NativeRequest {
            request_id: "handshake-request".to_owned(),
            command: NativeCommand::Handshake {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                extension_version: Some("0.1.0".to_owned()),
            },
        };
        let mut input = Vec::new();
        write_native_message(&mut input, &request).unwrap();
        let mut output = Vec::new();

        run_native_host(&mut input.as_slice(), &mut output, &HostDispatcher::new()).unwrap();

        let payload = read_native_message(&mut output.as_slice())
            .unwrap()
            .unwrap();
        let response: NativeResponse = serde_json::from_slice(&payload).unwrap();
        assert!(matches!(
            response.body,
            NativeResponseBody::Handshake { protocol_version, .. }
                if protocol_version == NATIVE_PROTOCOL_VERSION
        ));
    }
}
