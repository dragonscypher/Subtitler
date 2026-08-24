use serde::{Deserialize, Serialize};
use subtitler_core::{SubtitleCue, TimeRange, WordTimestamp};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleSegmentationConfig {
    /// The visible line width at the default overlay size.
    pub max_chars_per_line: usize,
    pub max_lines: usize,
    pub max_cue_duration_ms: u64,
    pub min_cue_duration_ms: u64,
    pub pause_break_ms: u64,
    /// Maximum comfortable reading speed, in characters per second.
    pub max_chars_per_second: u32,
}

impl Default for SubtitleSegmentationConfig {
    fn default() -> Self {
        Self {
            max_chars_per_line: 42,
            max_lines: 2,
            max_cue_duration_ms: 6_000,
            min_cue_duration_ms: 900,
            pause_break_ms: 650,
            max_chars_per_second: 17,
        }
    }
}

impl SubtitleSegmentationConfig {
    fn validate(&self) -> Result<(), SubtitleSegmentationError> {
        if self.max_chars_per_line == 0 || self.max_lines == 0 {
            return Err(SubtitleSegmentationError::InvalidConfiguration(
                "line limits must be greater than zero".to_owned(),
            ));
        }
        if self.max_cue_duration_ms == 0 || self.max_chars_per_second == 0 {
            return Err(SubtitleSegmentationError::InvalidConfiguration(
                "cue duration and reading speed must be greater than zero".to_owned(),
            ));
        }
        if self.min_cue_duration_ms > self.max_cue_duration_ms {
            return Err(SubtitleSegmentationError::InvalidConfiguration(
                "minimum cue duration cannot exceed maximum cue duration".to_owned(),
            ));
        }
        Ok(())
    }

    fn max_cue_chars(&self) -> usize {
        self.max_chars_per_line.saturating_mul(self.max_lines)
    }
}

/// Convert sorted word timestamps to display-ready subtitle cues.
pub fn segment_words(
    words: &[WordTimestamp],
    config: &SubtitleSegmentationConfig,
) -> Result<Vec<SubtitleCue>, SubtitleSegmentationError> {
    config.validate()?;
    validate_words(words)?;
    // A few ASR backends can emit a recognized token with an equal start/end
    // timestamp. It is meaningful in a transcript, but cannot become a valid
    // browser/SRT cue (`end` must be strictly after `start`). Omit only those
    // timing-less words here; keep the transcript artifact intact.
    let timed_words = words
        .iter()
        .filter(|word| word.timing.end_ms > word.timing.start_ms)
        .collect::<Vec<_>>();
    if timed_words.is_empty() {
        return Ok(Vec::new());
    }

    let mut cues = Vec::new();
    let mut current = CueBuilder::default();

    for word in timed_words {
        if current.is_empty() {
            current.push(word);
            continue;
        }

        if should_break_before(&current, word, config) {
            cues.push(current.into_cue(config)?);
            current = CueBuilder::default();
        }
        current.push(word);
    }

    if !current.is_empty() {
        cues.push(current.into_cue(config)?);
    }

    Ok(cues)
}

fn validate_words(words: &[WordTimestamp]) -> Result<(), SubtitleSegmentationError> {
    let mut previous_end = 0;
    for word in words {
        word.validate()
            .map_err(|error| SubtitleSegmentationError::InvalidWord(error.to_string()))?;
        if word.timing.start_ms < previous_end {
            return Err(SubtitleSegmentationError::OutOfOrderWords);
        }
        previous_end = word.timing.end_ms;
    }
    Ok(())
}

fn should_break_before(
    current: &CueBuilder<'_>,
    next: &WordTimestamp,
    config: &SubtitleSegmentationConfig,
) -> bool {
    let previous = current.last().expect("current cue is non-empty");
    let current_duration = current.end_ms().saturating_sub(current.start_ms());
    let candidate_duration = next.timing.end_ms.saturating_sub(current.start_ms());
    let candidate_chars = current.text_len_with(next);
    let gap_ms = next.timing.start_ms.saturating_sub(previous.timing.end_ms);

    let exceeds_duration = candidate_duration > config.max_cue_duration_ms;
    let exceeds_chars = candidate_chars > config.max_cue_chars();
    let exceeds_reading_speed = candidate_duration >= config.min_cue_duration_ms
        && (candidate_chars as u128).saturating_mul(1_000)
            > u128::from(config.max_chars_per_second)
                .saturating_mul(u128::from(candidate_duration));
    let substantial_pause = gap_ms >= config.pause_break_ms;
    let complete_sentence =
        ends_sentence(&previous.text) && current_duration >= config.min_cue_duration_ms;

    // Never split a single unbreakable word. It is better to let that one word
    // exceed a visual limit than omit or corrupt the transcript.
    (exceeds_duration
        || exceeds_chars
        || exceeds_reading_speed
        || substantial_pause
        || complete_sentence)
        && !current.is_empty()
}

fn ends_sentence(value: &str) -> bool {
    matches!(
        value
            .trim_end_matches(['"', '\'', ')', ']', '}', '…'])
            .chars()
            .last(),
        Some('.' | '!' | '?' | '…')
    )
}

#[derive(Default)]
struct CueBuilder<'a> {
    words: Vec<&'a WordTimestamp>,
}

