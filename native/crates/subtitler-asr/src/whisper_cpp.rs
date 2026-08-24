use crate::{
    AsrError, ComputeBackend, LanguageMode, LocalModel, Quantization, SpeechCapabilities,
    SpeechProvider, TranscriptionRequest,
};
use serde_json::Value;
use std::{
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use subtitler_core::{TimeRange, Transcript, TranscriptSegment, WordTimestamp};
use subtitler_media::{AudioFormat, AudioStream};
use tempfile::TempDir;
use thiserror::Error;

const WAV_HEADER_BYTES: u64 = 44;
const DEFAULT_WHISPER_THREAD_COUNT: u16 = 4;
/// Leave enough CPU capacity for the active browser and page renderer. The
/// command-line engine has no useful reason to consume every logical core for
/// this single audio range.
const MAX_AUTOMATIC_WHISPER_THREAD_COUNT: u16 = 8;

/// Configuration for a local `whisper.cpp` CLI installation.
///
/// The native host owns selecting these local paths. They are deliberately
/// excluded from `Debug` so a routine diagnostic cannot expose a user-specific
/// model directory.
#[derive(Clone, PartialEq, Eq)]
pub struct WhisperCppConfig {
    pub executable: PathBuf,
    pub model_path: PathBuf,
    pub model: LocalModel,
    pub quantization: Quantization,
    pub backend: ComputeBackend,
    /// Bounded compute parallelism for a single whisper.cpp invocation.
    /// This is neither a model-quality setting nor user content.
    pub thread_count: u16,
}

impl fmt::Debug for WhisperCppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WhisperCppConfig")
            .field("executable", &"<redacted local path>")
            .field("model_path", &"<redacted local path>")
            .field("model", &self.model)
            .field("quantization", &self.quantization)
            .field("backend", &self.backend)
            .field("thread_count", &self.thread_count)
            .finish()
    }
}

/// Validation failures are intentionally specific enough for setup UX without
/// embedding local paths in error messages or job status.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WhisperCppConfigError {
    #[error("The whisper.cpp executable is not installed.")]
    ExecutableMissing,
    #[error("The configured whisper.cpp executable is not a file.")]
    ExecutableNotFile,
    #[error("The selected whisper.cpp model is not installed.")]
    ModelMissing,
    #[error("The configured whisper.cpp model is not a file.")]
    ModelNotFile,
    #[error("The requested model does not match the configured whisper.cpp model.")]
    ModelMismatch,
    #[error("The requested quantization does not match the configured whisper.cpp model.")]
    QuantizationMismatch,
    #[error("The requested backend does not match the configured whisper.cpp backend.")]
    BackendMismatch,
    #[error("The configured whisper.cpp thread count must be between 1 and 8.")]
    InvalidThreadCount,
}

impl WhisperCppConfig {
    /// Locates a conventional whisper.cpp model filename in `model_directory`.
    ///
    /// The candidate list covers the common `.bin` and `.gguf` layouts. A
    /// caller with a custom filename can construct `WhisperCppConfig` directly
    /// and still receive the same validation before process launch.
    pub fn discover(
        executable: impl Into<PathBuf>,
        model_directory: impl AsRef<Path>,
        model: LocalModel,
        quantization: Quantization,
        backend: ComputeBackend,
    ) -> Result<Self, WhisperCppConfigError> {
        let executable = executable.into();
        validate_executable(&executable)?;

        let model_path = Self::model_file_names(model, quantization)
            .into_iter()
            .map(|name| model_directory.as_ref().join(name))
            .find(|candidate| candidate.is_file())
            .ok_or(WhisperCppConfigError::ModelMissing)?;

        let config = Self {
            executable,
            model_path,
            model,
            quantization,
            backend,
            thread_count: Self::recommended_thread_count(),
        };
        config.validate()?;
        Ok(config)
    }

    /// Returns the conventional filenames searched by [`Self::discover`], in
    /// deterministic preference order.
    pub fn model_file_names(model: LocalModel, quantization: Quantization) -> Vec<String> {
        let model_name = match model {
            LocalModel::Tiny => "tiny",
            LocalModel::Base => "base",
            LocalModel::Small => "small",
            LocalModel::Medium => "medium",
            LocalModel::LargeV3Turbo => "large-v3-turbo",
        };
        let quantization_name = match quantization {
            Quantization::Q5_0 => "q5_0",
            Quantization::Q5Km => "q5_k_m",
            Quantization::Q8_0 => "q8_0",
            Quantization::F16 => "f16",
        };

        let base = format!("ggml-{model_name}");
        let mut candidates = vec![
            format!("{base}-{quantization_name}.bin"),
            format!("{base}-{quantization_name}.gguf"),
        ];

        // Standard full-precision whisper.cpp downloads are commonly named
        // `ggml-small.bin`, rather than carrying an `f16` suffix.
        if quantization == Quantization::F16 {
            candidates.insert(0, format!("{base}.bin"));
            candidates.insert(1, format!("{base}.gguf"));
        }

        candidates
    }

    /// Chooses a bounded local parallelism level. A single subtitle window
    /// should become ready quickly while leaving capacity for Chrome playback.
    /// Advanced callers may set a smaller valid `thread_count` explicitly.
    pub fn recommended_thread_count() -> u16 {
        thread::available_parallelism()
            .ok()
            .map(|count| u16::try_from(count.get()).unwrap_or(MAX_AUTOMATIC_WHISPER_THREAD_COUNT))
            .unwrap_or(DEFAULT_WHISPER_THREAD_COUNT)
            .clamp(1, MAX_AUTOMATIC_WHISPER_THREAD_COUNT)
    }

    /// Checks that this configuration can be safely supplied to a process.
    pub fn validate(&self) -> Result<(), WhisperCppConfigError> {
        validate_executable(&self.executable)?;
        validate_model(&self.model_path)?;
        if !(1..=MAX_AUTOMATIC_WHISPER_THREAD_COUNT).contains(&self.thread_count) {
            return Err(WhisperCppConfigError::InvalidThreadCount);
        }
        Ok(())
    }

