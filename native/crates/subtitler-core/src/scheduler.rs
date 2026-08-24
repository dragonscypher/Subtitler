//! Deterministic scheduling policy for prerecorded-media subtitle buffering.
//!
//! This module deliberately has no clock, async runtime, worker queue, or
//! media dependency. The native host supplies playback snapshots and measured
//! processing samples, asks for the next range, and reports completed ranges.
//! Keeping the policy pure makes seek preemption and pacing decisions easy to
//! test and keeps a full-transcript job independent of browser playback.

use crate::{JobKind, TimeRange};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Conservative initial buffer targets. The host should replace these with
/// benchmark-informed settings for the selected model and hardware backend.
pub const DEFAULT_MINIMUM_AHEAD_MS: u64 = 30_000;
pub const DEFAULT_PREFERRED_AHEAD_MS: u64 = 120_000;
pub const DEFAULT_MAXIMUM_AHEAD_MS: u64 = 300_000;
pub const DEFAULT_PROCESSING_CHUNK_MS: u64 = 30_000;
pub const DEFAULT_CONTEXT_BEFORE_PLAYHEAD_MS: u64 = 5_000;

const FAST_PROCESSING_EFFECTIVE_RTF: f64 = 0.5;
const AT_RISK_EFFECTIVE_RTF: f64 = 0.85;

/// The type of work a scheduler is coordinating.
///
/// A full transcript has deliberately no dependency on playback position,
/// seeks, or subtitle lead. It always works from the earliest uncovered media
/// range. Subtitle buffering instead concentrates work near the current
/// playhead and preempts obsolete work when the seek generation advances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingMode {
    SubtitleBuffer,
    FullTranscript,
}

impl From<JobKind> for SchedulingMode {
    fn from(kind: JobKind) -> Self {
        match kind {
            JobKind::SubtitleGeneration => Self::SubtitleBuffer,
            JobKind::FullTranscript => Self::FullTranscript,
        }
    }
}

/// The lead a subtitle job should try to keep fully processed beyond the
/// playhead. Values are media-time milliseconds, not wall-clock time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleBufferTargets {
    pub minimum_ahead_ms: u64,
    pub preferred_ahead_ms: u64,
    pub maximum_ahead_ms: u64,
}

impl Default for SubtitleBufferTargets {
    fn default() -> Self {
        Self {
            minimum_ahead_ms: DEFAULT_MINIMUM_AHEAD_MS,
            preferred_ahead_ms: DEFAULT_PREFERRED_AHEAD_MS,
            maximum_ahead_ms: DEFAULT_MAXIMUM_AHEAD_MS,
        }
    }
}

/// Host-selected policy knobs. There is no timing loop hidden in this type:
/// callers decide when to request the next range and may use a single worker
/// or a bounded local worker pool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleSchedulerConfig {
    pub targets: SubtitleBufferTargets,
    /// Maximum source-media duration assigned to one transcription task.
    pub processing_chunk_ms: u64,
    /// Small context around a newly observed playhead so a cue that began just
    /// before it can still be generated after a seek.
    pub context_before_playhead_ms: u64,
    /// Upper bound on ranges leased by [`SubtitleBufferScheduler`] and not yet
    /// completed or released.
    pub max_in_flight_ranges: usize,
}

impl Default for SubtitleSchedulerConfig {
    fn default() -> Self {
        Self {
            targets: SubtitleBufferTargets::default(),
            processing_chunk_ms: DEFAULT_PROCESSING_CHUNK_MS,
            context_before_playhead_ms: DEFAULT_CONTEXT_BEFORE_PLAYHEAD_MS,
            max_in_flight_ranges: 1,
        }
    }
}

/// A browser-supplied snapshot. `seek_generation` must increase for every
/// discrete seek. Repeated ordinary playback updates retain the same value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybackUpdate {
    pub position_ms: u64,
    pub playback_rate: f64,
    pub is_playing: bool,
    pub seek_generation: u64,
}

impl PlaybackUpdate {
    pub fn initial() -> Self {
        Self {
            position_ms: 0,
            playback_rate: 1.0,
            is_playing: false,
            seek_generation: 0,
        }
    }
}

/// The relative importance of a requested range. The host can use this value
/// if it has more than one worker, but this scheduler already emits ranges in
/// the preferred order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangePriority {
    /// Rebuild the minimum usable lead after an explicit seek.
    SeekRecovery,
    /// The current processed lead is below the configured minimum.
    MinimumBuffer,
    /// The current processed lead is below the preferred target.
    PreferredBuffer,
    /// Additional useful prefetch up to the adaptive maximum target.
    Prefetch,
    /// Playback-independent transcription from the earliest uncovered range.
    FullTranscript,
}

