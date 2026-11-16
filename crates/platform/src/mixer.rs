//! Muting a Focusrite interface's main mix while dictating: a game console plugged into a
//! Scarlett and mixed straight to its outputs never passes through the OS output volume, so
//! ducking (see `duck`) can't reach it. Focusrite Control 2 takes MIDI control changes from a
//! MIDI port the user picks (a virtual one: a Windows MIDI Services loopback, or loopMIDI's):
//! CC 62 mutes (127) or unmutes (0) the Mix A bus, which feeds outputs 1-2 (the monitors) by
//! default. That silences everything on the speakers at once: the inputs and the computer's
//! playback. (CC 78, "Mute, Monitors", does nothing on a Scarlett 4i4, whose monitor knob is
//! analogue.) Runs on the `duck` thread, next to the volume.
//!
//! Only a mute AGT made is undone: muting again while muted sends nothing new, and a failed
//! send (the port is gone) leaves nothing to undo. A failed unmute is retried with the next
//! restore. While the mix is muted, a small marker file names the port: if AGT dies before
//! unmuting, the next start does (trying once: the port may have been renamed since).
//!
//! Each mute and unmute is an `info` event with how long the MIDI call took: AGT has died
//! without a trace while muting was on (no panic, no Windows error report), and these
//! breadcrumbs tell whether the last thing it did was a MIDI call into the driver. A death
//! inside the mute call leaves the marker, which the next start reports; one inside the unmute
//! call leaves "unmuting the interface's mix" as the run's last event.

use std::{fs, path::PathBuf, time::Instant};

/// Where to send the mute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixerMute {
    /// The MIDI output port Focusrite Control 2 listens on.
    pub port: String,
}

/// Focusrite Control 2: "Mix bus mute (Mix A)".
const MIX_A_MUTE: u8 = 62;

/// The control change, on channel 1 (which Focusrite Control 2 hears on "All channels" too).
fn message(mute: bool) -> [u8; 3] {
    [0xB0, MIX_A_MUTE, if mute { 127 } else { 0 }]
}

/// Sends MIDI messages to a port; `Err` if they weren't sent.
pub(crate) type Send = Box<dyn FnMut(&str, &[[u8; 3]]) -> Result<(), crate::Error>>;

pub(crate) struct Mixer {
    config: Option<MixerMute>,
    /// What we muted and haven't unmuted yet.
    muted: Option<MixerMute>,
    marker: Option<PathBuf>,
    send: Send,
    /// A failed mute was logged at `warn` once; the rest at `debug`.
    warned: bool,
}

impl Mixer {
    /// `config`: `None` mutes nothing (but a mix a previous run left muted is still unmuted).
    /// `marker`: where to note the mute (`None` in tests).
    pub(crate) fn new(config: Option<MixerMute>, marker: Option<PathBuf>, send: Send) -> Self {
        let left = marker
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|t| t.trim().to_owned())
            .filter(|port| !port.is_empty())
            .map(|port| MixerMute { port });
        let mut mixer = Self {
            config,
            muted: left,
            marker,
            send,
            warned: false,
        };
        if mixer.muted.is_some() {
            // Also the sign that the previous run died while muting or muted.
            tracing::info!("unmuting the interface's mix a previous run left muted");
            mixer.unmute();
            // Not retried: the port may be gone for good (renamed), which would otherwise
            // keep muting off forever.
            if mixer.muted.take().is_some() {
                mixer.save(None);
            }
        }
        mixer
    }

    pub(crate) fn mute(&mut self) {
        let Some(config) = &self.config else { return };
        if self.muted.is_some() {
            return;
        }
        let config = config.clone();
        // Before sending: a crash right after must still unmute.
        self.save(Some(&config));
        let t = Instant::now();
        match (self.send)(&config.port, &[message(true)]) {
            Ok(()) => {
                tracing::info!("muted the interface's mix ({})", ms(t));
                self.muted = Some(config);
            }
            Err(e) => {
                if self.warned {
                    tracing::debug!("cannot mute the interface's mix: {e}");
                } else {
                    self.warned = true;
                    tracing::warn!("cannot mute the interface's mix: {e}");
                }
                self.save(None);
            }
        }
    }

    /// Unmutes what we muted, if anything. A failed send is kept, to retry next time.
    pub(crate) fn unmute(&mut self) {
        let Some(muted) = &self.muted else { return };
        // Before sending: without it, a death inside the call looks like one while muted.
        tracing::info!("unmuting the interface's mix");
        let t = Instant::now();
        match (self.send)(&muted.port, &[message(false)]) {
            Ok(()) => {
                tracing::info!("unmuted the interface's mix ({})", ms(t));
                self.muted = None;
                self.save(None);
            }
            // Always loud: the speakers stay muted until a retry (the next release) works.
            Err(e) => tracing::warn!("cannot unmute the interface's mix: {e}"),
        }
    }

    fn save(&self, muted: Option<&MixerMute>) {
        let Some(p) = &self.marker else { return };
        let saved = match muted {
            Some(m) => fs::write(p, format!("{}\n", m.port)),
            None => match fs::remove_file(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                r => r,
            },
        };
        if let Err(e) = saved {
            tracing::debug!("cannot update {}: {e}", p.display());
        }
    }
}