    /// Ensures a caller cannot request a different model selection than the
    /// binary/model pair that was explicitly configured for the local engine.
    pub fn validate_for_request(
        &self,
        request: &TranscriptionRequest,
    ) -> Result<(), WhisperCppConfigError> {
        self.validate()?;
        if self.model != request.model {
            return Err(WhisperCppConfigError::ModelMismatch);
        }
        if self.quantization != request.quantization {
            return Err(WhisperCppConfigError::QuantizationMismatch);
        }
        if self.backend != request.backend {
            return Err(WhisperCppConfigError::BackendMismatch);
        }
        Ok(())
    }

    /// Whether this configured file is one of whisper.cpp's English-only
    /// model variants. Those variants cannot perform Whisper's translate task;
    /// asking them to do so weakens both recognition and timestamp quality.
    ///
    /// This deliberately classifies only the conventional, unambiguous
    /// `*.en.*` / `*.en-*` filename forms. Unknown custom names preserve the
    /// existing multilingual-capable behaviour rather than making an
    /// unsupported claim about a user's model asset.
    pub fn is_english_only_model(&self) -> bool {
        let Some(name) = self.model_path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        name.contains(".en.") || name.contains(".en-") || name.ends_with(".en")
    }

    /// Creates a command plan for a WAV input and a private output prefix.
    /// No argument is concatenated into a shell command. `-ojf` asks supported
    /// whisper.cpp builds for token timing data when callers request it.
    pub fn cli_plan(
        &self,
        normalized_audio_path: &Path,
        request: &TranscriptionRequest,
        output_prefix: &Path,
    ) -> WhisperCppCommandPlan {
        let mut arguments = vec![
            OsString::from("-m"),
            self.model_path.as_os_str().to_os_string(),
            OsString::from("-f"),
            normalized_audio_path.as_os_str().to_os_string(),
            OsString::from("-oj"),
            OsString::from("-of"),
            output_prefix.as_os_str().to_os_string(),
            // Output is consumed from the JSON artifact, so suppress terminal
            // transcript printing and avoid holding an unbounded stdout pipe.
            OsString::from("-np"),
            OsString::from("-t"),
            OsString::from(self.thread_count.to_string()),
        ];

        if request.word_timestamps {
            arguments.push(OsString::from("-ojf"));
        }

        match request.language_mode {
            LanguageMode::English => {
                arguments.push(OsString::from("-l"));
                arguments.push(OsString::from("en"));
            }
            LanguageMode::TranslateInputToEnglish => {
                arguments.push(OsString::from("-tr"));
            }
        }

        WhisperCppCommandPlan {
            program: self.executable.clone(),
            arguments,
            output_json_path: output_prefix.with_extension("json"),
        }
    }
}

fn validate_executable(path: &Path) -> Result<(), WhisperCppConfigError> {
    let metadata = fs::metadata(path).map_err(|_| WhisperCppConfigError::ExecutableMissing)?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(WhisperCppConfigError::ExecutableNotFile)
    }
}

fn validate_model(path: &Path) -> Result<(), WhisperCppConfigError> {
    let metadata = fs::metadata(path).map_err(|_| WhisperCppConfigError::ModelMissing)?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(WhisperCppConfigError::ModelNotFile)
    }
}

/// An argument-vector process plan. It has no shell representation and keeps
/// local paths out of its `Debug` representation.
#[derive(Clone)]
pub struct WhisperCppCommandPlan {
    program: PathBuf,
    arguments: Vec<OsString>,
    output_json_path: PathBuf,
}

impl WhisperCppCommandPlan {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub fn output_json_path(&self) -> &Path {
        &self.output_json_path
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        command
    }
}

impl fmt::Debug for WhisperCppCommandPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WhisperCppCommandPlan")
            .field("program", &"<redacted local path>")
            .field("arguments", &"<redacted local paths and inputs>")
            .field("output_json_path", &"<redacted temporary path>")
            .finish()
    }
}

/// A cloneable cancellation signal shared between the job scheduler and a
/// whisper.cpp process invocation. Cancellation is cooperative up to process
/// polling, then the child process is terminated before the method returns.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
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

/// Bounds process lifetime and cancellation polling. The two-hour default is
/// deliberately generous for an offline full-recording job while still
/// guaranteeing that a wedged local process cannot run forever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhisperCppExecutionOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for WhisperCppExecutionOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2 * 60 * 60),
            poll_interval: Duration::from_millis(50),
        }
    }
}

/// Per-invocation control supplied by the native job scheduler.
#[derive(Clone, Debug)]
pub struct WhisperCppExecutionControl {
    pub cancellation: CancellationToken,
    pub options: WhisperCppExecutionOptions,
    /// Shared only with the native job watchdog. It carries a millisecond
    /// heartbeat, never a process command line, source, or transcript text.
    pub activity_heartbeat: Option<Arc<AtomicU64>>,
}

impl WhisperCppExecutionControl {
    pub fn new(cancellation: CancellationToken, options: WhisperCppExecutionOptions) -> Self {
        Self {
            cancellation,
            options,
            activity_heartbeat: None,
        }
    }

    pub fn with_activity_heartbeat(
        cancellation: CancellationToken,
        options: WhisperCppExecutionOptions,
        activity_heartbeat: Arc<AtomicU64>,
    ) -> Self {
        Self {
            cancellation,
            options,
            activity_heartbeat: Some(activity_heartbeat),
        }
    }

    pub fn touch_activity(&self) {
        let Some(heartbeat) = self.activity_heartbeat.as_ref() else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        heartbeat.store(now, Ordering::Release);
    }
}

impl Default for WhisperCppExecutionControl {
    fn default() -> Self {
        Self::new(
            CancellationToken::new(),
            WhisperCppExecutionOptions::default(),
        )
    }
}