/// A lease for a non-overlapping media range. Completion is reported using
/// `reservation_id`; the same source range can still be recorded as processed
/// later if a preempted worker finishes after its cancellation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledRange {
    pub reservation_id: u64,
    pub timing: TimeRange,
    pub priority: RangePriority,
    pub seek_generation: u64,
}

/// Whether a playback update changed scheduling state, or was intentionally
/// ignored because it was stale or the job is a full transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackUpdateDisposition {
    Applied,
    IgnoredStaleGeneration,
    IgnoredForFullTranscript,
}

/// Result of accepting a browser playback update. The host should cancel work
/// represented by `preempted` when practical. If such work still completes,
/// its valid completed coverage may be recorded with
/// [`SubtitleBufferScheduler::record_processed_range`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackUpdateResult {
    pub disposition: PlaybackUpdateDisposition,
    pub preempted: Vec<ScheduledRange>,
}

/// A user-facing assessment of whether local processing can sustain the
/// current playback rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitlePacingState {
    /// Full transcripts do not depend on a playback buffer.
    FullTranscriptIndependent,
    /// Playback is paused, so the engine can build a larger cushion.
    PlaybackPaused,
    /// A playback position is known but no throughput sample is available.
    Measuring,
    /// Measured throughput is comfortably ahead of playback.
    KeepingUp,
    /// Throughput is close enough to playback that a larger buffer is useful.
    AtRisk,
    /// Throughput is slower than playback but existing lead remains above the
    /// minimum. The user should be informed before the cushion is exhausted.
    CannotKeepUp,
    /// Throughput is slower than playback and the usable lead is below the
    /// configured minimum, so briefly pausing playback is recommended.
    PauseRecommended,
}

/// A compact status snapshot for job progress reporting and UI decisions.
/// `processing_real_time_factor` is wall-clock processing milliseconds divided
/// by source-audio milliseconds: values below 1.0 are faster than real time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubtitleSchedulerStatus {
    pub mode: SchedulingMode,
    pub media_duration_ms: u64,
    pub playback: PlaybackUpdate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_buffer_ahead_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_buffer_ahead_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing_real_time_factor: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_real_time_factor: Option<f64>,
    pub pacing: SubtitlePacingState,
    pub in_flight_ranges: usize,
}

/// Errors returned for invalid, caller-supplied scheduler state. The library
/// never inspects a wall clock or starts work itself.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("media duration must be greater than zero")]
    ZeroMediaDuration,
    #[error("subtitle buffer targets must satisfy minimum <= preferred <= maximum")]
    InvalidBufferTargets,
    #[error("processing chunk duration must be greater than zero")]
    ZeroProcessingChunk,
    #[error("at least one in-flight processing range must be allowed")]
    ZeroInFlightCapacity,
    #[error("playback rate must be finite and greater than zero")]
    InvalidPlaybackRate,
    #[error(
        "playback position ({position_ms} ms) exceeds media duration ({media_duration_ms} ms)"
    )]
    PlaybackPositionOutsideMedia {
        position_ms: u64,
        media_duration_ms: u64,
    },
    #[error("processing sample must cover a non-zero amount of source audio")]
    EmptyProcessingSample,
    #[error("range ({start_ms}..{end_ms} ms) is empty or outside the media duration ({media_duration_ms} ms)")]
    InvalidProcessedRange {
        start_ms: u64,
        end_ms: u64,
        media_duration_ms: u64,
    },
    #[error("unknown processing reservation {reservation_id}")]
    UnknownReservation { reservation_id: u64 },
}

/// Pure, deterministic policy for a prerecorded-media job. It has no async
/// runtime dependency: a host drives it by feeding snapshots, measured samples
/// and completed ranges.
#[derive(Clone, Debug)]
pub struct SubtitleBufferScheduler {
    mode: SchedulingMode,
    media_duration_ms: u64,
    config: SubtitleSchedulerConfig,
    playback: PlaybackUpdate,
    /// `PlaybackUpdate::initial` is an internal default, not an observation
    /// from the page. The first actual snapshot may place the user far from
    /// zero while the host has already leased an initial range.
    has_received_playback_update: bool,
    processed: RangeSet,
    in_flight: Vec<ScheduledRange>,
    next_reservation_id: u64,
    seek_recovery_generation: Option<u64>,
    processed_audio_ms: u64,
    processing_wall_ms: u64,
}