impl<'a> CueBuilder<'a> {
    fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    fn push(&mut self, word: &'a WordTimestamp) {
        self.words.push(word);
    }

    fn last(&self) -> Option<&'a WordTimestamp> {
        self.words.last().copied()
    }

    fn start_ms(&self) -> u64 {
        self.words
            .first()
            .expect("current cue is non-empty")
            .timing
            .start_ms
    }

    fn end_ms(&self) -> u64 {
        self.words
            .last()
            .expect("current cue is non-empty")
            .timing
            .end_ms
    }

    fn text_len_with(&self, next: &WordTimestamp) -> usize {
        let words_len: usize = self.words.iter().map(|word| display_len(&word.text)).sum();
        words_len + self.words.len() + display_len(&next.text)
    }

    fn into_cue(
        self,
        config: &SubtitleSegmentationConfig,
    ) -> Result<SubtitleCue, SubtitleSegmentationError> {
        let timing = TimeRange::new(self.start_ms(), self.end_ms())
            .map_err(|error| SubtitleSegmentationError::InvalidWord(error.to_string()))?;
        let lines = wrap_words(&self.words, config);
        let speaker = self.shared_speaker();
        Ok(SubtitleCue {
            timing,
            lines,
            speaker,
        })
    }

    fn shared_speaker(&self) -> Option<String> {
        let first = self.words.first()?.speaker.as_ref()?;
        if self
            .words
            .iter()
            .all(|word| word.speaker.as_deref() == Some(first.as_str()))
        {
            Some(first.clone())
        } else {
            None
        }
    }
}

fn display_len(value: &str) -> usize {
    value.trim().chars().count()
}

fn join_words(words: &[&WordTimestamp]) -> String {
    words
        .iter()
        .map(|word| word.text.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

fn wrap_words(words: &[&WordTimestamp], config: &SubtitleSegmentationConfig) -> Vec<String> {
    let text = join_words(words);
    if display_len(&text) <= config.max_chars_per_line || config.max_lines == 1 {
        return vec![text];
    }

    // For the normal two-line subtitle case, pick the most balanced legal
    // word boundary. This avoids a short first line followed by a dense line.
    if config.max_lines == 2 {
        let mut best: Option<(usize, usize)> = None;
        for split in 1..words.len() {
            let left = join_words(&words[..split]);
            let right = join_words(&words[split..]);
            let left_len = display_len(&left);
            let right_len = display_len(&right);
            if left_len <= config.max_chars_per_line && right_len <= config.max_chars_per_line {
                let imbalance = left_len.abs_diff(right_len);
                if best.map(|(_, score)| imbalance < score).unwrap_or(true) {
                    best = Some((split, imbalance));
                }
            }
        }
        if let Some((split, _)) = best {
            return vec![join_words(&words[..split]), join_words(&words[split..])];
        }
    }

    // Fallback for custom line counts or a single overlong word. Segmentation
    // has already enforced the cue-wide limit, so this maintains word order
    // and only permits an overlong individual word.
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in words {
        let word = word.text.trim();
        let candidate_len = if current.is_empty() {
            display_len(word)
        } else {
            display_len(&current) + 1 + display_len(word)
        };
        if !current.is_empty()
            && candidate_len > config.max_chars_per_line
            && lines.len() + 1 < config.max_lines
        {
            lines.push(current);
            current = word.to_owned();
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SubtitleSegmentationError {
    #[error("invalid subtitle segmentation configuration: {0}")]
    InvalidConfiguration(String),
    #[error("word timestamps are invalid: {0}")]
    InvalidWord(String),
    #[error("word timestamps must be ordered and non-overlapping")]
    OutOfOrderWords,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start_ms: u64, end_ms: u64) -> WordTimestamp {
        WordTimestamp {
            text: text.to_owned(),
            timing: TimeRange::new(start_ms, end_ms).unwrap(),
            speaker: None,
        }
    }

    #[test]
    fn sentence_and_pause_boundaries_make_stable_cues() {
        let words = vec![
            word("Hello", 0, 500),
            word("world.", 500, 1_400),
            word("This", 1_700, 1_900),
            word("is", 1_900, 2_100),
            word("Subtitler.", 2_100, 2_800),
        ];
        let cues = segment_words(&words, &SubtitleSegmentationConfig::default()).unwrap();

        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text(), "Hello world.");
        assert_eq!(cues[0].timing.end_ms, 1_400);
        assert_eq!(cues[1].text(), "This is Subtitler.");
    }

    #[test]
    fn segmenter_rejects_overlapping_word_timestamps() {
        let words = vec![word("one", 0, 500), word("two", 400, 800)];
        assert_eq!(
            segment_words(&words, &SubtitleSegmentationConfig::default()).unwrap_err(),
            SubtitleSegmentationError::OutOfOrderWords
        );
    }

    #[test]
    fn segmenter_omits_zero_duration_words_from_browser_cues() {
        let words = vec![
            word("timeless", 1_000, 1_000),
            word("visible", 1_100, 1_700),
        ];
        let cues = segment_words(&words, &SubtitleSegmentationConfig::default()).unwrap();

        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text(), "visible");
        assert!(cues[0].timing.end_ms > cues[0].timing.start_ms);
    }
}