/// A runner result. Tests can inject `json_output` directly; the system runner
/// leaves it as `None` and the engine reads the JSON file requested in the
/// command plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhisperCppCommandOutput {
    pub succeeded: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub json_output: Option<Vec<u8>>,
}

/// Process-layer failures do not carry OS error text because it may include
/// private local paths. They are converted into concise user-safe `AsrError`s.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum WhisperCppRunnerError {
    #[error("whisper.cpp could not be started")]
    Spawn,
    #[error("whisper.cpp process I/O failed")]
    Io,
    #[error("whisper.cpp timed out")]
    TimedOut,
    #[error("whisper.cpp was cancelled")]
    Cancelled,
}

/// Isolates child-process management from command planning and parsing. It is
/// intentionally injectable so CI can prove the complete adapter behavior
/// without installing a model or executing whisper.cpp.
pub trait WhisperCppCommandRunner: Send + Sync {
    fn run(
        &self,
        plan: &WhisperCppCommandPlan,
        control: &WhisperCppExecutionControl,
    ) -> Result<WhisperCppCommandOutput, WhisperCppRunnerError>;
}

/// The production process runner. It launches the supplied executable
/// directly (never through a shell), discards terminal output because the
/// requested JSON artifact is the only supported result channel, and
/// terminates the child on cancellation or timeout.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWhisperCppCommandRunner;

impl WhisperCppCommandRunner for SystemWhisperCppCommandRunner {
    fn run(
        &self,
        plan: &WhisperCppCommandPlan,
        control: &WhisperCppExecutionControl,
    ) -> Result<WhisperCppCommandOutput, WhisperCppRunnerError> {
        if control.cancellation.is_cancelled() {
            return Err(WhisperCppRunnerError::Cancelled);
        }
        if control.options.timeout.is_zero() {
            return Err(WhisperCppRunnerError::TimedOut);
        }

        let mut command = plan.to_command();
        command
            .stdin(Stdio::null())
            // `-np` suppresses the human-readable transcript. The host reads
            // only its private JSON artifact; terminal pipes add no result
            // value and can leave a completion thread waiting for EOF.
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|_| WhisperCppRunnerError::Spawn)?;
        control.touch_activity();
        let started = Instant::now();
        let poll_interval = control.options.poll_interval.max(Duration::from_millis(10));

        let status = loop {
            control.touch_activity();
            if control.cancellation.is_cancelled() {
                terminate_child(&mut child);
                return Err(WhisperCppRunnerError::Cancelled);
            }
            if started.elapsed() >= control.options.timeout {
                terminate_child(&mut child);
                return Err(WhisperCppRunnerError::TimedOut);
            }

            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(poll_interval),
                Err(_) => {
                    terminate_child(&mut child);
                    return Err(WhisperCppRunnerError::Io);
                }
            }
        };

        Ok(WhisperCppCommandOutput {
            succeeded: status.success(),
            exit_code: status.code(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            json_output: None,
        })
    }
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A real CLI adapter for an installed whisper.cpp binary and local model.
/// `R` is injectable solely to make lifecycle and parser tests hermetic.
pub struct WhisperCppCliEngine<R = SystemWhisperCppCommandRunner> {
    runner: R,
    options: WhisperCppExecutionOptions,
}

impl Default for WhisperCppCliEngine<SystemWhisperCppCommandRunner> {
    fn default() -> Self {
        Self::new(WhisperCppExecutionOptions::default())
    }
}

impl WhisperCppCliEngine<SystemWhisperCppCommandRunner> {
    pub fn new(options: WhisperCppExecutionOptions) -> Self {
        Self {
            runner: SystemWhisperCppCommandRunner,
            options,
        }
    }
}

impl<R> WhisperCppCliEngine<R> {
    pub fn with_runner(runner: R, options: WhisperCppExecutionOptions) -> Self {
        Self { runner, options }
    }

    pub fn options(&self) -> &WhisperCppExecutionOptions {
        &self.options
    }
}

impl<R: WhisperCppCommandRunner> WhisperCppCliEngine<R> {
    /// Transcribes a pre-normalized local WAV artifact. This is the primary
    /// integration point for the native host after its media pipeline has
    /// extracted and normalized audio; no `AudioStream` reconstruction is
    /// required.
    pub fn transcribe_file(
        &self,
        config: &WhisperCppConfig,
        request: &TranscriptionRequest,
        normalized_audio_path: &Path,
    ) -> Result<Transcript, AsrError> {
        let control =
            WhisperCppExecutionControl::new(CancellationToken::new(), self.options.clone());
        self.transcribe_file_with_control(config, request, normalized_audio_path, &control)
    }

    /// The cancellation-aware variant used by a job scheduler. The invocation
    /// owns a private temporary output directory and deletes it when parsing
    /// finishes or an error occurs.
    pub fn transcribe_file_with_control(
        &self,
        config: &WhisperCppConfig,
        request: &TranscriptionRequest,
        normalized_audio_path: &Path,
        control: &WhisperCppExecutionControl,
    ) -> Result<Transcript, AsrError> {
        if control.cancellation.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        config
            .validate_for_request(request)
            .map_err(config_error_to_asr)?;
        if !normalized_audio_path.is_file() {
            return Err(AsrError::ProcessingFailed(
                "The normalized audio artifact is no longer available.".to_owned(),
            ));
        }

        let output_directory = create_private_temp_dir()?;
        let output_prefix = output_directory.path().join("transcription");
        let plan = config.cli_plan(normalized_audio_path, request, &output_prefix);
        let output = self
            .runner
            .run(&plan, control)
            .map_err(|error| runner_error_to_asr(error, control.options.timeout))?;

        if control.cancellation.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        if !output.succeeded {
            let status = output
                .exit_code
                .map(|code| format!("whisper.cpp exited with status {code}."))
                .unwrap_or_else(|| "whisper.cpp exited unsuccessfully.".to_owned());
            return Err(AsrError::ProcessingFailed(status));
        }

        let json = match output.json_output {
            Some(json) => json,
            None => fs::read(plan.output_json_path()).map_err(|_| {
                AsrError::InvalidOutput(
                    "whisper.cpp completed but did not create its JSON transcription file."
                        .to_owned(),
                )
            })?,
        };
        parse_whisper_cpp_json(&json, request)
    }

