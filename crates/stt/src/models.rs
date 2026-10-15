//! Model files: downloaded on first use, SHA-256 verified, atomically renamed into place
//! (see [`agt_fetch`], shared with the managed S1-mini polish in `agt-polish`).

pub use agt_fetch::{ModelFile, Progress, ensure};