impl SubtitleBufferScheduler {
    pub fn new(
        mode: SchedulingMode,
        media_duration_ms: u64,
        config: SubtitleSchedulerConfig,
    ) -> Result<Self, SchedulerError> {
        validate_config(media_duration_ms, &config)?;

        Ok(Self {
            mode,
            media_duration_ms,
            config,
            playback: PlaybackUpdate::initial(),
            has_received_playback_update: false,
            processed: RangeSet::default(),
            in_flight: Vec::new(),
            next_reservation_id: 1,
            seek_recovery_generation: None,
            processed_audio_ms: 0,
            processing_wall_ms: 0,
        })
    }

    pub fn mode(&self) -> SchedulingMode {
        self.mode
    }

    pub fn media_duration_ms(&self) -> u64 {
        self.media_duration_ms
    }

    pub fn playback(&self) -> &PlaybackUpdate {
        &self.playback
    }

    /// Reports compact, non-content scheduling status. For subtitle jobs,
    /// `subtitle_buffer_ahead_ms` measures contiguous *completed* coverage
    /// from the playhead; leased work does not count as usable subtitle lead.
    pub fn status(&self) -> SubtitleSchedulerStatus {
        let processing_real_time_factor = self.processing_real_time_factor();
        let (subtitle_buffer_ahead_ms, target_buffer_ahead_ms, effective_real_time_factor, pacing) =
            match self.mode {
                SchedulingMode::FullTranscript => (
                    None,
                    None,
                    None,
                    SubtitlePacingState::FullTranscriptIndependent,
                ),
                SchedulingMode::SubtitleBuffer => {
                    let buffer_ahead_ms = self.processed_lead_ms();
                    let effective = processing_real_time_factor
                        .map(|factor| factor * self.playback.playback_rate);
                    let pacing = self.subtitle_pacing(buffer_ahead_ms, effective);
                    (
                        Some(buffer_ahead_ms),
                        Some(self.target_buffer_ahead_ms()),
                        effective,
                        pacing,
                    )
                }
            };

        SubtitleSchedulerStatus {
            mode: self.mode,
            media_duration_ms: self.media_duration_ms,
            playback: self.playback.clone(),
            subtitle_buffer_ahead_ms,
            target_buffer_ahead_ms,
            processing_real_time_factor,
            effective_real_time_factor,
            pacing,
            in_flight_ranges: self.in_flight.len(),
        }
    }

    /// Applies a playback snapshot. A higher seek generation preempts all
    /// outstanding subtitle-range leases so the host can prioritize the new
    /// playhead. The first real snapshot also preempts an already leased
    /// default-zero range when it places the user elsewhere in the recording.
    /// A lower generation is stale and does not change state.
    pub fn update_playback(
        &mut self,
        update: PlaybackUpdate,
    ) -> Result<PlaybackUpdateResult, SchedulerError> {
        if self.mode == SchedulingMode::FullTranscript {
            return Ok(PlaybackUpdateResult {
                disposition: PlaybackUpdateDisposition::IgnoredForFullTranscript,
                preempted: Vec::new(),
            });
        }

        self.validate_playback(&update)?;

        if update.seek_generation < self.playback.seek_generation {
            return Ok(PlaybackUpdateResult {
                disposition: PlaybackUpdateDisposition::IgnoredStaleGeneration,
                preempted: Vec::new(),
            });
        }

        let is_initial_reposition =
            !self.has_received_playback_update && update.position_ms != self.playback.position_ms;
        let is_new_seek = update.seek_generation > self.playback.seek_generation;
        self.playback = update;
        self.has_received_playback_update = true;

        let preempted = if is_new_seek || is_initial_reposition {
            self.seek_recovery_generation = Some(self.playback.seek_generation);
            std::mem::take(&mut self.in_flight)
        } else {
            Vec::new()
        };
        self.clear_seek_recovery_if_buffered();

        Ok(PlaybackUpdateResult {
            disposition: PlaybackUpdateDisposition::Applied,
            preempted,
        })
    }

    /// Adds one measured processing sample. `source_audio_ms` is the amount of
    /// media processed, while `wall_elapsed_ms` is the wall-clock cost of that
    /// work. The scheduler only aggregates caller-supplied measurements; it
    /// never reads system time itself.
    pub fn record_processing_sample(
        &mut self,
        source_audio_ms: u64,
        wall_elapsed_ms: u64,
    ) -> Result<(), SchedulerError> {
        if source_audio_ms == 0 {
            return Err(SchedulerError::EmptyProcessingSample);
        }

        self.processed_audio_ms = self.processed_audio_ms.saturating_add(source_audio_ms);
        self.processing_wall_ms = self.processing_wall_ms.saturating_add(wall_elapsed_ms);
        Ok(())
    }