    /// Compatibility path for existing `SpeechProvider` callers. It writes
    /// canonical 16 kHz mono PCM to a temporary WAV and then follows the same
    /// direct-file path as the native host.
    pub fn transcribe_audio_stream_with_control(
        &self,
        config: &WhisperCppConfig,
        request: &TranscriptionRequest,
        audio: &mut dyn AudioStream,
        control: &WhisperCppExecutionControl,
    ) -> Result<Transcript, AsrError> {
        let input_directory = create_private_temp_dir()?;
        let input_path = input_directory.path().join("normalized-input.wav");
        write_audio_stream_as_wave(audio, &input_path, &control.cancellation)?;
        self.transcribe_file_with_control(config, request, &input_path, control)
    }
}

fn create_private_temp_dir() -> Result<TempDir, AsrError> {
    tempfile::Builder::new()
        .prefix("subtitler-asr-")
        .tempdir()
        .map_err(|_| {
            AsrError::ProcessingFailed(
                "Subtitler could not create a private temporary processing location.".to_owned(),
            )
        })
}

fn config_error_to_asr(error: WhisperCppConfigError) -> AsrError {
    AsrError::InvalidConfiguration(error.to_string())
}

fn runner_error_to_asr(error: WhisperCppRunnerError, timeout: Duration) -> AsrError {
    match error {
        WhisperCppRunnerError::Cancelled => AsrError::Cancelled,
        WhisperCppRunnerError::TimedOut => AsrError::TimedOut {
            timeout_ms: duration_to_millis(timeout),
        },
        WhisperCppRunnerError::Spawn => AsrError::ProcessingFailed(
            "The configured whisper.cpp executable could not be started.".to_owned(),
        ),
        WhisperCppRunnerError::Io => AsrError::ProcessingFailed(
            "The whisper.cpp process encountered an I/O failure.".to_owned(),
        ),
    }
}

fn duration_to_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn write_audio_stream_as_wave(
    audio: &mut dyn AudioStream,
    output_path: &Path,
    cancellation: &CancellationToken,
) -> Result<(), AsrError> {
    let mut file = File::create(output_path).map_err(|_| {
        AsrError::ProcessingFailed("The normalized audio artifact could not be created.".to_owned())
    })?;
    file.write_all(&[0_u8; WAV_HEADER_BYTES as usize])
        .map_err(|_| {
            AsrError::ProcessingFailed(
                "The normalized audio artifact could not be written.".to_owned(),
            )
        })?;

    let mut written_frames = 0_u64;
    while let Some(chunk) = audio.next_chunk().map_err(|_| {
        AsrError::ProcessingFailed("The normalized audio stream could not be read.".to_owned())
    })? {
        if cancellation.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        chunk.validate().map_err(|_| {
            AsrError::ProcessingFailed("The normalized audio stream is invalid.".to_owned())
        })?;
        if chunk.format != AudioFormat::CANONICAL {
            return Err(AsrError::ProcessingFailed(
                "The local speech engine requires canonical 16 kHz mono audio.".to_owned(),
            ));
        }

        let expected_start_frame = chunk.start_ms.saturating_mul(16);
        if expected_start_frame > written_frames {
            ensure_wave_fits(expected_start_frame)?;
            write_silence_frames(&mut file, expected_start_frame - written_frames)?;
            written_frames = expected_start_frame;
        }

        let overlapping_frames = written_frames.saturating_sub(expected_start_frame) as usize;
        let samples = chunk.samples.get(overlapping_frames..).unwrap_or_default();
        ensure_wave_fits(written_frames.saturating_add(samples.len() as u64))?;
        write_pcm16_samples(&mut file, samples)?;
        written_frames = written_frames.saturating_add(samples.len() as u64);
        ensure_wave_fits(written_frames)?;
    }

    let data_bytes = written_frames.saturating_mul(2);
    ensure_wave_fits(written_frames)?;
    file.seek(SeekFrom::Start(0)).map_err(|_| {
        AsrError::ProcessingFailed(
            "The normalized audio artifact could not be finalized.".to_owned(),
        )
    })?;
    write_wave_header(&mut file, data_bytes)?;
    file.flush().map_err(|_| {
        AsrError::ProcessingFailed(
            "The normalized audio artifact could not be finalized.".to_owned(),
        )
    })?;
    Ok(())
}

fn ensure_wave_fits(frames: u64) -> Result<(), AsrError> {
    let data_bytes = frames.saturating_mul(2);
    if data_bytes > u64::from(u32::MAX)
        || WAV_HEADER_BYTES.saturating_add(data_bytes) > u64::from(u32::MAX)
    {
        return Err(AsrError::ProcessingFailed(
            "The normalized audio artifact is too large for a single WAV file.".to_owned(),
        ));
    }
    Ok(())
}

fn write_silence_frames(file: &mut File, frames: u64) -> Result<(), AsrError> {
    ensure_wave_fits(frames)?;
    let silence = [0_u8; 8_192];
    let mut remaining_bytes = frames.saturating_mul(2);
    while remaining_bytes > 0 {
        let bytes = remaining_bytes.min(silence.len() as u64) as usize;
        file.write_all(&silence[..bytes]).map_err(|_| {
            AsrError::ProcessingFailed(
                "The normalized audio artifact could not be written.".to_owned(),
            )
        })?;
        remaining_bytes -= bytes as u64;
    }
    Ok(())
}

