use serde::Serialize;
use std::fmt::Write;
use subtitler_core::{SubtitleCue, TimeRange, Transcript};
use thiserror::Error;

/// Export a simple human-readable transcript. Speaker labels are included only
/// when a source adapter or diarization component supplied them.
pub fn export_txt(transcript: &Transcript) -> String {
    transcript
        .segments
        .iter()
        .map(|segment| match &segment.speaker {
            Some(speaker) => format!("{speaker}: {}", segment.text.trim()),
            None => segment.text.trim().to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn export_timestamped_txt(transcript: &Transcript) -> String {
    transcript
        .segments
        .iter()
        .map(|segment| {
            let prefix = format_timestamp(segment.timing.start_ms, '.');
            match &segment.speaker {
                Some(speaker) => format!("[{prefix}] {speaker}: {}", segment.text.trim()),
                None => format!("[{prefix}] {}", segment.text.trim()),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn export_srt(cues: &[SubtitleCue]) -> Result<String, SubtitleExportError> {
    validate_cues(cues)?;
    let mut output = String::new();
    for (index, cue) in cues.iter().enumerate() {
        writeln!(output, "{}", index + 1).expect("writing to String cannot fail");
        writeln!(
            output,
            "{} --> {}",
            format_timestamp(cue.timing.start_ms, ','),
            format_timestamp(cue.timing.end_ms, ',')
        )
        .expect("writing to String cannot fail");
        for line in &cue.lines {
            writeln!(output, "{}", line.trim()).expect("writing to String cannot fail");
        }
        output.push('\n');
    }
    Ok(output)
}

pub fn export_vtt(cues: &[SubtitleCue]) -> Result<String, SubtitleExportError> {
    validate_cues(cues)?;
    let mut output = String::from("WEBVTT\n\n");
    for cue in cues {
        writeln!(
            output,
            "{} --> {}",
            format_timestamp(cue.timing.start_ms, '.'),
            format_timestamp(cue.timing.end_ms, '.')
        )
        .expect("writing to String cannot fail");
        for line in &cue.lines {
            writeln!(output, "{}", line.trim()).expect("writing to String cannot fail");
        }
        output.push('\n');
    }
    Ok(output)
}

/// Pretty JSON export preserves timestamps, word timestamps, language, and
/// any verified speaker metadata for downstream use.
pub fn export_transcript_json(transcript: &Transcript) -> Result<String, SubtitleExportError> {
    export_json(transcript)
}

pub fn export_subtitles_json(cues: &[SubtitleCue]) -> Result<String, SubtitleExportError> {
    validate_cues(cues)?;
    export_json(cues)
}

fn export_json(value: &(impl Serialize + ?Sized)) -> Result<String, SubtitleExportError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| SubtitleExportError::Json(error.to_string()))
}

fn validate_cues(cues: &[SubtitleCue]) -> Result<(), SubtitleExportError> {
    let mut previous_end = 0;
    for cue in cues {
        validate_timing(cue.timing)?;
        if cue.timing.start_ms < previous_end {
            return Err(SubtitleExportError::OverlappingCues);
        }
        if cue.lines.is_empty() || cue.lines.iter().any(|line| line.trim().is_empty()) {
            return Err(SubtitleExportError::EmptyCueText);
        }
        if cue
            .lines
            .iter()
            .any(|line| line.contains('\n') || line.contains('\r'))
        {
            return Err(SubtitleExportError::EmbeddedLineBreak);
        }
        previous_end = cue.timing.end_ms;
    }
    Ok(())
}

fn validate_timing(timing: TimeRange) -> Result<(), SubtitleExportError> {
    if timing.end_ms < timing.start_ms {
        return Err(SubtitleExportError::InvalidTiming);
    }
    Ok(())
}

fn format_timestamp(milliseconds: u64, separator: char) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds % 3_600_000) / 60_000;
    let seconds = (milliseconds % 60_000) / 1_000;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}{separator}{millis:03}")
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SubtitleExportError {
    #[error("a subtitle cue has invalid timestamps")]
    InvalidTiming,
    #[error("subtitle cues must be ordered and non-overlapping")]
    OverlappingCues,
    #[error("a subtitle cue must have visible text")]
    EmptyCueText,
    #[error("subtitle cue lines cannot contain embedded line breaks")]
    EmbeddedLineBreak,
    #[error("could not create JSON export: {0}")]
    Json(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use subtitler_core::{SubtitleCue, TimeRange, Transcript, TranscriptSegment};

    fn cue(start_ms: u64, end_ms: u64, line: &str) -> SubtitleCue {
        SubtitleCue {
            timing: TimeRange::new(start_ms, end_ms).unwrap(),
            lines: vec![line.to_owned()],
            speaker: None,
        }
    }

    #[test]
    fn srt_and_vtt_use_their_required_timestamp_separators() {
        let cues = vec![cue(1_234, 5_678, "Hello world.")];
        let srt = export_srt(&cues).unwrap();
        let vtt = export_vtt(&cues).unwrap();

        assert_eq!(srt, "1\n00:00:01,234 --> 00:00:05,678\nHello world.\n\n");
        assert_eq!(
            vtt,
            "WEBVTT\n\n00:00:01.234 --> 00:00:05.678\nHello world.\n\n"
        );
    }

    #[test]
    fn text_exports_preserve_speaker_and_timestamp_information() {
        let transcript = Transcript {
            language: "en".to_owned(),
            translated_from: None,
            segments: vec![TranscriptSegment {
                timing: TimeRange::new(62_000, 64_000).unwrap(),
                text: "We can ship Friday.".to_owned(),
                speaker: Some("Sarah".to_owned()),
                words: Vec::new(),
            }],
        };
        assert_eq!(export_txt(&transcript), "Sarah: We can ship Friday.");
        assert_eq!(
            export_timestamped_txt(&transcript),
            "[00:01:02.000] Sarah: We can ship Friday."
        );
        assert!(export_transcript_json(&transcript)
            .unwrap()
            .contains("ship Friday"));
    }
}
