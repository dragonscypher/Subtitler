//! Local-first ASR interfaces.
//!
//! `whisper.cpp` is intentionally represented by an explicit process/FFI
//! boundary rather than bundled as unreviewed source or invoked through a
//! shell. The real engine can be supplied later without changing callers.

mod cloud;
mod model;
mod provider;
mod whisper_cpp;

pub use cloud::*;
pub use model::*;
pub use provider::*;
pub use whisper_cpp::*;
