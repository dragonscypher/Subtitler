//! Media acquisition and audio-pipeline boundaries.
//!
//! This crate deliberately validates and plans media acquisition without
//! reading browser cookies, defeating DRM, or shelling out through a command
//! string. A future platform adapter can implement the explicit browser-
//! mediated handoff boundary while retaining these policy checks.

mod audio;
mod policy;
mod remote;

pub use audio::*;
pub use policy::*;
pub use remote::*;
