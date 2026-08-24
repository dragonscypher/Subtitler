use crate::MediaError;
use std::{
    ffi::OsString,
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::{Builder as TempDirBuilder, TempDir};
use thiserror::Error;
use url::Url;

pub const CANONICAL_SAMPLE_RATE_HZ: u32 = 16_000;
pub const CANONICAL_CHANNELS: u16 = 1;
/// The largest individual audio window the media layer will ask FFmpeg to
/// decode. Ahead-of-playhead scheduling can combine adjacent windows, but an
/// individual request must remain bounded so a seek cannot unexpectedly fill
/// the temporary cache with an entire recording.
pub const MAX_AUDIO_EXTRACTION_RANGE_MS: u64 = 15 * 60 * 1_000;
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;

/// A validated, half-open media interval in milliseconds: `[start_ms, end_ms)`.
///
/// Range construction is deliberately separate from the FFmpeg invocation.
/// The decoder receives only trusted numeric bounds, never page-provided
/// command-line fragments or arbitrary FFmpeg options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioExtractionRange {
    start_ms: u64,
    end_ms: u64,
}

impl AudioExtractionRange {
    pub fn new(start_ms: u64, end_ms: u64) -> Result<Self, AudioExtractionRangeError> {
        let duration_ms = end_ms
            .checked_sub(start_ms)
            .ok_or(AudioExtractionRangeError::InvalidBounds { start_ms, end_ms })?;
        if duration_ms == 0 {
            return Err(AudioExtractionRangeError::InvalidBounds { start_ms, end_ms });
        }
        if duration_ms > MAX_AUDIO_EXTRACTION_RANGE_MS {
            return Err(AudioExtractionRangeError::ExceedsMaximumDuration {
                duration_ms,
                maximum_ms: MAX_AUDIO_EXTRACTION_RANGE_MS,
            });
        }

        Ok(Self { start_ms, end_ms })
    }

    pub fn start_ms(self) -> u64 {
        self.start_ms
    }

    pub fn end_ms(self) -> u64 {
        self.end_ms
    }

    pub fn duration_ms(self) -> u64 {
        // `new` guarantees this subtraction cannot underflow.
        self.end_ms - self.start_ms
    }

    fn ffmpeg_start_timestamp(self) -> String {
        format_ffmpeg_timestamp(self.start_ms)
    }

    fn ffmpeg_duration_timestamp(self) -> String {
        format_ffmpeg_timestamp(self.duration_ms())
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum AudioExtractionRangeError {
    #[error("audio extraction range must have an end after its start")]
    InvalidBounds { start_ms: u64, end_ms: u64 },
    #[error(
        "audio extraction range is {duration_ms} ms, exceeding the {maximum_ms} ms per-window limit"
    )]
    ExceedsMaximumDuration { duration_ms: u64, maximum_ms: u64 },
}

fn format_ffmpeg_timestamp(milliseconds: u64) -> String {
    format!("{}.{:03}", milliseconds / 1_000, milliseconds % 1_000)
}

/// Input passed to a decoder after media policy validation. It is intentionally
/// not serializable, which helps prevent sensitive signed URLs from entering
/// normal job status or logs.
///
/// `RemoteUrl` exists only as a pre-acquisition marker for callers that still
/// model a direct source. The FFmpeg boundary rejects it; callers must first
/// use `RemoteMediaAcquirer` and pass the resulting `LocalPath`.
#[derive(Clone, PartialEq, Eq)]
pub enum AudioInput {
    RemoteUrl(Url),
    LocalPath(PathBuf),
}

impl AudioInput {
    fn local_path(&self) -> Result<&Path, FfmpegExtractionError> {
        match self {
            Self::RemoteUrl(_) => Err(FfmpegExtractionError::RemoteInputRequiresAcquisition),
            Self::LocalPath(path) => Ok(path),
        }
    }
}

impl fmt::Debug for AudioInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteUrl(_) => formatter.write_str("AudioInput::RemoteUrl(<redacted>)"),
            Self::LocalPath(_) => formatter.write_str("AudioInput::LocalPath(<redacted>)"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub const CANONICAL: Self = Self {
        sample_rate_hz: CANONICAL_SAMPLE_RATE_HZ,
        channels: CANONICAL_CHANNELS,
    };
}

/// Interleaved normalized PCM samples. In production, FFmpeg/native decoding
/// is expected to produce the canonical mono, 16 kHz f32 stream.
#[derive(Clone, Debug, PartialEq)]
pub struct PcmChunk {
    pub start_ms: u64,
    pub format: AudioFormat,
    pub samples: Vec<f32>,
}

