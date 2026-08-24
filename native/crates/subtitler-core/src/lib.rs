//! Shared domain and protocol types used by the extension-facing native host
//! and the local processing crates. These types intentionally carry metadata,
//! not browser cookies, transcript contents in logs, or executable commands.

mod domain;
mod jobs;
mod protocol;
mod scheduler;

pub use domain::*;
pub use jobs::*;
pub use protocol::*;
pub use scheduler::*;
