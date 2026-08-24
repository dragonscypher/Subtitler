use crate::{JobFailure, JobState, JobStatus};
use thiserror::Error;

/// The transition rules are centralized so scheduler, host, and future UI
/// adapters cannot accidentally resurrect a cancelled or completed job.
pub fn transition_job(
    status: &mut JobStatus,
    next: JobState,
    message: Option<String>,
    failure: Option<JobFailure>,
) -> Result<(), JobTransitionError> {
    if !is_valid_transition(status.state, next) {
        return Err(JobTransitionError::Invalid {
            from: status.state,
            to: next,
        });
    }

    if next == JobState::Failed && failure.is_none() {
        return Err(JobTransitionError::MissingFailure);
    }
    if next != JobState::Failed && failure.is_some() {
        return Err(JobTransitionError::UnexpectedFailure);
    }

    status.state = next;
    status.message = message;
    status.failure = failure;
    Ok(())
}

pub fn is_valid_transition(from: JobState, to: JobState) -> bool {
    use JobState::*;
    matches!(
        (from, to),
        (Queued, Discovering)
            | (Queued, Cancelled)
            | (Queued, Stale)
            | (Queued, Failed)
            | (Discovering, Acquiring)
            | (Discovering, Processing)
            | (Discovering, Cancelled)
            | (Discovering, Stale)
            | (Discovering, Failed)
            | (Acquiring, Processing)
            | (Acquiring, Cancelled)
            | (Acquiring, Stale)
            | (Acquiring, Failed)
            | (Processing, Completed)
            | (Processing, Cancelled)
            | (Processing, Stale)
            | (Processing, Failed)
    )
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum JobTransitionError {
    #[error("cannot transition a job from {from:?} to {to:?}")]
    Invalid { from: JobState, to: JobState },
    #[error("a failed job must include a user-safe failure")]
    MissingFailure,
    #[error("only failed jobs may include a failure")]
    UnexpectedFailure,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobId, JobKind};

    #[test]
    fn terminal_jobs_cannot_be_restarted() {
        let mut status = JobStatus::queued(JobId::new(), JobKind::FullTranscript, None);
        transition_job(
            &mut status,
            JobState::Cancelled,
            Some("Cancelled by user.".to_owned()),
            None,
        )
        .unwrap();

        let error = transition_job(&mut status, JobState::Processing, None, None).unwrap_err();
        assert!(matches!(error, JobTransitionError::Invalid { .. }));
    }
}