fn write_pcm16_samples(file: &mut File, samples: &[f32]) -> Result<(), AsrError> {
    let mut bytes = Vec::with_capacity(samples.len().saturating_mul(2));
    for sample in samples {
        let pcm = if *sample <= -1.0 {
            i16::MIN
        } else if *sample >= 1.0 {
            i16::MAX
        } else {
            (*sample * f32::from(i16::MAX)).round() as i16
        };
        bytes.extend_from_slice(&pcm.to_le_bytes());
    }
    file.write_all(&bytes).map_err(|_| {
        AsrError::ProcessingFailed("The normalized audio artifact could not be written.".to_owned())
    })
}

fn write_wave_header(file: &mut File, data_bytes: u64) -> Result<(), AsrError> {
    let data_bytes = u32::try_from(data_bytes).map_err(|_| {
        AsrError::ProcessingFailed(
            "The normalized audio artifact is too large for a single WAV file.".to_owned(),
        )
    })?;
    let riff_size = 36_u32.checked_add(data_bytes).ok_or_else(|| {
        AsrError::ProcessingFailed(
            "The normalized audio artifact is too large for a single WAV file.".to_owned(),
        )
    })?;

    let mut header = Vec::with_capacity(WAV_HEADER_BYTES as usize);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&16_000_u32.to_le_bytes());
    header.extend_from_slice(&(16_000_u32 * 2).to_le_bytes());
    header.extend_from_slice(&2_u16.to_le_bytes());
    header.extend_from_slice(&16_u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    debug_assert_eq!(header.len(), WAV_HEADER_BYTES as usize);
    file.write_all(&header).map_err(|_| {
        AsrError::ProcessingFailed(
            "The normalized audio artifact could not be finalized.".to_owned(),
        )
    })
}

/// Converts whisper.cpp's JSON result into the shared transcript domain.
///
/// The parser supports the JSON layouts emitted by whisper.cpp releases that
/// use either `transcription`/`offsets` or `segments`/`t0`/`t1`. Its input is
/// intentionally a byte slice so both the real process runner and deterministic
/// tests exercise exactly the same conversion.
pub fn parse_whisper_cpp_json(
    json: &[u8],
    request: &TranscriptionRequest,
) -> Result<Transcript, AsrError> {
    let root: Value = serde_json::from_slice(json)
        .map_err(|_| AsrError::InvalidOutput("whisper.cpp returned malformed JSON.".to_owned()))?;
    let source_language = find_language(&root);
    let segment_values = find_segments(&root).ok_or_else(|| {
        AsrError::InvalidOutput(
            "whisper.cpp JSON did not contain a transcription segment list.".to_owned(),
        )
    })?;

    let mut segments = Vec::with_capacity(segment_values.len());
    let mut previous_segment_start = 0_u64;
    for value in segment_values {
        let timing = parse_timing(value).ok_or_else(|| {
            AsrError::InvalidOutput(
                "A whisper.cpp segment did not contain valid start and end timestamps.".to_owned(),
            )
        })?;
        if !segments.is_empty() && timing.start_ms < previous_segment_start {
            return Err(AsrError::InvalidOutput(
                "whisper.cpp segment timestamps are not ordered.".to_owned(),
            ));
        }
        previous_segment_start = timing.start_ms;

        let parsed_words = if request.word_timestamps {
            parse_words(value).unwrap_or_default()
        } else {
            Vec::new()
        };
        let text = find_text(value).unwrap_or_else(|| words_to_text(&parsed_words));
        if text.trim().is_empty() {
            // Empty token-only entries are normal around special model tokens.
            continue;
        }
        // `-ojf` normally carries token timings, but some whisper.cpp builds
        // omit them for a segment (and some emit overlapping subword timings).
        // A valid segment timestamp is still useful: split its recognized text
        // deterministically rather than dropping the complete subtitle job.
        let words = if request.word_timestamps && parsed_words.is_empty() {
            synthesize_word_timestamps(&text, timing)
        } else {
            parsed_words
        };

        segments.push(TranscriptSegment {
            timing,
            text,
            speaker: None,
            words,
        });
    }

    let (language, translated_from) = match request.language_mode {
        LanguageMode::English => (source_language.unwrap_or_else(|| "en".to_owned()), None),
        LanguageMode::TranslateInputToEnglish => {
            let translated_from =
                source_language.filter(|language| !language.eq_ignore_ascii_case("en"));
            ("en".to_owned(), translated_from)
        }
    };
    normalize_transcript_word_timestamps(&mut segments);
    let transcript = Transcript {
        language,
        translated_from,
        segments,
    };
    transcript.validate().map_err(|_| {
        AsrError::InvalidOutput("whisper.cpp word timestamps are not ordered.".to_owned())
    })?;
    Ok(transcript)
}

fn find_segments(root: &Value) -> Option<&Vec<Value>> {
    root.get("transcription")
        .and_then(Value::as_array)
        .or_else(|| root.get("segments").and_then(Value::as_array))
        .or_else(|| {
            root.get("result")
                .and_then(|result| result.get("transcription"))
                .and_then(Value::as_array)
        })
        .or_else(|| {
            root.get("result")
                .and_then(|result| result.get("segments"))
                .and_then(Value::as_array)
        })
}