/// "12.3 ms": how long a MIDI call took.
fn ms(since: Instant) -> String {
    format!("{:.1} ms", since.elapsed().as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<(String, Vec<[u8; 3]>)>>>;

    /// A sender that records what it sends; fails while `*down` is true.
    fn recorder(down: Arc<Mutex<bool>>) -> (Send, Log) {
        let log: Log = Arc::default();
        let l = log.clone();
        let send: Send = Box::new(move |port, msgs| {
            if *down.lock().unwrap() {
                return Err("port gone".into());
            }
            l.lock().unwrap().push((port.to_owned(), msgs.to_vec()));
            Ok(())
        });
        (send, log)
    }

    fn agt() -> Option<MixerMute> {
        Some(MixerMute { port: "AGT".into() })
    }

    const MUTE: [u8; 3] = [0xB0, 62, 127];
    const UNMUTE: [u8; 3] = [0xB0, 62, 0];

    #[test]
    fn mutes_and_unmutes_once() {
        let (send, log) = recorder(Arc::default());
        let mut m = Mixer::new(agt(), None, send);
        m.mute();
        m.mute(); // pressed again while muted: nothing new
        m.unmute();
        m.unmute();
        assert_eq!(
            *log.lock().unwrap(),
            [("AGT".into(), vec![MUTE]), ("AGT".into(), vec![UNMUTE])]
        );
    }

    #[test]
    fn nothing_is_sent_without_a_port() {
        let (send, log) = recorder(Arc::default());
        let mut m = Mixer::new(None, None, send);
        m.mute();
        m.unmute();
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_mute_is_never_undone_and_a_failed_unmute_is_retried() {
        let down = Arc::new(Mutex::new(true));
        let (send, log) = recorder(down.clone());
        let mut m = Mixer::new(agt(), None, send);
        m.mute();
        *down.lock().unwrap() = false;
        m.unmute();
        assert!(log.lock().unwrap().is_empty());

        m.mute();
        *down.lock().unwrap() = true;
        m.unmute();
        *down.lock().unwrap() = false;
        m.unmute();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].1, vec![UNMUTE]);
    }

    #[test]
    fn a_mix_left_muted_by_a_crash_is_unmuted_on_the_next_start() {
        let dir = std::env::temp_dir().join(format!("agt-mixer-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("muted-mix");
        let (send, _) = recorder(Arc::default());
        let mut m = Mixer::new(agt(), Some(path.clone()), send);
        m.mute();
        assert!(path.exists());
        std::mem::forget(m); // "crashed"

        // Started again with muting since turned off: still unmuted, on the old port.
        let (send, log) = recorder(Arc::default());
        drop(Mixer::new(None, Some(path.clone()), send));
        assert_eq!(*log.lock().unwrap(), [("AGT".into(), vec![UNMUTE])]);
        assert!(!path.exists());

        // Left muted on a port that's gone for good: tried once, then forgotten, so muting
        // on the configured port still works.
        let (send, _) = recorder(Arc::default());
        let mut m = Mixer::new(agt(), Some(path.clone()), send);
        m.mute();
        std::mem::forget(m);
        let down = Arc::new(Mutex::new(true));
        let (send, log) = recorder(down.clone());
        let mut m = Mixer::new(agt(), Some(path.clone()), send);
        assert!(!path.exists());
        *down.lock().unwrap() = false;
        m.mute();
        assert_eq!(*log.lock().unwrap(), [("AGT".into(), vec![MUTE])]);
        m.unmute();

        // A normal unmute removes the file too.
        let (send, _) = recorder(Arc::default());
        let mut m = Mixer::new(agt(), Some(path.clone()), send);
        m.mute();
        m.unmute();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Mutes this machine's Focusrite mix for three seconds through Focusrite Control 2.
    /// Run by hand, with it listening on the port in `AGT_MIDI_PORT` (default "AGT"):
    /// `cargo test -p agt-platform real_mixer -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_mixer_mutes_for_three_seconds() {
        let port = std::env::var("AGT_MIDI_PORT").unwrap_or_else(|_| "AGT".into());
        crate::imp::midi_send(&port, &[message(true)]).expect("muted");
        println!("muted Mix A on {port:?}");
        std::thread::sleep(std::time::Duration::from_secs(3));
        crate::imp::midi_send(&port, &[message(false)]).expect("unmuted");
        println!("unmuted");
    }
}
