//! Deterministic subtitle segmentation and export.
//!
//! The segmenter consumes word timestamps rather than arbitrary ASR chunks so
//! cues can respect pauses, punctuation, line length, reading speed, and seek
//! synchronization.

mod export;
mod files;
mod segment;

pub use export::*;
pub use files::*;
pub use segment::*;