impl PcmChunk {
    pub fn validate(&self) -> Result<(), AudioPipelineError> {
        if self.format.sample_rate_hz == 0 || self.format.channels == 0 {
            return Err(AudioPipelineError::InvalidFormat);
        }
        if self.samples.len() % usize::from(self.format.channels) != 0 {
            return Err(AudioPipelineError::MisalignedSamples);
        }
        if self.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(AudioPipelineError::NonFiniteSample);
        }
        Ok(())
    }

    pub fn duration_ms(&self) -> u64 {
        let frames = self.samples.len() / usize::from(self.format.channels.max(1));
        (frames as u64).saturating_mul(1_000) / u64::from(self.format.sample_rate_hz.max(1))
    }
}

/// Pull-based decoded-audio stream. Pulling avoids decoding full video when a
/// subtitle job only needs audio around and ahead of the current playhead.
pub trait AudioStream: Send {
    fn next_chunk(&mut self) -> Result<Option<PcmChunk>, AudioPipelineError>;
}

/// A decoder implementation owns media decoding but returns audio only.
pub trait AudioDecoder: Send + Sync {
    fn open(&self, input: AudioInput) -> Result<Box<dyn AudioStream>, MediaError>;
}

/// Process-independent FFmpeg command plan. It uses an argument vector rather
/// than a shell string, eliminating shell-command injection at this boundary.
#[derive(Clone)]
pub struct FfmpegCommandPlan {
    program: PathBuf,
    arguments: Vec<OsString>,
}

impl FfmpegCommandPlan {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        command
    }
}

impl fmt::Debug for FfmpegCommandPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FfmpegCommandPlan")
            .field("program", &self.program)
            .field("arguments", &"<redacted media input>")
            .finish()
    }
}

/// A complete FFmpeg extraction plan. The media input and private temporary
/// output path are intentionally redacted from its `Debug` representation.
#[derive(Clone)]
pub struct FfmpegExtractionPlan {
    program: PathBuf,
    arguments: Vec<OsString>,
    output_path: PathBuf,
}

impl FfmpegExtractionPlan {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        command
    }
}

impl fmt::Debug for FfmpegExtractionPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FfmpegExtractionPlan")
            .field("program", &self.program)
            .field("arguments", &"<redacted media input and temporary output>")
            .finish()
    }
}

/// A cloneable cancellation signal shared between a scheduler and a local
/// FFmpeg invocation. Dropping the returned artifact never retains decoded
/// audio in a long-lived cache.
#[derive(Clone, Debug, Default)]
pub struct ExtractionCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ExtractionCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Bounds one FFmpeg invocation. The cap protects the temporary cache from a
/// malformed source returning an unexpectedly large decoded stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfmpegExtractionOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub max_output_bytes: u64,
    /// Optional private engine cache root. `None` delegates to the OS
    /// user-temporary directory through `tempfile`.
    pub temporary_root: Option<PathBuf>,
}

impl Default for FfmpegExtractionOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2 * 60 * 60),
            poll_interval: Duration::from_millis(50),
            max_output_bytes: 4 * 1024 * 1024 * 1024,
            temporary_root: None,
        }
    }
}

/// Owns one canonical WAV artifact. Its unique private directory is removed
/// on `Drop` unless a caller explicitly copies an export elsewhere.
pub struct NormalizedAudioArtifact {
    directory: TempDir,
    path: PathBuf,
}

impl NormalizedAudioArtifact {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> AudioFormat {
        AudioFormat::CANONICAL
    }

    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

impl fmt::Debug for NormalizedAudioArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NormalizedAudioArtifact")
            .field("path", &"<private temporary audio>")
            .field("format", &AudioFormat::CANONICAL)
            .finish()
    }
}

/// A result from an FFmpeg command runner. Diagnostics are captured only for
/// bounded local troubleshooting and are never returned in user-facing error
/// strings, which may otherwise expose signed media URLs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfmpegCommandOutput {
    pub succeeded: bool,
    pub exit_code: Option<i32>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum FfmpegRunnerError {
    #[error("FFmpeg could not be started")]
    Spawn,
    #[error("FFmpeg process I/O failed")]
    Io,
    #[error("FFmpeg timed out")]
    TimedOut,
    #[error("FFmpeg was cancelled")]
    Cancelled,
}