fn find_language(root: &Value) -> Option<String> {
    root.get("language")
        .and_then(Value::as_str)
        .or_else(|| {
            root.get("result")
                .and_then(|result| result.get("language"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

fn find_text(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn parse_words(segment: &Value) -> Result<Vec<WordTimestamp>, AsrError> {
    let word_entries = segment.get("words").and_then(Value::as_array);
    let tokens = word_entries.or_else(|| segment.get("tokens").and_then(Value::as_array));
    let Some(tokens) = tokens else {
        return Ok(Vec::new());
    };

    let mut words: Vec<WordTimestamp> = Vec::new();
    for token in tokens {
        let raw_text = token
            .get("word")
            .and_then(Value::as_str)
            .or_else(|| token.get("text").and_then(Value::as_str));
        let Some(raw_text) = raw_text else {
            continue;
        };
        let trimmed = raw_text.trim();
        if trimmed.is_empty() || is_special_token(trimmed) {
            continue;
        }
        let Some(timing) = parse_timing(token) else {
            // Segment-level timing remains valid even when an individual
            // token has no timing record. The caller will synthesize words
            // from the recognized segment text when no reliable words remain.
            continue;
        };

        let starts_new_word = word_entries.is_some()
            || raw_text
                .chars()
                .next()
                .map(char::is_whitespace)
                .unwrap_or(false)
            || words.is_empty();
        if starts_new_word {
            words.push(WordTimestamp {
                text: trimmed.to_owned(),
                timing,
                speaker: None,
            });
        } else {
            let previous = words
                .last_mut()
                .expect("a non-empty word list has a last item");
            previous.text.push_str(trimmed);
            previous.timing.end_ms = previous.timing.end_ms.max(timing.end_ms);
        }
    }
    Ok(words)
}

fn synthesize_word_timestamps(text: &str, timing: TimeRange) -> Vec<WordTimestamp> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() {
        return Vec::new();
    }

    let duration = timing.end_ms.saturating_sub(timing.start_ms);
    let word_count = words.len() as u64;
    words
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let index = index as u64;
            let start_ms = timing
                .start_ms
                .saturating_add(duration.saturating_mul(index) / word_count);
            let end_ms = if index + 1 == word_count {
                timing.end_ms
            } else {
                timing
                    .start_ms
                    .saturating_add(duration.saturating_mul(index + 1) / word_count)
            };
            WordTimestamp {
                text: text.to_owned(),
                timing: TimeRange::new(start_ms, end_ms)
                    .expect("a synthesized timestamp must not run backwards"),
                speaker: None,
            }
        })
        .collect()
}

fn normalize_transcript_word_timestamps(segments: &mut [TranscriptSegment]) {
    let mut previous_end_ms = 0_u64;
    for segment in segments {
        for word in &mut segment.words {
            if word.timing.start_ms < previous_end_ms {
                word.timing.start_ms = previous_end_ms;
            }
            if word.timing.end_ms < word.timing.start_ms {
                word.timing.end_ms = word.timing.start_ms;
            }
            previous_end_ms = word.timing.end_ms;
        }
    }
}

fn is_special_token(text: &str) -> bool {
    text.starts_with('[') && text.ends_with(']')
}

fn words_to_text(words: &[WordTimestamp]) -> String {
    words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_timing(value: &Value) -> Option<TimeRange> {
    parse_timing_from_timestamp_strings(value)
        .or_else(|| parse_timing_from_offsets(value))
        .or_else(|| parse_timing_from_centiseconds(value))
        .or_else(|| parse_timing_from_milliseconds(value))
        .or_else(|| parse_timing_from_seconds(value))
}

fn parse_timing_from_timestamp_strings(value: &Value) -> Option<TimeRange> {
    let timestamps = value.get("timestamps")?;
    let start = parse_timestamp_string(timestamps.get("from")?.as_str()?)?;
    let end = parse_timestamp_string(timestamps.get("to")?.as_str()?)?;
    TimeRange::new(start, end).ok()
}

fn parse_timing_from_offsets(value: &Value) -> Option<TimeRange> {
    let offsets = value.get("offsets")?;
    let start = json_number(offsets.get("from")?)?;
    let end = json_number(offsets.get("to")?)?;
    TimeRange::new(number_to_u64(start)?, number_to_u64(end)?).ok()
}

fn parse_timing_from_centiseconds(value: &Value) -> Option<TimeRange> {
    let start = json_number(value.get("t0")?)?;
    let end = json_number(value.get("t1")?)?;
    TimeRange::new(number_to_u64(start * 10.0)?, number_to_u64(end * 10.0)?).ok()
}

fn parse_timing_from_milliseconds(value: &Value) -> Option<TimeRange> {
    let start = json_number(value.get("start_ms")?)?;
    let end = json_number(value.get("end_ms")?)?;
    TimeRange::new(number_to_u64(start)?, number_to_u64(end)?).ok()
}

fn parse_timing_from_seconds(value: &Value) -> Option<TimeRange> {
    let start = json_number(value.get("start")?)?;
    let end = json_number(value.get("end")?)?;
    TimeRange::new(
        number_to_u64(start * 1_000.0)?,
        number_to_u64(end * 1_000.0)?,
    )
    .ok()
}

fn json_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|string| string.parse::<f64>().ok()))
}

fn number_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
        return None;
    }
    Some(value.round() as u64)
}

fn parse_timestamp_string(value: &str) -> Option<u64> {
    let mut components = value.trim().split(':');
    let hours = components.next()?.parse::<u64>().ok()?;
    let minutes = components.next()?.parse::<u64>().ok()?;
    let seconds_component = components.next()?;
    if components.next().is_some() || minutes >= 60 {
        return None;
    }
    let seconds = seconds_component.replace(',', ".").parse::<f64>().ok()?;
    if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let prefix_ms = hours
        .checked_mul(3_600_000)?
        .checked_add(minutes.checked_mul(60_000)?)?;
    prefix_ms.checked_add(number_to_u64(seconds * 1_000.0)?)
}

/// The minimal bridge a linked C/C++ whisper.cpp integration must provide.
/// Keeping this boundary safe means any future `unsafe` FFI is localized to a
/// dedicated adapter rather than leaking through job scheduling code.
pub trait WhisperCppEngine: Send + Sync {
    fn transcribe(
        &self,
        config: &WhisperCppConfig,
        request: &TranscriptionRequest,
        audio: &mut dyn AudioStream,
    ) -> Result<Transcript, AsrError>;
}

