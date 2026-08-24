use crate::{ComputeBackend, LocalModel, Quantization};
use serde::{Deserialize, Serialize};
use subtitler_core::Transcript;
use subtitler_media::AudioStream;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageMode {
    /// English input produces an English transcript and subtitles.
    English,
    /// Non-English input is transcribed and translated to English in V1.
    #[default]
    TranslateInputToEnglish,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionRequest {
    #[serde(default)]
    pub language_mode: LanguageMode,
    #[serde(default = "default_word_timestamps")]
    pub word_timestamps: bool,
    #[serde(default)]
    pub speaker_diarization: bool,
    pub model: LocalModel,
    pub quantization: Quantization,
    #[serde(default)]
    pub backend: ComputeBackend,
}

fn default_word_timestamps() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechCapabilities {
    pub local: bool,
    pub word_timestamps: bool,
    pub translate_to_english: bool,
    pub lightweight_diarization: bool,
}

/// Common interface for a local ASR implementation. Its input is normalized
/// PCM rather than an arbitrary URL, keeping media acquisition separate from
/// speech recognition.
pub trait SpeechProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> SpeechCapabilities;
    fn transcribe(
        &self,
        request: &TranscriptionRequest,
        audio: &mut dyn AudioStream,
    ) -> Result<Transcript, AsrError>;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AsrError {
    #[error("The local speech engine is not installed or linked yet.")]
    EngineUnavailable,
    #[error("The requested local ASR configuration is invalid: {0}")]
    InvalidConfiguration(String),
    #[error("Local speech processing was cancelled.")]
    Cancelled,
    #[error("The local speech engine did not finish within {timeout_ms} ms.")]
    TimedOut { timeout_ms: u64 },
    #[error("The local speech engine did not produce valid timestamped output: {0}")]
    InvalidOutput(String),
    #[error("The local speech engine could not process this audio: {0}")]
    ProcessingFailed(String),
}