    /// Returns the next non-overlapping source range to process and reserves
    /// it. `None` means the adaptive target is fully covered, the media is
    /// complete, or all allowed workers already have a range leased.
    pub fn next_processing_range(&mut self) -> Option<ScheduledRange> {
        if self.in_flight.len() >= self.config.max_in_flight_ranges {
            return None;
        }

        let (window_start_ms, window_end_ms, priority) = match self.mode {
            SchedulingMode::FullTranscript => {
                (0, self.media_duration_ms, RangePriority::FullTranscript)
            }
            SchedulingMode::SubtitleBuffer => self.subtitle_window()?,
        };

        let free_range = self.first_unoccupied_range(window_start_ms, window_end_ms)?;
        let end_ms = free_range
            .start_ms
            .saturating_add(self.config.processing_chunk_ms)
            .min(free_range.end_ms);
        if end_ms <= free_range.start_ms {
            return None;
        }

        let scheduled = ScheduledRange {
            reservation_id: self.next_reservation_id,
            timing: TimeRange {
                start_ms: free_range.start_ms,
                end_ms,
            },
            priority,
            seek_generation: self.playback.seek_generation,
        };
        self.next_reservation_id = self.next_reservation_id.saturating_add(1);
        self.in_flight.push(scheduled.clone());
        Some(scheduled)
    }

    /// Marks a leased range fully processed, making its timestamps usable for
    /// subtitle lead calculations. A worker that finishes after seek
    /// preemption should instead use [`Self::record_processed_range`] because
    /// its lease has already been returned to the host as preempted.
    pub fn complete_processing_range(
        &mut self,
        reservation_id: u64,
    ) -> Result<TimeRange, SchedulerError> {
        let index = self
            .in_flight
            .iter()
            .position(|range| range.reservation_id == reservation_id)
            .ok_or(SchedulerError::UnknownReservation { reservation_id })?;
        let scheduled = self.in_flight.remove(index);
        self.record_processed_range(scheduled.timing)?;
        Ok(scheduled.timing)
    }

    /// Completes a leased range and records the wall-clock cost of that work
    /// in one imperative call. This is the normal host integration path after
    /// a successful decode/VAD/ASR task; it uses the full leased media range
    /// as the source-audio duration for real-time-factor measurement.
    pub fn complete_processing_range_with_sample(
        &mut self,
        reservation_id: u64,
        wall_elapsed_ms: u64,
    ) -> Result<TimeRange, SchedulerError> {
        let timing = self.complete_processing_range(reservation_id)?;
        self.record_processing_sample(timing.duration_ms(), wall_elapsed_ms)?;
        Ok(timing)
    }

    /// Releases a lease after a worker cancellation or recoverable failure so
    /// the range can be selected again. It is harmless to release an unknown
    /// or already-preempted reservation.
    pub fn release_processing_range(&mut self, reservation_id: u64) -> Option<ScheduledRange> {
        let index = self
            .in_flight
            .iter()
            .position(|range| range.reservation_id == reservation_id)?;
        Some(self.in_flight.remove(index))
    }

    /// Records completed coverage supplied by a cache, another worker, or a
    /// preempted worker that still finished. Coverage is merged so adjacent and
    /// overlapping ranges form one contiguous subtitle lead.
    pub fn record_processed_range(&mut self, timing: TimeRange) -> Result<(), SchedulerError> {
        self.validate_processed_range(timing)?;
        self.processed.insert(timing);
        self.clear_seek_recovery_if_buffered();
        Ok(())
    }

    /// Returns whether all milliseconds in `timing` have completed processing.
    pub fn is_range_processed(&self, timing: TimeRange) -> bool {
        self.processed.covers(timing)
    }

    /// Completed, normalized coverage in ascending media-time order. This is
    /// metadata only; it contains no transcript text.
    pub fn processed_coverage(&self) -> &[TimeRange] {
        self.processed.ranges()
    }

    /// The contiguous completed lead from the current playhead, in media-time
    /// milliseconds. In-flight ranges are intentionally excluded.
    pub fn processed_lead_ms(&self) -> u64 {
        if self.mode == SchedulingMode::FullTranscript {
            return 0;
        }
        self.processed
            .covered_until_from(self.playback.position_ms)
            .saturating_sub(self.playback.position_ms)
    }