/// Injectable command execution boundary. Tests never need a real FFmpeg
/// installation; the production runner still uses a direct argument vector.
pub trait FfmpegCommandRunner: Send + Sync {
    fn run(
        &self,
        plan: &FfmpegExtractionPlan,
        cancellation: &ExtractionCancellation,
        options: &FfmpegExtractionOptions,
    ) -> Result<FfmpegCommandOutput, FfmpegRunnerError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemFfmpegCommandRunner;

impl FfmpegCommandRunner for SystemFfmpegCommandRunner {
    fn run(
        &self,
        plan: &FfmpegExtractionPlan,
        cancellation: &ExtractionCancellation,
        options: &FfmpegExtractionOptions,
    ) -> Result<FfmpegCommandOutput, FfmpegRunnerError> {
        if cancellation.is_cancelled() {
            return Err(FfmpegRunnerError::Cancelled);
        }
        if options.timeout.is_zero() {
            return Err(FfmpegRunnerError::TimedOut);
        }

        let mut command = plan.to_command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| FfmpegRunnerError::Spawn)?;
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate_child(&mut child);
                return Err(FfmpegRunnerError::Io);
            }
        };
        let stderr_reader = thread::spawn(move || drain_limited(stderr));
        let started = Instant::now();
        let poll_interval = options.poll_interval.max(Duration::from_millis(10));

        let status = loop {
            if cancellation.is_cancelled() {
                terminate_child(&mut child);
                let _ = join_reader(stderr_reader);
                return Err(FfmpegRunnerError::Cancelled);
            }
            if started.elapsed() >= options.timeout {
                terminate_child(&mut child);
                let _ = join_reader(stderr_reader);
                return Err(FfmpegRunnerError::TimedOut);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(poll_interval),
                Err(_) => {
                    terminate_child(&mut child);
                    let _ = join_reader(stderr_reader);
                    return Err(FfmpegRunnerError::Io);
                }
            }
        };

        Ok(FfmpegCommandOutput {
            succeeded: status.success(),
            exit_code: status.code(),
            stderr: join_reader(stderr_reader)?,
        })
    }
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn join_reader(
    handle: thread::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, FfmpegRunnerError> {
    handle
        .join()
        .map_err(|_| FfmpegRunnerError::Io)?
        .map_err(|_| FfmpegRunnerError::Io)
}

fn drain_limited(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut captured = Vec::with_capacity(MAX_DIAGNOSTIC_BYTES);
    let mut buffer = [0_u8; 4_096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(captured)
}

/// FFmpeg-backed audio extraction. `R` is generic only so integration tests
/// can exercise full artifact lifecycle without launching a process.
pub struct FfmpegDecoder<R = SystemFfmpegCommandRunner> {
    program: PathBuf,
    runner: R,
    options: FfmpegExtractionOptions,
}

impl Default for FfmpegDecoder<SystemFfmpegCommandRunner> {
    fn default() -> Self {
        Self::new("ffmpeg")
    }
}

impl FfmpegDecoder<SystemFfmpegCommandRunner> {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            runner: SystemFfmpegCommandRunner,
            options: FfmpegExtractionOptions::default(),
        }
    }

    pub fn with_options(program: impl Into<PathBuf>, options: FfmpegExtractionOptions) -> Self {
        Self {
            program: program.into(),
            runner: SystemFfmpegCommandRunner,
            options,
        }
    }
}

impl<R> FfmpegDecoder<R> {
    pub fn with_runner(
        program: impl Into<PathBuf>,
        runner: R,
        options: FfmpegExtractionOptions,
    ) -> Self {
        Self {
            program: program.into(),
            runner,
            options,
        }
    }

    pub fn options(&self) -> &FfmpegExtractionOptions {
        &self.options
    }

    pub fn command_plan(
        &self,
        input: &AudioInput,
    ) -> Result<FfmpegCommandPlan, FfmpegExtractionError> {
        let input = input.local_path()?;
        let arguments = vec![
            OsString::from("-nostdin"),
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            // The remote acquisition boundary always materializes a local
            // file. Keep FFmpeg from following nested playlist/manifest URLs
            // even if hostile bytes are presented as a media container.
            OsString::from("-protocol_whitelist"),
            OsString::from("file"),
            OsString::from("-i"),
            input.as_os_str().to_os_string(),
            OsString::from("-vn"),
            OsString::from("-map"),
            OsString::from("0:a:0?"),
            OsString::from("-ac"),
            OsString::from(CANONICAL_CHANNELS.to_string()),
            OsString::from("-ar"),
            OsString::from(CANONICAL_SAMPLE_RATE_HZ.to_string()),
            OsString::from("-f"),
            OsString::from("f32le"),
            OsString::from("pipe:1"),
        ];
        Ok(FfmpegCommandPlan {
            program: self.program.clone(),
            arguments,
        })
    }

    /// Build the fixed FFmpeg invocation used for whole-source file-based
    /// whisper.cpp input. The remote/local source occupies one argument
    /// position only; page data cannot add flags or invoke a shell.
    pub fn wav_command_plan(
        &self,
        input: &AudioInput,
        output_path: &Path,
    ) -> Result<FfmpegExtractionPlan, FfmpegExtractionError> {
        self.wav_command_plan_with_range(input, None, output_path)
    }

