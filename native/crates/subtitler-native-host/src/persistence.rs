//! Private, metadata-only checkpoints for Native Messaging job recovery.
//!
//! These files deliberately contain no media source, URL, cookie, token,
//! transcript text, cue text, or local media path. Completed transcript text
//! remains in the existing job-private export bundle and is reopened only when
//! the browser reattaches the matching opaque native job identifier.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use subtitler_core::{JobId, JobKind, JobStatus, Transcript};
use subtitler_subtitles::ExportBundle;

const CHECKPOINT_VERSION: u8 = 1;
const TRANSCRIPT_FILE: &str = "Transcript.json";

#[derive(Clone)]
pub struct JobPersistence {
    checkpoint_root: PathBuf,
    export_root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedJob {
    pub(crate) version: u8,
    pub(crate) job_id: JobId,
    #[serde(default)]
    pub(crate) client_job_id: Option<String>,
    pub(crate) kind: JobKind,
    pub(crate) status: JobStatus,
}

pub struct RestoredOutcome {
    pub transcript: Transcript,
    pub exports: ExportBundle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    Missing,
    Invalid,
}

impl JobPersistence {
    pub fn new(checkpoint_root: PathBuf, export_root: PathBuf) -> Self {
        Self {
            checkpoint_root,
            export_root,
        }
    }

    pub fn write(&self, client_job_id: Option<&str>, kind: JobKind, status: &JobStatus) {
        let record = PersistedJob {
            version: CHECKPOINT_VERSION,
            job_id: status.job_id.clone(),
            client_job_id: client_job_id.map(str::to_owned),
            kind,
            status: status.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&record) else {
            return;
        };
        if fs::create_dir_all(&self.checkpoint_root).is_err() {
            return;
        }
        // This is small operational metadata. A completed export remains the
        // recovery source of truth if a power loss interrupts this write.
        let _ = fs::write(self.checkpoint_path(&record.job_id), bytes);
    }

    pub fn load(&self, job_id: &JobId) -> Option<PersistedJob> {
        let bytes = fs::read(self.checkpoint_path(job_id)).ok()?;
        let record = serde_json::from_slice::<PersistedJob>(&bytes).ok()?;
        (record.version == CHECKPOINT_VERSION && record.job_id == *job_id).then_some(record)
    }

    pub fn restore_outcome(&self, job_id: &JobId) -> Result<RestoredOutcome, RestoreError> {
        let directory = self.export_root.join(job_id.to_string());
        let transcript_path = directory.join(TRANSCRIPT_FILE);
        let bytes = fs::read(transcript_path).map_err(|_| RestoreError::Missing)?;
        let transcript =
            serde_json::from_slice::<Transcript>(&bytes).map_err(|_| RestoreError::Invalid)?;
        transcript.validate().map_err(|_| RestoreError::Invalid)?;
        let exports = export_bundle_from_directory(&directory).ok_or(RestoreError::Missing)?;
        Ok(RestoredOutcome {
            transcript,
            exports,
        })
    }

    fn checkpoint_path(&self, job_id: &JobId) -> PathBuf {
        self.checkpoint_root.join(format!("{job_id}.json"))
    }
}

fn export_bundle_from_directory(directory: &Path) -> Option<ExportBundle> {
    let transcript_txt = directory.join("Transcript.txt");
    let timestamped_txt = directory.join("Transcript-timestamped.txt");
    let subtitles_srt = directory.join("Subtitles.srt");
    let subtitles_vtt = directory.join("Subtitles.vtt");
    let transcript_json = directory.join(TRANSCRIPT_FILE);
    let complete = [
        &transcript_txt,
        &timestamped_txt,
        &subtitles_srt,
        &subtitles_vtt,
        &transcript_json,
    ]
    .into_iter()
    .all(|path| path.is_file());
    complete.then_some(ExportBundle {
        directory: directory.to_owned(),
        transcript_txt,
        timestamped_txt,
        subtitles_srt,
        subtitles_vtt,
        transcript_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use subtitler_core::{
        JobProgress, JobState, SubtitleCue, TimeRange, TranscriptSegment, WordTimestamp,
    };
    use subtitler_subtitles::{segment_words, write_export_bundle, SubtitleSegmentationConfig};

    #[test]
    fn checkpoint_round_trip_contains_no_transcript_or_source_fields() {
        let root = tempfile::tempdir().unwrap();
        let persistence =
            JobPersistence::new(root.path().join("jobs"), root.path().join("exports"));
        let job_id = JobId::new();
        let status = JobStatus {
            job_id: job_id.clone(),
            kind: JobKind::FullTranscript,
            state: JobState::Processing,
            progress: JobProgress::default(),
            message: Some("Creating a timestamped local transcript.".to_owned()),
            failure: None,
        };
        persistence.write(Some("client-id"), JobKind::FullTranscript, &status);
        let restored = persistence.load(&job_id).unwrap();
        assert_eq!(restored.kind, JobKind::FullTranscript);
        assert_eq!(restored.status, status);
        let bytes = fs::read(root.path().join("jobs").join(format!("{job_id}.json"))).unwrap();
        let serialized = String::from_utf8(bytes).unwrap();
        assert!(!serialized.contains("https://"));
        assert!(!serialized.contains("Transcript text"));
    }

    #[test]
    fn completed_export_bundle_can_be_reopened_without_a_source_request() {
        let root = tempfile::tempdir().unwrap();
        let persistence =
            JobPersistence::new(root.path().join("jobs"), root.path().join("exports"));
        let job_id = JobId::new();
        let timing = TimeRange::new(0, 1_000).unwrap();
        let transcript = Transcript {
            language: "en".to_owned(),
            translated_from: None,
            segments: vec![TranscriptSegment {
                timing,
                text: "Safe test transcript.".to_owned(),
                speaker: None,
                words: vec![WordTimestamp {
                    text: "Safe".to_owned(),
                    timing,
                    speaker: None,
                }],
            }],
        };
        let words = transcript
            .segments
            .iter()
            .flat_map(|segment| segment.words.iter().cloned())
            .collect::<Vec<_>>();
        let cues: Vec<SubtitleCue> =
            segment_words(&words, &SubtitleSegmentationConfig::default()).unwrap();
        write_export_bundle(&root.path().join("exports"), &job_id, &transcript, &cues).unwrap();

        let restored = persistence.restore_outcome(&job_id).unwrap();
        assert_eq!(restored.transcript, transcript);
        assert!(restored.exports.transcript_json.is_file());
    }
}