impl<R: WhisperCppCommandRunner> WhisperCppEngine for WhisperCppCliEngine<R> {
    fn transcribe(
        &self,
        config: &WhisperCppConfig,
        request: &TranscriptionRequest,
        audio: &mut dyn AudioStream,
    ) -> Result<Transcript, AsrError> {
        let control =
            WhisperCppExecutionControl::new(CancellationToken::new(), self.options.clone());
        self.transcribe_audio_stream_with_control(config, request, audio, &control)
    }
}

/// Default foundation engine. It makes a missing local CLI implementation
/// explicit rather than claiming that audio has been transcribed.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableWhisperCppEngine;

impl WhisperCppEngine for UnavailableWhisperCppEngine {
    fn transcribe(
        &self,
        _config: &WhisperCppConfig,
        _request: &TranscriptionRequest,
        _audio: &mut dyn AudioStream,
    ) -> Result<Transcript, AsrError> {
        Err(AsrError::EngineUnavailable)
    }
}

/// A local provider parameterized over its concrete process or FFI engine.
/// Production can inject a tested whisper.cpp implementation without changing
/// the public `SpeechProvider` interface.
pub struct LocalProvider<E = UnavailableWhisperCppEngine> {
    config: WhisperCppConfig,
    engine: E,
}

impl<E> LocalProvider<E> {
    pub fn new(config: WhisperCppConfig, engine: E) -> Self {
        Self { config, engine }
    }

    pub fn config(&self) -> &WhisperCppConfig {
        &self.config
    }
}

impl LocalProvider<UnavailableWhisperCppEngine> {
    pub fn unavailable(config: WhisperCppConfig) -> Self {
        Self::new(config, UnavailableWhisperCppEngine)
    }
}