    /// Build the fixed FFmpeg invocation for a validated media window. FFmpeg
    /// receives decimal timestamps generated from numeric millisecond bounds;
    /// callers cannot supply flags, filter expressions, or a shell command.
    ///
    /// `-ss` is an input option so FFmpeg can seek before decoding. With the
    /// default accurate-seek behavior, any frames before the requested point
    /// are decoded and discarded while output begins at the requested range.
    pub fn wav_range_command_plan(
        &self,
        input: &AudioInput,
        range: AudioExtractionRange,
        output_path: &Path,
    ) -> Result<FfmpegExtractionPlan, FfmpegExtractionError> {
        self.wav_command_plan_with_range(input, Some(range), output_path)
    }

    fn wav_command_plan_with_range(
        &self,
        input: &AudioInput,
        range: Option<AudioExtractionRange>,
        output_path: &Path,
    ) -> Result<FfmpegExtractionPlan, FfmpegExtractionError> {
        let input = input.local_path()?;
        let mut arguments = vec![
            OsString::from("-nostdin"),
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
        ];
        if let Some(range) = range {
            arguments.extend([
                OsString::from("-ss"),
                OsString::from(range.ffmpeg_start_timestamp()),
            ]);
        }
        arguments.extend([
            // This is an input option, so it applies to the source and nested
            // demuxer protocols without restricting the fixed local output.
            OsString::from("-protocol_whitelist"),
            OsString::from("file"),
            OsString::from("-i"),
            input.as_os_str().to_os_string(),
        ]);
        if let Some(range) = range {
            arguments.extend([
                OsString::from("-t"),
                OsString::from(range.ffmpeg_duration_timestamp()),
            ]);
        }
        arguments.extend([
            OsString::from("-map"),
            OsString::from("0:a:0?"),
            OsString::from("-vn"),
            OsString::from("-sn"),
            OsString::from("-dn"),
            OsString::from("-ac"),
            OsString::from(CANONICAL_CHANNELS.to_string()),
            OsString::from("-ar"),
            OsString::from(CANONICAL_SAMPLE_RATE_HZ.to_string()),
            OsString::from("-c:a"),
            OsString::from("pcm_s16le"),
            OsString::from("-f"),
            OsString::from("wav"),
            OsString::from("-n"),
            output_path.as_os_str().to_os_string(),
        ]);
        Ok(FfmpegExtractionPlan {
            program: self.program.clone(),
            arguments,
            output_path: output_path.to_path_buf(),
        })
    }
}

impl<R: FfmpegCommandRunner> FfmpegDecoder<R> {
    /// Extract and normalize only the first audio stream into a private 16 kHz
    /// mono PCM WAV file. The returned artifact deletes itself when dropped.
    pub fn extract_to_wav(
        &self,
        input: &AudioInput,
        cancellation: &ExtractionCancellation,
    ) -> Result<NormalizedAudioArtifact, FfmpegExtractionError> {
        self.extract_to_wav_with_range(input, None, cancellation)
    }

    /// Extract one validated, bounded time range into a private canonical WAV
    /// artifact. This shares the same cancellation, timeout, output-size cap,
    /// validation, and automatic cleanup behavior as whole-source extraction.
    pub fn extract_range_to_wav(
        &self,
        input: &AudioInput,
        range: AudioExtractionRange,
        cancellation: &ExtractionCancellation,
    ) -> Result<NormalizedAudioArtifact, FfmpegExtractionError> {
        self.extract_to_wav_with_range(input, Some(range), cancellation)
    }

    fn extract_to_wav_with_range(
        &self,
        input: &AudioInput,
        range: Option<AudioExtractionRange>,
        cancellation: &ExtractionCancellation,
    ) -> Result<NormalizedAudioArtifact, FfmpegExtractionError> {
        if cancellation.is_cancelled() {
            return Err(FfmpegExtractionError::Cancelled);
        }
        let directory = create_private_temp_dir(self.options.temporary_root.as_deref())?;
        let output_path = directory.path().join("normalized.wav");
        let plan = match range {
            Some(range) => self.wav_range_command_plan(input, range, &output_path),
            None => self.wav_command_plan(input, &output_path),
        }?;
        let output = self
            .runner
            .run(&plan, cancellation, &self.options)
            .map_err(runner_error_to_extraction)?;
        if cancellation.is_cancelled() {
            return Err(FfmpegExtractionError::Cancelled);
        }
        if !output.succeeded {
            let status = output
                .exit_code
                .map(|code| format!("FFmpeg exited with status {code}."))
                .unwrap_or_else(|| "FFmpeg exited unsuccessfully.".to_owned());
            return Err(FfmpegExtractionError::ProcessingFailed(status));
        }
        let metadata =
            fs::metadata(&output_path).map_err(|_| FfmpegExtractionError::OutputMissing)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(FfmpegExtractionError::OutputMissing);
        }
        if metadata.len() > self.options.max_output_bytes {
            return Err(FfmpegExtractionError::OutputTooLarge {
                maximum_bytes: self.options.max_output_bytes,
            });
        }
        validate_canonical_wav(&output_path)?;
        Ok(NormalizedAudioArtifact {
            directory,
            path: output_path,
        })
    }
}

