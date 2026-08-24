use crate::{
    export_srt, export_timestamped_txt, export_transcript_json, export_txt, export_vtt,
    SubtitleExportError,
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use subtitler_core::{JobId, SubtitleCue, Transcript};
use thiserror::Error;
use uuid::Uuid;

/// The five user-facing export files generated for a completed transcript.
/// Paths are local engine details and are intentionally not serialized into
/// native-messaging status messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportBundle {
    pub directory: PathBuf,
    pub transcript_txt: PathBuf,
    pub timestamped_txt: PathBuf,
    pub subtitles_srt: PathBuf,
    pub subtitles_vtt: PathBuf,
    pub transcript_json: PathBuf,
}

/// Write all V1 transcript exports into a new job-specific directory.
///
/// The caller owns `root` (normally a private per-user engine directory). A
/// UUID-based directory prevents an old transcript from being overwritten,
/// and every file is first written to a same-directory temporary file before
/// it is atomically renamed into place. Transcript text is never included in
/// an error value.
pub fn write_export_bundle(
    root: &Path,
    job_id: &JobId,
    transcript: &Transcript,
    cues: &[SubtitleCue],
) -> Result<ExportBundle, ExportFileError> {
    transcript
        .validate()
        .map_err(|error| ExportFileError::InvalidTranscript(error.to_string()))?;

    fs::create_dir_all(root).map_err(|error| ExportFileError::CreateRoot(error.kind()))?;
    let directory = root.join(job_id.to_string());
    fs::create_dir(&directory)
        .map_err(|error| ExportFileError::CreateJobDirectory(error.kind()))?;

    let result = (|| {
        let transcript_txt = write_named(&directory, "Transcript.txt", &export_txt(transcript))?;
        let timestamped_txt = write_named(
            &directory,
            "Transcript-timestamped.txt",
            &export_timestamped_txt(transcript),
        )?;
        let subtitles_srt = write_named(
            &directory,
            "Subtitles.srt",
            &export_srt(cues).map_err(ExportFileError::Export)?,
        )?;
        let subtitles_vtt = write_named(
            &directory,
            "Subtitles.vtt",
            &export_vtt(cues).map_err(ExportFileError::Export)?,
        )?;
        let transcript_json = write_named(
            &directory,
            "Transcript.json",
            &export_transcript_json(transcript).map_err(ExportFileError::Export)?,
        )?;
        Ok(ExportBundle {
            directory: directory.clone(),
            transcript_txt,
            timestamped_txt,
            subtitles_srt,
            subtitles_vtt,
            transcript_json,
        })
    })();

    if result.is_err() {
        // A partial export is not useful and could mislead a user into
        // believing a transcript completed. The directory is job-specific,
        // so cleanup cannot affect unrelated user files.
        let _ = fs::remove_dir_all(&directory);
    }
    result
}

fn write_named(directory: &Path, name: &str, contents: &str) -> Result<PathBuf, ExportFileError> {
    let destination = directory.join(name);
    let temporary = directory.join(format!(".{name}.{}.tmp", Uuid::new_v4()));
    write_atomic(&temporary, &destination, contents.as_bytes())?;
    Ok(destination)
}

fn write_atomic(
    temporary: &Path,
    destination: &Path,
    contents: &[u8],
) -> Result<(), ExportFileError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .map_err(|error| ExportFileError::Write(error.kind()))?;
    let write_result = (|| {
        file.write_all(contents)
            .map_err(|error| ExportFileError::Write(error.kind()))?;
        file.sync_all()
            .map_err(|error| ExportFileError::Write(error.kind()))?;
        drop(file);
        fs::rename(temporary, destination).map_err(|error| ExportFileError::Finalize(error.kind()))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    write_result
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExportFileError {
    #[error("Subtitler could not validate this transcript before export: {0}")]
    InvalidTranscript(String),
    #[error("Subtitler could not create its private export directory ({0:?}).")]
    CreateRoot(std::io::ErrorKind),
    #[error("Subtitler could not create this job's export directory ({0:?}).")]
    CreateJobDirectory(std::io::ErrorKind),
    #[error("Subtitler could not write an export file ({0:?}).")]
    Write(std::io::ErrorKind),
    #[error("Subtitler could not finalize an export file ({0:?}).")]
    Finalize(std::io::ErrorKind),
    #[error("Subtitler could not format an export: {0}")]
    Export(SubtitleExportError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use subtitler_core::{SubtitleCue, TimeRange, TranscriptSegment, WordTimestamp};

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("subtitler-export-test-{}", Uuid::new_v4()))
    }

    fn transcript() -> Transcript {
        let timing = TimeRange::new(0, 1_000).unwrap();
        Transcript {
            language: "en".to_owned(),
            translated_from: None,
            segments: vec![TranscriptSegment {
                timing,
                text: "Hello Subtitler.".to_owned(),
                speaker: None,
                words: vec![WordTimestamp {
                    text: "Hello".to_owned(),
                    timing,
                    speaker: None,
                }],
            }],
        }
    }

    #[test]
    fn writes_the_complete_v1_export_bundle_without_overwriting_a_job() {
        let root = test_root();
        let job_id = JobId::new();
        let cues = vec![SubtitleCue {
            timing: TimeRange::new(0, 1_000).unwrap(),
            lines: vec!["Hello Subtitler.".to_owned()],
            speaker: None,
        }];

        let bundle = write_export_bundle(&root, &job_id, &transcript(), &cues).unwrap();
        assert_eq!(bundle.directory, root.join(job_id.to_string()));
        assert_eq!(
            fs::read_to_string(&bundle.transcript_txt).unwrap(),
            "Hello Subtitler."
        );
        assert!(fs::read_to_string(&bundle.timestamped_txt)
            .unwrap()
            .contains("[00:00:00.000]"));
        assert!(fs::read_to_string(&bundle.subtitles_srt)
            .unwrap()
            .contains("00:00:00,000 --> 00:00:01,000"));
        assert!(fs::read_to_string(&bundle.subtitles_vtt)
            .unwrap()
            .starts_with("WEBVTT"));
        assert!(fs::read_to_string(&bundle.transcript_json)
            .unwrap()
            .contains("Hello Subtitler"));

        let second = write_export_bundle(&root, &job_id, &transcript(), &cues);
        assert!(matches!(
            second,
            Err(ExportFileError::CreateJobDirectory(_))
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_partial_exports_after_an_invalid_subtitle_payload() {
        let root = test_root();
        let job_id = JobId::new();
        let invalid_cues = vec![SubtitleCue {
            timing: TimeRange::new(0, 1_000).unwrap(),
            lines: Vec::new(),
            speaker: None,
        }];

        let error = write_export_bundle(&root, &job_id, &transcript(), &invalid_cues).unwrap_err();
        assert!(matches!(error, ExportFileError::Export(_)));
        assert!(!root.join(job_id.to_string()).exists());

        if root.exists() {
            fs::remove_dir_all(root).unwrap();
        }
    }
}