impl<E: WhisperCppEngine> SpeechProvider for LocalProvider<E> {
    fn name(&self) -> &'static str {
        "whisper.cpp"
    }

    fn capabilities(&self) -> SpeechCapabilities {
        SpeechCapabilities {
            local: true,
            word_timestamps: true,
            translate_to_english: true,
            // Diarization needs a separate lightweight component, so the ASR
            // engine does not claim to provide it by itself.
            lightweight_diarization: false,
        }
    }

    fn transcribe(
        &self,
        request: &TranscriptionRequest,
        audio: &mut dyn AudioStream,
    ) -> Result<Transcript, AsrError> {
        self.engine.transcribe(&self.config, request, audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LanguageMode, TranscriptionRequest};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    const TRANSCRIPTION_JSON: &[u8] = br#"
    {
      "result": { "language": "fr" },
      "transcription": [
        {
          "timestamps": { "from": "00:00:01,250", "to": "00:00:03,500" },
          "offsets": { "from": 1250, "to": 3500 },
          "text": " Bonjour, monde!",
          "tokens": [
            { "text": " Bonjour", "offsets": { "from": 1250, "to": 1850 } },
            { "text": ",", "offsets": { "from": 1850, "to": 1900 } },
            { "text": " monde", "offsets": { "from": 2000, "to": 2600 } },
            { "text": "!", "offsets": { "from": 2600, "to": 2650 } }
          ]
        }
      ]
    }
    "#;

    fn request(language_mode: LanguageMode) -> TranscriptionRequest {
        TranscriptionRequest {
            language_mode,
            word_timestamps: true,
            speaker_diarization: false,
            model: LocalModel::Small,
            quantization: Quantization::Q5Km,
            backend: ComputeBackend::Cpu,
        }
    }

    fn configured_files() -> (TempDir, WhisperCppConfig) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("whisper-cli");
        let model = directory.path().join("ggml-small-q5_k_m.bin");
        fs::write(&executable, b"test executable").unwrap();
        fs::write(&model, b"test model").unwrap();
        (
            directory,
            WhisperCppConfig {
                executable,
                model_path: model,
                model: LocalModel::Small,
                quantization: Quantization::Q5Km,
                backend: ComputeBackend::Cpu,
                thread_count: WhisperCppConfig::recommended_thread_count(),
            },
        )
    }

    #[test]
    fn discovery_prefers_the_requested_quantized_model() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("whisper-cli");
        fs::write(&executable, b"test executable").unwrap();
        let selected_model = directory.path().join("ggml-small-q5_k_m.bin");
        fs::write(&selected_model, b"test model").unwrap();
        fs::write(directory.path().join("ggml-small-q8_0.bin"), b"other model").unwrap();

        let config = WhisperCppConfig::discover(
            executable,
            directory.path(),
            LocalModel::Small,
            Quantization::Q5Km,
            ComputeBackend::Cpu,
        )
        .unwrap();

        assert_eq!(config.model_path, selected_model);
    }

    #[test]
    fn configuration_does_not_leak_paths_when_missing() {
        let config = WhisperCppConfig {
            executable: PathBuf::from("C:/very/private/whisper-cli.exe"),
            model_path: PathBuf::from("C:/very/private/model.bin"),
            model: LocalModel::Small,
            quantization: Quantization::Q5Km,
            backend: ComputeBackend::Cpu,
            thread_count: WhisperCppConfig::recommended_thread_count(),
        };
        let error = config.validate().unwrap_err();
        assert_eq!(error, WhisperCppConfigError::ExecutableMissing);
        assert!(!error.to_string().contains("very/private"));
    }

    #[test]
    fn process_plan_uses_an_argument_vector_and_requests_full_json() {
        let (_directory, config) = configured_files();
        let plan = config.cli_plan(
            Path::new("C:/cache/input.wav"),
            &request(LanguageMode::English),
            Path::new("C:/cache/output/transcription"),
        );
        assert_eq!(plan.program(), config.executable);
        assert!(plan.arguments().iter().any(|arg| arg == "-oj"));
        assert!(plan.arguments().iter().any(|arg| arg == "-ojf"));
        assert!(plan.arguments().iter().any(|arg| arg == "-of"));
        let thread_argument = plan
            .arguments()
            .windows(2)
            .find_map(|arguments| (arguments[0] == "-t").then_some(&arguments[1]));
        assert_eq!(
            thread_argument,
            Some(&OsString::from(config.thread_count.to_string()))
        );
        assert!(!format!("{plan:?}").contains("input.wav"));
        assert_eq!(
            plan.output_json_path(),
            Path::new("C:/cache/output/transcription.json")
        );
    }

    #[test]
    fn conventional_english_only_model_names_are_not_translation_capable() {
        let (_directory, mut config) = configured_files();

        config.model_path = PathBuf::from("C:/models/ggml-base.en.bin");
        assert!(config.is_english_only_model());

        config.model_path = PathBuf::from("C:/models/ggml-small.en-q5_k_m.gguf");
        assert!(config.is_english_only_model());

        config.model_path = PathBuf::from("C:/models/ggml-base.bin");
        assert!(!config.is_english_only_model());

        config.model_path = PathBuf::from("C:/models/custom-model.bin");
        assert!(!config.is_english_only_model());
    }

    #[test]
    fn automatic_thread_count_stays_within_the_browser_friendly_limit() {
        assert!((1..=MAX_AUTOMATIC_WHISPER_THREAD_COUNT)
            .contains(&WhisperCppConfig::recommended_thread_count()));
    }

    #[test]
    fn invalid_thread_count_fails_before_process_launch() {
        let (_directory, mut config) = configured_files();
        config.thread_count = 0;
        assert_eq!(
            config.validate(),
            Err(WhisperCppConfigError::InvalidThreadCount)
        );
    }

    #[test]
    fn parser_maps_word_timestamps_and_translation_metadata() {
        let transcript = parse_whisper_cpp_json(
            TRANSCRIPTION_JSON,
            &request(LanguageMode::TranslateInputToEnglish),
        )
        .unwrap();

        assert_eq!(transcript.language, "en");
        assert_eq!(transcript.translated_from.as_deref(), Some("fr"));
        assert_eq!(transcript.segments[0].timing.start_ms, 1_250);
        assert_eq!(transcript.segments[0].timing.end_ms, 3_500);
        assert_eq!(transcript.segments[0].words[0].text, "Bonjour,");
        assert_eq!(transcript.segments[0].words[0].timing.end_ms, 1_900);
        assert_eq!(transcript.segments[0].words[1].text, "monde!");
    }

    #[test]
    fn parser_synthesizes_words_when_whisper_omits_token_timestamps() {
        let json = br#"{
            "transcription": [{
                "offsets": { "from": 0, "to": 1000 },
                "text": "Speech without tokens"
            }]
        }"#;
        let transcript = parse_whisper_cpp_json(json, &request(LanguageMode::English)).unwrap();
        let words = &transcript.segments[0].words;
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "Speech");
        assert_eq!(words[0].timing.start_ms, 0);
        assert_eq!(words[2].timing.end_ms, 1_000);
    }

    #[test]
    fn parser_normalizes_overlapping_subword_timestamps() {
        let json = br#"{
            "transcription": [{
                "offsets": { "from": 0, "to": 1000 },
                "text": "hello world",
                "tokens": [
                    {"text": " hello", "offsets": { "from": 0, "to": 800 }},
                    {"text": " world", "offsets": { "from": 500, "to": 1000 }}
                ]
            }]
        }"#;
        let transcript = parse_whisper_cpp_json(json, &request(LanguageMode::English)).unwrap();
        let words = &transcript.segments[0].words;
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].timing.end_ms, words[1].timing.start_ms);
        assert_eq!(words[1].timing.end_ms, 1_000);
    }

    #[derive(Clone)]
    struct FakeRunner {
        output: Result<WhisperCppCommandOutput, WhisperCppRunnerError>,
        plans: Arc<Mutex<Vec<WhisperCppCommandPlan>>>,
    }

    impl WhisperCppCommandRunner for FakeRunner {
        fn run(
            &self,
            plan: &WhisperCppCommandPlan,
            _control: &WhisperCppExecutionControl,
        ) -> Result<WhisperCppCommandOutput, WhisperCppRunnerError> {
            self.plans.lock().unwrap().push(plan.clone());
            self.output.clone()
        }
    }

    #[test]
    fn cli_engine_uses_injected_runner_and_json_without_a_whisper_install() {
        let (_directory, config) = configured_files();
        let input_directory = tempfile::tempdir().unwrap();
        let input = input_directory.path().join("normalized.wav");
        fs::write(&input, b"fake wav").unwrap();
        let plans = Arc::new(Mutex::new(Vec::new()));
        let engine = WhisperCppCliEngine::with_runner(
            FakeRunner {
                output: Ok(WhisperCppCommandOutput {
                    succeeded: true,
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    json_output: Some(TRANSCRIPTION_JSON.to_vec()),
                }),
                plans: Arc::clone(&plans),
            },
            WhisperCppExecutionOptions::default(),
        );

        let transcript = engine
            .transcribe_file(&config, &request(LanguageMode::English), &input)
            .unwrap();

        assert_eq!(transcript.language, "fr");
        let captured = plans.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(captured[0]
            .arguments()
            .iter()
            .any(|argument| argument == "-m"));
        assert!(captured[0]
            .arguments()
            .iter()
            .any(|argument| argument == "-f"));
        assert!(!format!("{:?}", captured[0]).contains("normalized.wav"));
    }

    #[test]
    fn runner_cancellation_is_returned_as_a_typed_error() {
        let (_directory, config) = configured_files();
        let input_directory = tempfile::tempdir().unwrap();
        let input = input_directory.path().join("normalized.wav");
        fs::write(&input, b"fake wav").unwrap();
        let engine = WhisperCppCliEngine::with_runner(
            FakeRunner {
                output: Err(WhisperCppRunnerError::Cancelled),
                plans: Arc::new(Mutex::new(Vec::new())),
            },
            WhisperCppExecutionOptions::default(),
        );

        let error = engine
            .transcribe_file(&config, &request(LanguageMode::English), &input)
            .unwrap_err();
        assert_eq!(error, AsrError::Cancelled);
    }
}