fn create_private_temp_dir(root: Option<&Path>) -> Result<TempDir, FfmpegExtractionError> {
    let mut builder = TempDirBuilder::new();
    builder.prefix("subtitler-audio-");
    match root {
        Some(root) => {
            fs::create_dir_all(root).map_err(|_| FfmpegExtractionError::TemporaryStorage)?;
            builder
                .tempdir_in(root)
                .map_err(|_| FfmpegExtractionError::TemporaryStorage)
        }
        None => builder
            .tempdir()
            .map_err(|_| FfmpegExtractionError::TemporaryStorage),
    }
}

fn runner_error_to_extraction(error: FfmpegRunnerError) -> FfmpegExtractionError {
    match error {
        FfmpegRunnerError::Spawn => FfmpegExtractionError::FfmpegUnavailable,
        FfmpegRunnerError::Io => FfmpegExtractionError::ProcessIo,
        FfmpegRunnerError::TimedOut => FfmpegExtractionError::TimedOut,
        FfmpegRunnerError::Cancelled => FfmpegExtractionError::Cancelled,
    }
}

fn validate_canonical_wav(path: &Path) -> Result<(), FfmpegExtractionError> {
    let mut header = [0_u8; 44];
    let mut file = fs::File::open(path).map_err(|_| FfmpegExtractionError::OutputMissing)?;
    file.read_exact(&mut header)
        .map_err(|_| FfmpegExtractionError::InvalidOutputFormat)?;
    let format_tag = u16::from_le_bytes([header[20], header[21]]);
    let channels = u16::from_le_bytes([header[22], header[23]]);
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let bits_per_sample = u16::from_le_bytes([header[34], header[35]]);
    if &header[..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
        || &header[12..16] != b"fmt "
        || format_tag != 1
        || channels != CANONICAL_CHANNELS
        || sample_rate != CANONICAL_SAMPLE_RATE_HZ
        || bits_per_sample != 16
    {
        return Err(FfmpegExtractionError::InvalidOutputFormat);
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FfmpegExtractionError {
    #[error(
        "A remote media URL must be acquired into a private local file before FFmpeg can run."
    )]
    RemoteInputRequiresAcquisition,
    #[error("The local FFmpeg decoder is not installed or could not be started.")]
    FfmpegUnavailable,
    #[error("The local FFmpeg decoder encountered an I/O error.")]
    ProcessIo,
    #[error("Audio extraction was cancelled.")]
    Cancelled,
    #[error("Audio extraction exceeded its allowed processing time.")]
    TimedOut,
    #[error("Subtitler could not create private temporary audio storage.")]
    TemporaryStorage,
    #[error("FFmpeg could not decode this recording's audio: {0}")]
    ProcessingFailed(String),
    #[error("FFmpeg completed without creating normalized audio.")]
    OutputMissing,
    #[error("FFmpeg did not produce canonical 16 kHz mono PCM WAV audio.")]
    InvalidOutputFormat,
    #[error(
        "The normalized audio exceeds Subtitler's temporary-cache limit ({maximum_bytes} bytes)."
    )]
    OutputTooLarge { maximum_bytes: u64 },
}

impl<R: Send + Sync> AudioDecoder for FfmpegDecoder<R> {
    fn open(&self, _input: AudioInput) -> Result<Box<dyn AudioStream>, MediaError> {
        // The production transcript path uses `extract_to_wav` so whisper.cpp
        // can consume a bounded temporary artifact without loading all audio
        // into memory. Streaming chunk decode remains a separate Phase 5 path
        // for ahead-of-playhead scheduling.
        Err(MediaError::DecoderUnavailable)
    }
}

pub trait AudioNormalizer: Send {
    fn normalize(&mut self, chunk: PcmChunk) -> Result<PcmChunk, AudioPipelineError>;
}

/// Verifies that a decoder honoured the canonical ASR format. Resampling is
/// intentionally owned by the decoder rather than hidden in this stream layer.
#[derive(Default)]
pub struct CanonicalAudioNormalizer;

impl AudioNormalizer for CanonicalAudioNormalizer {
    fn normalize(&mut self, chunk: PcmChunk) -> Result<PcmChunk, AudioPipelineError> {
        chunk.validate()?;
        if chunk.format != AudioFormat::CANONICAL {
            return Err(AudioPipelineError::UnexpectedFormat {
                actual: chunk.format,
                expected: AudioFormat::CANONICAL,
            });
        }
        Ok(chunk)
    }
}

pub trait VoiceActivityDetector: Send + Sync {
    fn contains_speech(&self, chunk: &PcmChunk) -> bool;
}