    /// Aggregated wall-clock processing cost divided by source-audio duration.
    /// A value below 1.0 is faster than 1x media playback.
    pub fn processing_real_time_factor(&self) -> Option<f64> {
        (self.processed_audio_ms != 0)
            .then(|| self.processing_wall_ms as f64 / self.processed_audio_ms as f64)
    }

    fn validate_playback(&self, update: &PlaybackUpdate) -> Result<(), SchedulerError> {
        if !update.playback_rate.is_finite() || update.playback_rate <= 0.0 {
            return Err(SchedulerError::InvalidPlaybackRate);
        }
        if update.position_ms > self.media_duration_ms {
            return Err(SchedulerError::PlaybackPositionOutsideMedia {
                position_ms: update.position_ms,
                media_duration_ms: self.media_duration_ms,
            });
        }
        Ok(())
    }

    fn validate_processed_range(&self, timing: TimeRange) -> Result<(), SchedulerError> {
        if timing.end_ms <= timing.start_ms || timing.end_ms > self.media_duration_ms {
            return Err(SchedulerError::InvalidProcessedRange {
                start_ms: timing.start_ms,
                end_ms: timing.end_ms,
                media_duration_ms: self.media_duration_ms,
            });
        }
        Ok(())
    }

    fn subtitle_window(&self) -> Option<(u64, u64, RangePriority)> {
        if self.playback.position_ms >= self.media_duration_ms {
            // Playback has reached the end. There is no longer a future
            // playhead to protect, so finish any sparse coverage left by
            // earlier seeks rather than parking the job forever and never
            // producing its requested export bundle.
            return (!self.processed.covers(TimeRange {
                start_ms: 0,
                end_ms: self.media_duration_ms,
            }))
            .then_some((0, self.media_duration_ms, RangePriority::Prefetch));
        }

        let target_end_ms = self
            .playback
            .position_ms
            .saturating_add(self.target_buffer_ahead_ms())
            .min(self.media_duration_ms);
        let window_start_ms = self
            .playback
            .position_ms
            .saturating_sub(self.config.context_before_playhead_ms);
        if target_end_ms <= window_start_ms {
            return None;
        }

        Some((
            window_start_ms,
            target_end_ms,
            self.subtitle_range_priority(),
        ))
    }

    fn subtitle_range_priority(&self) -> RangePriority {
        let lead_ms = self.processed_lead_ms();
        if self.seek_recovery_generation.is_some() && lead_ms < self.config.targets.minimum_ahead_ms
        {
            RangePriority::SeekRecovery
        } else if lead_ms < self.config.targets.minimum_ahead_ms {
            RangePriority::MinimumBuffer
        } else if lead_ms < self.config.targets.preferred_ahead_ms {
            RangePriority::PreferredBuffer
        } else {
            RangePriority::Prefetch
        }
    }

    fn target_buffer_ahead_ms(&self) -> u64 {
        if self.mode == SchedulingMode::FullTranscript {
            return 0;
        }
        if !self.playback.is_playing {
            return self.config.targets.maximum_ahead_ms;
        }

        match self
            .processing_real_time_factor()
            .map(|factor| factor * self.playback.playback_rate)
        {
            Some(effective_rtf)
                if effective_rtf <= FAST_PROCESSING_EFFECTIVE_RTF || effective_rtf >= 1.0 =>
            {
                self.config.targets.maximum_ahead_ms
            }
            _ => self.config.targets.preferred_ahead_ms,
        }
    }

    fn subtitle_pacing(
        &self,
        buffer_ahead_ms: u64,
        effective_real_time_factor: Option<f64>,
    ) -> SubtitlePacingState {
        if !self.playback.is_playing {
            return SubtitlePacingState::PlaybackPaused;
        }

        let Some(effective_rtf) = effective_real_time_factor else {
            return SubtitlePacingState::Measuring;
        };
        if effective_rtf >= 1.0 {
            return if buffer_ahead_ms < self.config.targets.minimum_ahead_ms {
                SubtitlePacingState::PauseRecommended
            } else {
                SubtitlePacingState::CannotKeepUp
            };
        }
        if effective_rtf >= AT_RISK_EFFECTIVE_RTF {
            SubtitlePacingState::AtRisk
        } else {
            SubtitlePacingState::KeepingUp
        }
    }

