//! Meeting notes without a bot: the user's microphone ("Me") and what the computer plays
//! ("Them"), transcribed on this machine into timestamped lines, kept next to the notes the
//! user types during the call, and turned into structured notes by the user's own Claude Code
//! CLI when they ask.
//!
//! **Privacy:** plain files on this machine. Nothing leaves it, except the transcript and the
//! user's notes piped to `claude` for a summary ([`summary::run`]): when the user clicks
//! Generate notes, or after each meeting if they turned on `meeting.auto_summary`.
//!
//! A [`Session`] records one meeting: both tracks cut live, transcribed through the app's
//! [`Transcriber`], echo filtered, written as it goes. [`DeviceCaptures`] opens the real
//! devices for it.
//!
//! # Layout
//!
//! One folder per meeting, `<root>/<id>/`, `id` being the local start time
//! `YYYY-MM-DD-HHMMSS`:
//! - `meeting.json`: the [`Meeting`] (title, times, the user's notes, the AI notes).
//! - `transcript.jsonl`: one [`Line`] per line of text, appended as each is final.
//! - `meeting.md`: the Markdown export ([`markdown`]), rewritten on stop and after a summary.
//!
//! A meeting AGT never got to stop (killed, crashed) keeps `ended_at` null and its WAV sizes at
//! zero; [`Store::recover`] puts both right, and writes its `meeting.md`, at the next launch.
//! - `mic.wav`, `system.wav`: the two tracks' audio, only with `meeting.save_audio`.

mod devices;
pub mod echo;
mod session;
mod store;
pub mod summary;
mod time;

pub use devices::DeviceCaptures;
pub use session::{
    CaptureFactory, CaptureHandle, Event, Session, SessionConfig, Sink, SystemAudio, Track,
};
pub use store::{Store, markdown};

use serde::{Deserialize, Serialize};

/// Who said a [`Line`]: the track it was transcribed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    /// The microphone: the user.
    Me,
    /// What the computer plays: everyone else on the call, however many people.
    Them,
}

impl Speaker {
    /// `Me` / `Them`, as the transcript shows it.
    pub fn label(self) -> &'static str {
        match self {
            Speaker::Me => "Me",
            Speaker::Them => "Them",
        }
    }
}

/// One transcribed segment, a line of `transcript.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// Unique within the meeting.
    pub id: u64,
    pub speaker: Speaker,
    /// On the meeting clock: milliseconds since the meeting started.
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// `meeting.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    /// The folder's name, `YYYY-MM-DD-HHMMSS` (local start time).
    pub id: String,
    pub title: String,
    /// Unix ms.
    pub started_at: i64,
    /// Unix ms; `None` while recording (or if AGT never got to stop it).
    #[serde(default)]
    pub ended_at: Option<i64>,
    /// What the user typed during the call.
    #[serde(default)]
    pub notes: String,
    /// The AI notes (Markdown).
    #[serde(default)]
    pub summary: Option<String>,
    /// What wrote `summary`, e.g. the model.
    #[serde(default)]
    pub summary_by: Option<String>,
    /// Audio files in the folder (only with `meeting.save_audio`).
    #[serde(default)]
    pub audio: Vec<String>,
}

/// One row of the past meetings list.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeetingInfo {
    pub id: String,
    pub title: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub has_summary: bool,
}

/// Speech to text for a meeting's segments; the app implements it with its transcription
/// engine (behind dictation, which always goes first).
pub trait Transcriber: Send + Sync {
    /// Transcribes `audio` (16 kHz mono) and calls `done` with the text or an error, on any
    /// thread, possibly much later.
    fn transcribe(&self, audio: Vec<f32>, done: Box<dyn FnOnce(Result<String, String>) + Send>);
}