/// Lightweight energy VAD for the scheduling layer. This is deliberately only
/// a pluggable baseline; model-specific VAD can replace it without changing
/// the audio or ASR interfaces.
#[derive(Clone, Copy, Debug)]
pub struct EnergyVad {
    threshold: f32,
}

impl EnergyVad {
    pub fn new(threshold: f32) -> Result<Self, AudioPipelineError> {
        if !threshold.is_finite() || threshold < 0.0 {
            return Err(AudioPipelineError::InvalidVadThreshold);
        }
        Ok(Self { threshold })
    }
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self { threshold: 0.01 }
    }
}

impl VoiceActivityDetector for EnergyVad {
    fn contains_speech(&self, chunk: &PcmChunk) -> bool {
        chunk
            .samples
            .iter()
            .any(|sample| sample.abs() >= self.threshold)
    }
}

/// A lazy stream that emits only normalized chunks containing speech. It can
/// be stopped by dropping it, allowing a scheduler to reprioritize after seek.
pub struct SpeechOnlyStream {
    source: Box<dyn AudioStream>,
    normalizer: Box<dyn AudioNormalizer>,
    vad: Box<dyn VoiceActivityDetector>,
}

impl SpeechOnlyStream {
    pub fn new(
        source: Box<dyn AudioStream>,
        normalizer: Box<dyn AudioNormalizer>,
        vad: Box<dyn VoiceActivityDetector>,
    ) -> Self {
        Self {
            source,
            normalizer,
            vad,
        }
    }