    fn clear_seek_recovery_if_buffered(&mut self) {
        if self.mode == SchedulingMode::SubtitleBuffer
            && self.processed_lead_ms() >= self.config.targets.minimum_ahead_ms
        {
            self.seek_recovery_generation = None;
        }
    }

    fn first_unoccupied_range(&self, start_ms: u64, end_ms: u64) -> Option<TimeRange> {
        if end_ms <= start_ms {
            return None;
        }

        let mut occupied = self.processed.ranges().to_vec();
        occupied.extend(self.in_flight.iter().map(|range| range.timing));
        normalize_ranges(&mut occupied);

        let mut cursor = start_ms;
        for range in occupied {
            if range.end_ms <= cursor {
                continue;
            }
            if range.start_ms > cursor {
                return Some(TimeRange {
                    start_ms: cursor,
                    end_ms: range.start_ms.min(end_ms),
                });
            }
            cursor = cursor.max(range.end_ms);
            if cursor >= end_ms {
                return None;
            }
        }

        (cursor < end_ms).then_some(TimeRange {
            start_ms: cursor,
            end_ms,
        })
    }
}

fn validate_config(
    media_duration_ms: u64,
    config: &SubtitleSchedulerConfig,
) -> Result<(), SchedulerError> {
    if media_duration_ms == 0 {
        return Err(SchedulerError::ZeroMediaDuration);
    }
    let targets = config.targets;
    if targets.minimum_ahead_ms > targets.preferred_ahead_ms
        || targets.preferred_ahead_ms > targets.maximum_ahead_ms
    {
        return Err(SchedulerError::InvalidBufferTargets);
    }
    if config.processing_chunk_ms == 0 {
        return Err(SchedulerError::ZeroProcessingChunk);
    }
    if config.max_in_flight_ranges == 0 {
        return Err(SchedulerError::ZeroInFlightCapacity);
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct RangeSet {
    ranges: Vec<TimeRange>,
}

impl RangeSet {
    fn ranges(&self) -> &[TimeRange] {
        &self.ranges
    }

    fn insert(&mut self, timing: TimeRange) {
        self.ranges.push(timing);
        normalize_ranges(&mut self.ranges);
    }

    fn covers(&self, timing: TimeRange) -> bool {
        self.ranges
            .iter()
            .any(|covered| covered.start_ms <= timing.start_ms && covered.end_ms >= timing.end_ms)
    }

    fn covered_until_from(&self, position_ms: u64) -> u64 {
        self.ranges
            .iter()
            .find(|range| range.start_ms <= position_ms && range.end_ms > position_ms)
            .map_or(position_ms, |range| range.end_ms)
    }
}

fn normalize_ranges(ranges: &mut Vec<TimeRange>) {
    ranges.sort_unstable_by_key(|range| (range.start_ms, range.end_ms));
    let mut normalized: Vec<TimeRange> = Vec::with_capacity(ranges.len());

    for range in ranges.drain(..) {
        if let Some(previous) = normalized.last_mut() {
            if range.start_ms <= previous.end_ms {
                previous.end_ms = previous.end_ms.max(range.end_ms);
                continue;
            }
        }
        normalized.push(range);
    }

    *ranges = normalized;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DURATION_MS: u64 = 600_000;

    fn config() -> SubtitleSchedulerConfig {
        SubtitleSchedulerConfig {
            targets: SubtitleBufferTargets {
                minimum_ahead_ms: 10_000,
                preferred_ahead_ms: 20_000,
                maximum_ahead_ms: 40_000,
            },
            processing_chunk_ms: 10_000,
            context_before_playhead_ms: 0,
            max_in_flight_ranges: 1,
        }
    }

    fn subtitle_scheduler() -> SubtitleBufferScheduler {
        SubtitleBufferScheduler::new(SchedulingMode::SubtitleBuffer, DURATION_MS, config()).unwrap()
    }

    fn playing(position_ms: u64, seek_generation: u64) -> PlaybackUpdate {
        PlaybackUpdate {
            position_ms,
            playback_rate: 1.0,
            is_playing: true,
            seek_generation,
        }
    }

    #[test]
    fn normal_playback_fills_the_preferred_lead_without_overscheduling() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(0, 0)).unwrap();

        let first = scheduler.next_processing_range().unwrap();
        assert_eq!(first.timing, TimeRange::new(0, 10_000).unwrap());
        assert_eq!(first.priority, RangePriority::MinimumBuffer);
        scheduler
            .complete_processing_range_with_sample(first.reservation_id, 8_000)
            .unwrap();

        let second = scheduler.next_processing_range().unwrap();
        assert_eq!(second.timing, TimeRange::new(10_000, 20_000).unwrap());
        assert_eq!(second.priority, RangePriority::PreferredBuffer);
        scheduler
            .complete_processing_range_with_sample(second.reservation_id, 8_000)
            .unwrap();

        assert_eq!(scheduler.processed_lead_ms(), 20_000);
        assert_eq!(scheduler.next_processing_range(), None);
        let status = scheduler.status();
        assert_eq!(status.subtitle_buffer_ahead_ms, Some(20_000));
        assert_eq!(status.target_buffer_ahead_ms, Some(20_000));
    }

    #[test]
    fn major_seek_preempts_old_work_and_prioritizes_the_new_playhead() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(0, 0)).unwrap();
        let old = scheduler.next_processing_range().unwrap();

        let result = scheduler.update_playback(playing(480_000, 1)).unwrap();
        assert_eq!(result.disposition, PlaybackUpdateDisposition::Applied);
        assert_eq!(result.preempted, vec![old]);

        let next = scheduler.next_processing_range().unwrap();
        assert_eq!(next.timing, TimeRange::new(480_000, 490_000).unwrap());
        assert_eq!(next.priority, RangePriority::SeekRecovery);
        assert_eq!(next.seek_generation, 1);
    }

    #[test]
    fn first_page_snapshot_repositions_an_initial_zero_range() {
        let mut scheduler = subtitle_scheduler();
        let initial = scheduler.next_processing_range().unwrap();
        assert_eq!(initial.timing, TimeRange::new(0, 10_000).unwrap());

        // The native host can start work before the content observer reports
        // its first snapshot. A user who opened the extension at 08:00 must
        // not wait for the default 00:00 range to finish.
        let result = scheduler.update_playback(playing(480_000, 0)).unwrap();
        assert_eq!(result.disposition, PlaybackUpdateDisposition::Applied);
        assert_eq!(result.preempted, vec![initial]);

        let next = scheduler.next_processing_range().unwrap();
        assert_eq!(next.timing, TimeRange::new(480_000, 490_000).unwrap());
        assert_eq!(next.priority, RangePriority::SeekRecovery);
        assert_eq!(next.seek_generation, 0);
    }

    #[test]
    fn reaching_media_end_finishes_sparse_ranges_instead_of_parking() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(DURATION_MS, 1)).unwrap();
        scheduler
            .record_processed_range(TimeRange::new(10_000, 20_000).unwrap())
            .unwrap();

        let first_gap = scheduler.next_processing_range().unwrap();
        assert_eq!(first_gap.timing, TimeRange::new(0, 10_000).unwrap());
        assert_eq!(first_gap.priority, RangePriority::Prefetch);
        scheduler
            .complete_processing_range(first_gap.reservation_id)
            .unwrap();

        let second_gap = scheduler.next_processing_range().unwrap();
        assert_eq!(second_gap.timing, TimeRange::new(20_000, 30_000).unwrap());
    }

    #[test]
    fn completed_coverage_is_merged_and_gaps_are_prioritized() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(5_000, 0)).unwrap();
        scheduler
            .record_processed_range(TimeRange::new(0, 10_000).unwrap())
            .unwrap();
        scheduler
            .record_processed_range(TimeRange::new(20_000, 30_000).unwrap())
            .unwrap();
        scheduler
            .record_processed_range(TimeRange::new(10_000, 20_000).unwrap())
            .unwrap();

        assert_eq!(
            scheduler.processed_coverage(),
            &[TimeRange::new(0, 30_000).unwrap()]
        );
        assert_eq!(scheduler.processed_lead_ms(), 25_000);
        assert!(scheduler.is_range_processed(TimeRange::new(5_000, 25_000).unwrap()));

        scheduler
            .record_processed_range(TimeRange::new(40_000, 50_000).unwrap())
            .unwrap();
        // A fast local engine is allowed to prefetch to the maximum lead, so
        // the scheduler sees and fills the otherwise uncovered 30-40 s gap.
        scheduler.record_processing_sample(10_000, 4_000).unwrap();
        let next = scheduler.next_processing_range().unwrap();
        assert_eq!(next.timing, TimeRange::new(30_000, 40_000).unwrap());
    }

    #[test]
    fn playback_rate_changes_effective_realtime_factor_and_target() {
        let mut scheduler = subtitle_scheduler();
        scheduler
            .update_playback(PlaybackUpdate {
                position_ms: 0,
                playback_rate: 2.0,
                is_playing: true,
                seek_generation: 0,
            })
            .unwrap();
        scheduler.record_processing_sample(10_000, 6_000).unwrap();

        let status = scheduler.status();
        assert_eq!(status.processing_real_time_factor, Some(0.6));
        assert_eq!(status.effective_real_time_factor, Some(1.2));
        assert_eq!(status.target_buffer_ahead_ms, Some(40_000));
        assert_eq!(status.pacing, SubtitlePacingState::PauseRecommended);
    }

    #[test]
    fn slow_processing_recommends_a_pause_only_when_minimum_lead_is_missing() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(0, 0)).unwrap();
        scheduler.record_processing_sample(10_000, 15_000).unwrap();
        assert_eq!(
            scheduler.status().pacing,
            SubtitlePacingState::PauseRecommended
        );

        scheduler
            .record_processed_range(TimeRange::new(0, 15_000).unwrap())
            .unwrap();
        assert_eq!(scheduler.status().pacing, SubtitlePacingState::CannotKeepUp);
    }

    #[test]
    fn full_transcript_ignores_seek_updates_and_stays_sequential() {
        let mut scheduler =
            SubtitleBufferScheduler::new(SchedulingMode::FullTranscript, DURATION_MS, config())
                .unwrap();
        let first = scheduler.next_processing_range().unwrap();
        assert_eq!(first.timing, TimeRange::new(0, 10_000).unwrap());
        let update = scheduler.update_playback(playing(480_000, 10)).unwrap();
        assert_eq!(
            update.disposition,
            PlaybackUpdateDisposition::IgnoredForFullTranscript
        );
        assert!(update.preempted.is_empty());
        scheduler
            .complete_processing_range(first.reservation_id)
            .unwrap();
        let second = scheduler.next_processing_range().unwrap();
        assert_eq!(second.timing, TimeRange::new(10_000, 20_000).unwrap());
        assert_eq!(
            scheduler.status().pacing,
            SubtitlePacingState::FullTranscriptIndependent
        );
    }

    #[test]
    fn stale_seek_generation_cannot_undo_a_newer_seek() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(120_000, 2)).unwrap();
        let stale = scheduler.update_playback(playing(20_000, 1)).unwrap();
        assert_eq!(
            stale.disposition,
            PlaybackUpdateDisposition::IgnoredStaleGeneration
        );
        assert_eq!(scheduler.playback().position_ms, 120_000);
        assert_eq!(scheduler.playback().seek_generation, 2);
    }

    #[test]
    fn a_preempted_worker_can_still_contribute_valid_coverage() {
        let mut scheduler = subtitle_scheduler();
        scheduler.update_playback(playing(0, 0)).unwrap();
        let old = scheduler.next_processing_range().unwrap();
        scheduler.update_playback(playing(300_000, 1)).unwrap();

        assert_eq!(
            scheduler.complete_processing_range(old.reservation_id),
            Err(SchedulerError::UnknownReservation {
                reservation_id: old.reservation_id
            })
        );
        scheduler.record_processed_range(old.timing).unwrap();
        assert!(scheduler.is_range_processed(old.timing));
    }

    #[test]
    fn validates_configuration_and_input_ranges() {
        let error = SubtitleBufferScheduler::new(
            SchedulingMode::SubtitleBuffer,
            0,
            SubtitleSchedulerConfig::default(),
        )
        .unwrap_err();
        assert_eq!(error, SchedulerError::ZeroMediaDuration);

        let mut bad_config = config();
        bad_config.targets.minimum_ahead_ms = 30_000;
        bad_config.targets.preferred_ahead_ms = 20_000;
        let error =
            SubtitleBufferScheduler::new(SchedulingMode::SubtitleBuffer, DURATION_MS, bad_config)
                .unwrap_err();
        assert_eq!(error, SchedulerError::InvalidBufferTargets);

        let mut scheduler = subtitle_scheduler();
        assert_eq!(
            scheduler.record_processed_range(TimeRange::new(5_000, 5_000).unwrap()),
            Err(SchedulerError::InvalidProcessedRange {
                start_ms: 5_000,
                end_ms: 5_000,
                media_duration_ms: DURATION_MS,
            })
        );
        assert_eq!(
            scheduler.update_playback(PlaybackUpdate {
                position_ms: 0,
                playback_rate: 0.0,
                is_playing: true,
                seek_generation: 0,
            }),
            Err(SchedulerError::InvalidPlaybackRate)
        );
    }
}