    pub fn next_speech_chunk(&mut self) -> Result<Option<PcmChunk>, AudioPipelineError> {
        while let Some(chunk) = self.source.next_chunk()? {
            let normalized = self.normalizer.normalize(chunk)?;
            if self.vad.contains_speech(&normalized) {
                return Ok(Some(normalized));
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum AudioPipelineError {
    #[error("audio format must have a non-zero sample rate and channel count")]
    InvalidFormat,
    #[error("PCM samples are not aligned to channel boundaries")]
    MisalignedSamples,
    #[error("PCM contains a non-finite sample")]
    NonFiniteSample,
    #[error("expected {expected:?} audio but received {actual:?}")]
    UnexpectedFormat {
        actual: AudioFormat,
        expected: AudioFormat,
    },
    #[error("VAD threshold must be finite and non-negative")]
    InvalidVadThreshold,
    #[error("audio stream failed: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn ffmpeg_plan_uses_local_file_arguments_not_a_shell_command() {
        let input = AudioInput::LocalPath(PathBuf::from("C:/recordings/meeting.mp4"));
        let plan = FfmpegDecoder::default().command_plan(&input).unwrap();
        assert_eq!(plan.program(), Path::new("ffmpeg"));
        assert!(plan.arguments().iter().any(|arg| arg == "-nostdin"));
        assert!(plan
            .arguments()
            .windows(2)
            .any(|arguments| arguments[0] == "-protocol_whitelist" && arguments[1] == "file"));
        assert!(plan.arguments().iter().any(|arg| arg == "pipe:1"));
        assert!(!format!("{plan:?}").contains("meeting.mp4"));
    }

    #[test]
    fn ffmpeg_rejects_remote_urls_before_building_or_running_a_command() {
        let input =
            AudioInput::RemoteUrl(Url::parse("https://example.test/a.mp4?token=secret").unwrap());
        assert_eq!(
            FfmpegDecoder::default().command_plan(&input).unwrap_err(),
            FfmpegExtractionError::RemoteInputRequiresAcquisition
        );
    }

    #[test]
    fn energy_vad_skips_silence() {
        let silent = PcmChunk {
            start_ms: 0,
            format: AudioFormat::CANONICAL,
            samples: vec![0.0; 160],
        };
        let speech = PcmChunk {
            start_ms: 10,
            format: AudioFormat::CANONICAL,
            samples: vec![0.02; 160],
        };
        let vad = EnergyVad::default();
        assert!(!vad.contains_speech(&silent));
        assert!(vad.contains_speech(&speech));
    }

    #[derive(Clone)]
    struct FakeFfmpegRunner {
        output: Result<FfmpegCommandOutput, FfmpegRunnerError>,
        wav_bytes: Option<Vec<u8>>,
        plans: Arc<Mutex<Vec<FfmpegExtractionPlan>>>,
    }

    impl FfmpegCommandRunner for FakeFfmpegRunner {
        fn run(
            &self,
            plan: &FfmpegExtractionPlan,
            _cancellation: &ExtractionCancellation,
            _options: &FfmpegExtractionOptions,
        ) -> Result<FfmpegCommandOutput, FfmpegRunnerError> {
            self.plans.lock().unwrap().push(plan.clone());
            if let Some(bytes) = &self.wav_bytes {
                fs::write(plan.output_path(), bytes).unwrap();
            }
            self.output.clone()
        }
    }

    fn canonical_wav_bytes() -> Vec<u8> {
        let data = [0_u8; 2];
        let byte_rate = CANONICAL_SAMPLE_RATE_HZ * u32::from(CANONICAL_CHANNELS) * 2;
        let block_align = CANONICAL_CHANNELS * 2;
        let mut output = Vec::with_capacity(44 + data.len());
        output.extend_from_slice(b"RIFF");
        output.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        output.extend_from_slice(b"WAVEfmt ");
        output.extend_from_slice(&16_u32.to_le_bytes());
        output.extend_from_slice(&1_u16.to_le_bytes());
        output.extend_from_slice(&CANONICAL_CHANNELS.to_le_bytes());
        output.extend_from_slice(&CANONICAL_SAMPLE_RATE_HZ.to_le_bytes());
        output.extend_from_slice(&byte_rate.to_le_bytes());
        output.extend_from_slice(&block_align.to_le_bytes());
        output.extend_from_slice(&16_u16.to_le_bytes());
        output.extend_from_slice(b"data");
        output.extend_from_slice(&(data.len() as u32).to_le_bytes());
        output.extend_from_slice(&data);
        output
    }

    fn successful_output() -> Result<FfmpegCommandOutput, FfmpegRunnerError> {
        Ok(FfmpegCommandOutput {
            succeeded: true,
            exit_code: Some(0),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn extraction_rejects_remote_input_without_starting_ffmpeg() {
        let plans = Arc::new(Mutex::new(Vec::new()));
        let decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: successful_output(),
                wav_bytes: Some(canonical_wav_bytes()),
                plans: Arc::clone(&plans),
            },
            FfmpegExtractionOptions::default(),
        );
        let input =
            AudioInput::RemoteUrl(Url::parse("https://media.example.test/recording.mp4").unwrap());

        assert_eq!(
            decoder
                .extract_to_wav(&input, &ExtractionCancellation::new())
                .unwrap_err(),
            FfmpegExtractionError::RemoteInputRequiresAcquisition
        );
        assert!(plans.lock().unwrap().is_empty());
    }

    #[test]
    fn extraction_creates_a_private_canonical_wav_and_cleans_it_on_drop() {
        let plans = Arc::new(Mutex::new(Vec::new()));
        let decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: successful_output(),
                wav_bytes: Some(canonical_wav_bytes()),
                plans: Arc::clone(&plans),
            },
            FfmpegExtractionOptions::default(),
        );
        let input = AudioInput::LocalPath(PathBuf::from("C:/recordings/meeting.mp4"));
        let cancellation = ExtractionCancellation::new();

        let artifact_path = {
            let artifact = decoder.extract_to_wav(&input, &cancellation).unwrap();
            assert_eq!(artifact.format(), AudioFormat::CANONICAL);
            assert!(artifact.path().is_file());
            assert!(!format!("{artifact:?}").contains("meeting.mp4"));
            artifact.path().to_path_buf()
        };
        assert!(!artifact_path.exists());

        let plans = plans.lock().unwrap();
        assert_eq!(plans.len(), 1);
        assert!(plans[0]
            .arguments()
            .iter()
            .any(|argument| argument == "-c:a"));
        assert!(plans[0]
            .arguments()
            .iter()
            .any(|argument| argument == "pcm_s16le"));
        assert!(!format!("{:?}", plans[0]).contains("meeting.mp4"));
    }

    #[test]
    fn ranged_extraction_uses_fixed_seek_and_duration_arguments() {
        let plans = Arc::new(Mutex::new(Vec::new()));
        let decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: successful_output(),
                wav_bytes: Some(canonical_wav_bytes()),
                plans: Arc::clone(&plans),
            },
            FfmpegExtractionOptions::default(),
        );
        let input = AudioInput::LocalPath(PathBuf::from("C:/recordings/meeting.mp4"));
        let range = AudioExtractionRange::new(87_654, 207_654).unwrap();

        let artifact_path = {
            let artifact = decoder
                .extract_range_to_wav(&input, range, &ExtractionCancellation::new())
                .unwrap();
            assert_eq!(artifact.format(), AudioFormat::CANONICAL);
            assert!(artifact.path().is_file());
            artifact.path().to_path_buf()
        };
        assert!(!artifact_path.exists());

        let plans = plans.lock().unwrap();
        assert_eq!(plans.len(), 1);
        let arguments = plans[0]
            .arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let seek_index = arguments
            .iter()
            .position(|argument| argument == "-ss")
            .unwrap();
        assert_eq!(arguments[seek_index + 1], "87.654");
        assert_eq!(arguments[seek_index + 2], "-protocol_whitelist");
        assert_eq!(arguments[seek_index + 3], "file");
        assert_eq!(arguments[seek_index + 4], "-i");

        let input_index = arguments
            .iter()
            .position(|argument| argument == "-i")
            .unwrap();
        let duration_index = arguments
            .iter()
            .position(|argument| argument == "-t")
            .unwrap();
        assert!(duration_index > input_index);
        assert_eq!(arguments[duration_index + 1], "120.000");
        assert!(arguments.iter().any(|argument| argument == "-c:a"));
        assert!(arguments.iter().any(|argument| argument == "pcm_s16le"));
        assert!(!format!("{:?}", plans[0]).contains("meeting.mp4"));
    }

    #[test]
    fn invalid_audio_extraction_ranges_are_rejected() {
        assert_eq!(
            AudioExtractionRange::new(42, 42),
            Err(AudioExtractionRangeError::InvalidBounds {
                start_ms: 42,
                end_ms: 42,
            })
        );
        assert_eq!(
            AudioExtractionRange::new(43, 42),
            Err(AudioExtractionRangeError::InvalidBounds {
                start_ms: 43,
                end_ms: 42,
            })
        );
        assert_eq!(
            AudioExtractionRange::new(0, MAX_AUDIO_EXTRACTION_RANGE_MS + 1),
            Err(AudioExtractionRangeError::ExceedsMaximumDuration {
                duration_ms: MAX_AUDIO_EXTRACTION_RANGE_MS + 1,
                maximum_ms: MAX_AUDIO_EXTRACTION_RANGE_MS,
            })
        );
    }

    #[test]
    fn ranged_extraction_preserves_timeout_and_private_cleanup_behavior() {
        let plans = Arc::new(Mutex::new(Vec::new()));
        let decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: Err(FfmpegRunnerError::TimedOut),
                wav_bytes: None,
                plans: Arc::clone(&plans),
            },
            FfmpegExtractionOptions::default(),
        );
        let input = AudioInput::LocalPath(PathBuf::from("C:/recordings/meeting.mp4"));

        assert_eq!(
            decoder
                .extract_range_to_wav(
                    &input,
                    AudioExtractionRange::new(30_000, 60_000).unwrap(),
                    &ExtractionCancellation::new(),
                )
                .unwrap_err(),
            FfmpegExtractionError::TimedOut
        );
        let output_path = plans.lock().unwrap()[0].output_path().to_path_buf();
        assert!(!output_path.exists());
    }

    #[test]
    fn cancellation_prevents_a_process_launch() {
        let plans = Arc::new(Mutex::new(Vec::new()));
        let decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: successful_output(),
                wav_bytes: Some(canonical_wav_bytes()),
                plans: Arc::clone(&plans),
            },
            FfmpegExtractionOptions::default(),
        );
        let cancellation = ExtractionCancellation::new();
        cancellation.cancel();
        let input = AudioInput::LocalPath(PathBuf::from("C:/recordings/meeting.mp4"));

        assert_eq!(
            decoder.extract_to_wav(&input, &cancellation).unwrap_err(),
            FfmpegExtractionError::Cancelled
        );
        assert_eq!(
            decoder
                .extract_range_to_wav(
                    &input,
                    AudioExtractionRange::new(0, 30_000).unwrap(),
                    &cancellation,
                )
                .unwrap_err(),
            FfmpegExtractionError::Cancelled
        );
        assert!(plans.lock().unwrap().is_empty());
    }

    #[test]
    fn oversize_or_non_wav_artifacts_fail_without_leaving_temporary_audio() {
        let plans = Arc::new(Mutex::new(Vec::new()));
        let options = FfmpegExtractionOptions {
            max_output_bytes: 1,
            ..FfmpegExtractionOptions::default()
        };
        let decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: successful_output(),
                wav_bytes: Some(canonical_wav_bytes()),
                plans: Arc::clone(&plans),
            },
            options,
        );
        let input = AudioInput::LocalPath(PathBuf::from("C:/recordings/meeting.mp4"));
        let error = decoder
            .extract_to_wav(&input, &ExtractionCancellation::new())
            .unwrap_err();
        assert!(matches!(
            error,
            FfmpegExtractionError::OutputTooLarge { .. }
        ));
        let output_path = plans.lock().unwrap()[0].output_path().to_path_buf();
        assert!(!output_path.exists());

        let invalid_decoder = FfmpegDecoder::with_runner(
            "ffmpeg",
            FakeFfmpegRunner {
                output: successful_output(),
                wav_bytes: Some(b"not a wav artifact".to_vec()),
                plans: Arc::new(Mutex::new(Vec::new())),
            },
            FfmpegExtractionOptions::default(),
        );
        assert_eq!(
            invalid_decoder
                .extract_to_wav(&input, &ExtractionCancellation::new())
                .unwrap_err(),
            FfmpegExtractionError::InvalidOutputFormat
        );
    }
}
