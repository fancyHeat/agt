//! Ducking: turning the default output device's volume down while dictating, so music or a
//! video doesn't talk over the user, and back up afterwards.
//!
//! All volume work happens on a `duck` thread fed by a channel: [`Ducker::duck`] is a channel
//! send, so it never delays starting the microphone. The volume fades (down in
//! [`FADE_DOWN`], back up in [`FADE_UP`]) in a few steps.
//!
//! Restoring is careful not to fight the user:
//! - It only sets the volume back if it's still the one we set: a change the user made while
//!   dictating stays.
//! - A muted output, or one already as low as the duck target, is left alone.
//! - A dictation started before the previous one's volume came back keeps the original
//!   from before the first one: pressing again and again never ratchets the volume down.
//! - While a device is ducked, its ID and original volume are kept in a small marker file.
//!   If AGT dies before restoring, the next start restores it; if the device goes away while
//!   ducked (a Bluetooth headset switched off), it's restored once it's back (at the next
//!   dictation). Only if its volume still looks like ours: after a crash, anything between the
//!   ducked value and the original (a fade may have been cut short); after a device loss,
//!   exactly the volume we left it at.

use crate::imp;
use std::{
    fs,
    path::PathBuf,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

const FADE_DOWN: Duration = Duration::from_millis(80);
const FADE_UP: Duration = Duration::from_millis(250);
/// Time between fade steps: 4 steps down, 12 up.
const STEP: Duration = Duration::from_millis(20);
/// How close the volume must be to the one we set (as read back) to count as untouched by
/// the user: well under the smallest step a volume key makes (Windows: 0.02; macOS: 1/16 or
/// 1/64).
const TOLERANCE: f32 = 0.005;

/// A duck level as a usable fraction: 0..=1, NaN as 0.
fn clamp_level(level: f32) -> f32 {
    if level.is_nan() {
        0.0
    } else {
        level.clamp(0.0, 1.0)
    }
}

/// What ducking needs from a volume control (the OS one, or a fake in tests).
trait Output {
    /// Stable across reconnects, to find the device again.
    fn id(&self) -> String;
    /// `None`: can't be read (e.g. the device went away).
    fn volume(&mut self) -> Option<f32>;
    fn muted(&mut self) -> bool;
    fn set_volume(&mut self, v: f32) -> bool;
}

/// Opens volume controls.
trait Devices {
    type Out: Output;
    /// The default output, if its volume can be set.
    fn open_default(&mut self) -> Option<Self::Out>;
    /// The device with this [`Output::id`], if it's there now.
    fn open(&mut self, id: &str) -> Option<Self::Out>;
}

impl Output for imp::OutputVolume {
    fn id(&self) -> String {
        imp::OutputVolume::id(self).to_owned()
    }

    fn volume(&mut self) -> Option<f32> {
        imp::OutputVolume::volume(self)
            .inspect_err(|e| tracing::debug!("cannot read the output volume: {e}"))
            .ok()
    }

    fn muted(&mut self) -> bool {
        imp::OutputVolume::muted(self).unwrap_or(false)
    }

    fn set_volume(&mut self, v: f32) -> bool {
        imp::OutputVolume::set_volume(self, v)
            .inspect_err(|e| tracing::debug!("cannot set the output volume: {e}"))
            .is_ok()
    }
}

/// The OS's devices.
#[derive(Default)]
struct Real {
    said_unsupported: bool,
}

impl Devices for Real {
    type Out = imp::OutputVolume;

    fn open_default(&mut self) -> Option<Self::Out> {
        match imp::OutputVolume::open_default() {
            Ok(Some(out)) => Some(out),
            Ok(None) => {
                if !self.said_unsupported {
                    self.said_unsupported = true;
                    tracing::debug!("the default output's volume can't be set; not ducking");
                }
                None
            }
            Err(e) => {
                tracing::debug!("cannot open the default output's volume: {e}");
                None
            }
        }
    }

    fn open(&mut self, id: &str) -> Option<Self::Out> {
        imp::OutputVolume::open(id)
            .inspect_err(|e| tracing::debug!("cannot open output device {id}: {e}"))
            .ok()
            .flatten()
    }
}

/// At most this many devices are remembered for a later restore (the oldest is dropped).
const MAX_LOST: usize = 8;

/// A device we lowered, and what to put back.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    original: f32,
    /// The last volume we set (as read back).
    ducked: f32,
    id: String,
    /// The live session's entry: if AGT is killed, it may be in the middle of a fade, so
    /// anything between `ducked` and `original` counts as ours. A lost device's entry is
    /// exact: it's at the volume we left it at, or the user changed it.
    wide: bool,
}

impl Entry {
    /// Whether a volume of `v` is still ours to restore (see [`Entry::wide`]).
    fn ours(&self, v: f32) -> bool {
        if self.wide {
            v >= self.ducked - TOLERANCE && v <= self.original + TOLERANCE
        } else {
            (v - self.ducked).abs() <= TOLERANCE
        }
    }
}

/// The marker file: one [`Entry`] per line, `original ducked wide|exact id`, oldest first
/// (`None` in tests: no file).
struct Marker(Option<PathBuf>);

impl Marker {
    fn save<'a>(&self, entries: impl IntoIterator<Item = &'a Entry>) {
        let Some(p) = &self.0 else { return };
        let text: String = entries
            .into_iter()
            .map(|e| {
                let range = if e.wide { "wide" } else { "exact" };
                format!("{} {} {range} {}\n", e.original, e.ducked, e.id)
            })
            .collect();
        let saved = if text.is_empty() {
            match fs::remove_file(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                r => r,
            }
        } else {
            fs::write(p, text)
        };
        if let Err(e) = saved {
            tracing::debug!("cannot update {}: {e}", p.display());
        }
    }

    fn load(&self) -> Vec<Entry> {
        let Some(text) = self.0.as_ref().and_then(|p| fs::read_to_string(p).ok()) else {
            return Vec::new();
        };
        let mut entries: Vec<Entry> = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(4, ' ');
                Some(Entry {
                    original: parts.next()?.parse().ok()?,
                    ducked: parts.next()?.parse().ok()?,
                    wide: match parts.next()? {
                        "wide" => true,
                        "exact" => false,
                        _ => return None,
                    },
                    id: parts.next().filter(|id| !id.is_empty())?.to_owned(),
                })
            })
            .collect();
        // The newest last (the session's): keep those.
        entries.drain(..entries.len().saturating_sub(MAX_LOST));
        entries
    }
}

/// What a device's volume says about the session.
#[derive(Debug, PartialEq)]
enum Seen {
    /// Still the last volume we set.
    Untouched,
    /// The user changed it: leave it alone.
    Changed,
    /// It can't be read: the device went away (or isn't answering).
    Unreadable,
}

/// The volume we lowered, from the first duck until it's restored (or the user took over).
struct Session<O> {
    out: O,
    /// `original`: the volume before the first duck, what restoring goes back to.
    entry: Entry,
    /// The volume after our last set, as read back (a device may round to its own steps).
    set: f32,
}

impl<O: Output> Session<O> {
    fn seen(&mut self) -> Seen {
        match self.out.volume() {
            None => Seen::Unreadable,
            Some(v) if (v - self.set).abs() <= TOLERANCE => Seen::Untouched,
            Some(_) => Seen::Changed,
        }
    }

    fn set(&mut self, v: f32) -> bool {
        let ok = self.out.set_volume(v);
        if ok {
            self.set = self.out.volume().unwrap_or(v);
        }
        ok
    }
}

struct Fade {
    from: f32,
    to: f32,
    start: Instant,
    len: Duration,
    /// Back to the original: the session ends when it's done.
    restoring: bool,
}

impl Fade {
    /// The volume at `now`, and whether the fade is done.
    fn at(&self, now: Instant) -> (f32, bool) {
        let t = if self.len.is_zero() {
            1.0
        } else {
            now.saturating_duration_since(self.start).as_secs_f32() / self.len.as_secs_f32()
        };
        if t >= 1.0 {
            (self.to, true)
        } else {
            (self.from + (self.to - self.from) * t, false)
        }
    }
}

/// The ducking state machine, without the thread (see [`run`]).
struct Duck<D: Devices> {
    devices: D,
    session: Option<Session<D::Out>>,
    /// Devices still to restore once they're back: they went away while ducked, or a previous
    /// run exited before restoring them.
    lost: Vec<Entry>,
    fade: Option<Fade>,
    /// When the next fade step is due.
    next: Option<Instant>,
    marker: Marker,
    fade_down: Duration,
    fade_up: Duration,
}

impl<D: Devices> Duck<D> {
    /// Starts with what the marker file says a previous run left lowered, restoring what it
    /// can right away.
    fn new(devices: D, marker: Marker, fade_down: Duration, fade_up: Duration) -> Self {
        let mut duck = Self {
            devices,
            session: None,
            lost: marker.load(),
            fade: None,
            next: None,
            marker,
            fade_down,
            fade_up,
        };
        duck.retry_lost();
        duck.save();
        duck
    }

    fn save(&self) {
        let session = self.session.as_ref().map(|s| &s.entry);
        self.marker.save(self.lost.iter().chain(session));
    }

    /// Restores each lost device that is back. One that's back with a volume that isn't ours
    /// (the user changed it) is forgotten; one that refuses the set is kept.
    fn retry_lost(&mut self) {
        let before = self.lost.len();
        let devices = &mut self.devices;
        self.lost.retain(|e| {
            let Some(mut out) = devices.open(&e.id) else {
                return true;
            };
            let Some(v) = out.volume() else { return true };
            if !e.ours(v) {
                return false;
            }
            let restored = out.set_volume(e.original);
            if restored {
                tracing::info!(
                    "restored the output volume of a device left lowered (AGT exited, or the device went away, while ducked)"
                );
            }
            !restored
        });
        if self.lost.len() != before {
            self.save();
        }
    }

    /// Lowers the volume to `level` × the original.
    fn duck(&mut self, level: f32, now: Instant) {
        let level = clamp_level(level);
        // Still ducked, or fading back up: keep the original from before the first duck.
        match self.session.as_mut().map(Session::seen) {
            Some(Seen::Untouched) | None => {}
            // The user changed it since: that's the new original.
            Some(Seen::Changed) => self.end(),
            Some(Seen::Unreadable) => self.lose(),
        }
        if self.session.is_none() {
            self.open_session(level);
        }
        // After the session took over its own device's entry: that one is never set to its
        // original here, which would blip it to full volume just before it fades down.
        self.retry_lost();
        let Some(s) = &self.session else { return };
        let to = s.entry.original * level;
        tracing::debug!("ducking other audio: volume {:.3} -> {to:.3}", s.set);
        self.start_fade(to, self.fade_down, false, now);
    }

    /// Starts a session on the default output, unless there's nothing to lower.
    fn open_session(&mut self, level: f32) {
        let Some(mut out) = self.devices.open_default() else {
            return;
        };
        let Some(v) = out.volume() else { return };
        if out.muted() {
            return;
        }
        let id = out.id();
        // A device still lost to us that is the default again: its original stands.
        let lost = self.lost.iter().position(|e| e.id == id && e.ours(v));
        let original = lost.map_or(v, |i| self.lost[i].original);
        if original - original * level <= TOLERANCE {
            return; // already silent, or a level of (nearly) 1: nothing to lower
        }
        if let Some(i) = lost {
            self.lost.remove(i);
        }
        self.session = Some(Session {
            out,
            entry: Entry {
                original,
                ducked: original * level, // until the fade down is done: then as read back
                id,
                wide: true,
            },
            set: v,
        });
        self.save();
    }

    /// Fades back to the original volume, unless the user changed it meanwhile.
    fn restore(&mut self, now: Instant) {
        let Some(s) = &mut self.session else { return };
        match s.seen() {
            Seen::Untouched => {}
            Seen::Changed => {
                tracing::debug!("output volume changed while ducked; leaving it");
                return self.end();
            }
            Seen::Unreadable => return self.lose(),
        }
        let to = s.entry.original;
        tracing::debug!("restoring other audio: volume {:.3} -> {to:.3}", s.set);
        self.start_fade(to, self.fade_up, true, now);
    }

    fn start_fade(&mut self, to: f32, len: Duration, restoring: bool, now: Instant) {
        let from = self.session.as_ref().expect("a session").set;
        self.fade = Some(Fade {
            from,
            to,
            start: now,
            len,
            restoring,
        });
        // The first step would set the volume it's at: take the next one.
        if len.is_zero() {
            self.tick(now);
        } else {
            self.next = Some(now + STEP);
        }
    }

    /// Takes the fade's next step (due at [`Self::next`]).
    fn tick(&mut self, now: Instant) {
        self.next = None;
        let (Some(s), Some(fade)) = (&mut self.session, &self.fade) else {
            return;
        };
        match s.seen() {
            Seen::Untouched => {}
            Seen::Changed => {
                tracing::debug!("output volume changed while ducking; leaving it");
                return self.end();
            }
            Seen::Unreadable => return self.lose(),
        }
        let (v, done) = fade.at(now);
        if !s.set(v) {
            return self.lose();
        }
        if !done {
            self.next = Some(now + STEP);
        } else if fade.restoring {
            self.end();
        } else {
            // What the device really is at, for a restore after a crash.
            s.entry.ducked = s.set;
            self.fade = None;
            self.save();
        }
    }

    /// Restores the original volume at once (no fade), e.g. when AGT exits.
    fn finish(&mut self) {
        let Some(s) = &mut self.session else { return };
        match s.seen() {
            Seen::Untouched => {
                let original = s.entry.original;
                if s.set(original) {
                    self.end();
                } else {
                    self.lose();
                }
            }
            Seen::Changed => self.end(),
            Seen::Unreadable => self.lose(),
        }
    }

    /// The session is over: restored, or the user took over.
    fn end(&mut self) {
        self.session = None;
        self.fade = None;
        self.next = None;
        self.save();
    }

    /// The session's device can't be read or set: keep its entry, to restore it once it's
    /// back.
    fn lose(&mut self) {
        if let Some(s) = self.session.take() {
            tracing::info!(
                "output device unavailable while ducked; its volume is restored once it's back"
            );
            if self.lost.len() == MAX_LOST {
                self.lost.remove(0);
            }
            // Restored only if it comes back at exactly the volume we left it at.
            self.lost.push(Entry {
                ducked: s.set,
                wide: false,
                ..s.entry
            });
        }
        self.end();
    }
}

enum Cmd {
    Duck(f32),
    Restore,
    /// Restore at once and stop; the sender is dropped when done.
    Shutdown(Sender<()>),
}

/// Handle to the `duck` thread. Cheap to clone.
#[derive(Clone)]
pub struct Ducker {
    tx: Sender<Cmd>,
}

/// While alive, other audio is ducked; dropping it fades the volume back.
#[must_use = "dropping it restores the volume right away"]
pub struct Ducked {
    tx: Sender<Cmd>,
}

impl Drop for Ducked {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Restore);
    }
}

impl Ducker {
    /// Starts the `duck` thread. `marker`: where to keep the original volume while ducked; a
    /// marker left by a previous run is acted on first.
    pub fn start(marker: PathBuf) -> Self {
        Self::spawn(move || {
            imp::init_volume_thread();
            Duck::new(Real::default(), Marker(Some(marker)), FADE_DOWN, FADE_UP)
        })
    }

    /// `make` runs on the new thread (the OS volume objects belong to the thread using them).
    fn spawn<D: Devices>(make: impl FnOnce() -> Duck<D> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("duck".into())
            .spawn(move || run(rx, make()));
        if let Err(e) = spawned {
            tracing::warn!("cannot start the duck thread ({e}); other audio won't be lowered");
        }
        Self { tx }
    }

    /// Lowers the output volume to `level` (0..=1) times what it is now, until the returned
    /// guard is dropped. Never blocks.
    pub fn duck(&self, level: f32) -> Ducked {
        let _ = self.tx.send(Cmd::Duck(level));
        Ducked {
            tx: self.tx.clone(),
        }
    }

    /// Restores a ducked volume at once and stops the thread; waits up to `wait` for it.
    pub fn shutdown(&self, wait: Duration) {
        let (done, finished) = mpsc::channel();
        if self.tx.send(Cmd::Shutdown(done)).is_ok() {
            // Disconnected (not a message) once the thread has restored.
            let _ = finished.recv_timeout(wait);
        }
    }
}

fn run<D: Devices>(rx: Receiver<Cmd>, mut duck: Duck<D>) {
    loop {
        let cmd = match duck.next {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            },
        };
        let now = Instant::now();
        match cmd {
            None => duck.tick(now),
            Some(Cmd::Duck(level)) => duck.duck(level, now),
            Some(Cmd::Restore) => duck.restore(now),
            Some(Cmd::Shutdown(done)) => {
                duck.finish();
                drop(done); // only now: `shutdown` waits for this
                return;
            }
        }
    }
    duck.finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A volume control; clones share it, as with the real device.
    #[derive(Clone)]
    struct Fake(Arc<Mutex<FakeState>>);

    struct FakeState {
        id: String,
        volume: f32,
        muted: bool,
        /// Unplugged: can't be read, set or opened.
        gone: bool,
        /// The device rounds what's set to multiples of this (0: exact).
        grid: f32,
        /// How long a set takes.
        set_delay: Duration,
        /// Sets fail (a device still starting up).
        refuses: bool,
        sets: usize,
        /// The highest volume set so far.
        peak: f32,
    }

    impl Fake {
        fn new(id: &str, volume: f32) -> Self {
            Self(Arc::new(Mutex::new(FakeState {
                id: id.into(),
                volume,
                muted: false,
                gone: false,
                grid: 0.0,
                set_delay: Duration::ZERO,
                refuses: false,
                sets: 0,
                peak: 0.0,
            })))
        }
        fn s(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.0.lock().unwrap()
        }
        fn get(&self) -> f32 {
            self.s().volume
        }
        /// The user moving the slider.
        fn user_sets(&self, v: f32) {
            self.s().volume = v;
        }
        fn sets(&self) -> usize {
            self.s().sets
        }
    }

    impl Output for Fake {
        fn id(&self) -> String {
            self.s().id.clone()
        }
        fn volume(&mut self) -> Option<f32> {
            let s = self.s();
            (!s.gone).then_some(s.volume)
        }
        fn muted(&mut self) -> bool {
            self.s().muted
        }
        fn set_volume(&mut self, v: f32) -> bool {
            let delay = self.s().set_delay;
            thread::sleep(delay);
            let mut s = self.s();
            if s.gone || s.refuses {
                return false;
            }
            s.volume = if s.grid > 0.0 {
                (v / s.grid).round() * s.grid
            } else {
                v
            };
            s.sets += 1;
            s.peak = s.peak.max(s.volume);
            true
        }
    }

    /// Some devices; `default` is the default output.
    struct FakeDevices {
        all: Vec<Fake>,
        default: usize,
    }

    impl FakeDevices {
        fn new(all: Vec<Fake>) -> Self {
            Self { all, default: 0 }
        }
    }

    impl Devices for FakeDevices {
        type Out = Fake;
        fn open_default(&mut self) -> Option<Fake> {
            let d = self.all.get(self.default)?;
            (!d.s().gone).then(|| d.clone())
        }
        fn open(&mut self, id: &str) -> Option<Fake> {
            self.all
                .iter()
                .find(|d| d.s().id == id && !d.s().gone)
                .cloned()
        }
    }

    fn one(out: &Fake) -> FakeDevices {
        FakeDevices::new(vec![out.clone()])
    }

    /// No fades: every step lands at once.
    fn instant(out: &Fake) -> Duck<FakeDevices> {
        Duck::new(one(out), Marker(None), Duration::ZERO, Duration::ZERO)
    }

    /// Runs the fade in progress to its end.
    fn settle(d: &mut Duck<FakeDevices>) {
        while let Some(at) = d.next {
            d.tick(at);
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    fn temp_marker(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agt-duck-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("ducked-volume")
    }

    #[test]
    fn ducks_and_restores_exactly() {
        let out = Fake::new("a", 0.62);
        let mut d = instant(&out);
        let now = Instant::now();
        d.duck(0.1, now);
        assert!(close(out.get(), 0.062));
        d.restore(now);
        assert_eq!(out.get(), 0.62);
        assert!(d.session.is_none());
    }

    #[test]
    fn a_volume_the_user_changed_is_left_alone() {
        let out = Fake::new("a", 0.5);
        let mut d = instant(&out);
        let now = Instant::now();
        d.duck(0.1, now);
        out.user_sets(0.3);
        d.restore(now);
        assert_eq!(out.get(), 0.3);
        assert!(d.session.is_none());
        // Also when it happens mid-fade.
        let mut d = Duck::new(
            one(&out),
            Marker(None),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        out.user_sets(0.5);
        d.duck(0.1, now);
        out.user_sets(0.8);
        d.tick(now + Duration::from_millis(500));
        assert_eq!(out.get(), 0.8);
        assert!(d.session.is_none());
        // And on exit.
        let mut d = instant(&out);
        d.duck(0.1, now);
        out.user_sets(0.2);
        d.finish();
        assert_eq!(out.get(), 0.2);
    }

    #[test]
    fn overlapping_dictations_do_not_ratchet() {
        let out = Fake::new("a", 0.8);
        let mut d = Duck::new(
            one(&out),
            Marker(None),
            Duration::ZERO,
            Duration::from_millis(100),
        );
        let now = Instant::now();
        d.duck(0.5, now);
        assert!(close(out.get(), 0.4));
        // Pressed again halfway through the fade back up...
        d.restore(now);
        d.tick(now + Duration::from_millis(50));
        assert!(out.get() > 0.4 && out.get() < 0.8);
        d.duck(0.5, now);
        // ...ducks to half the original, not half the current volume.
        assert!(close(out.get(), 0.4));
        // Pressed again with no restore in between (a never-released hold): same.
        d.duck(0.5, now);
        assert!(close(out.get(), 0.4));
        d.restore(now);
        d.tick(now + Duration::from_millis(200));
        assert_eq!(out.get(), 0.8);
        assert!(d.session.is_none());
    }

    #[test]
    fn fades_in_steps() {
        let out = Fake::new("a", 1.0);
        let mut d = Duck::new(one(&out), Marker(None), FADE_DOWN, FADE_UP);
        let now = Instant::now();
        d.duck(0.0, now);
        // No step at the start (it would set the volume it's at).
        assert_eq!(out.sets(), 0);
        assert_eq!(d.next, Some(now + STEP));
        d.tick(now + FADE_DOWN / 2);
        assert!(close(out.get(), 0.5));
        d.tick(now + FADE_DOWN);
        assert_eq!(out.get(), 0.0);
        assert_eq!(d.next, None); // stays ducked
        assert!(d.session.is_some());
        d.restore(now + FADE_DOWN);
        settle(&mut d);
        assert_eq!(out.get(), 1.0);
        assert!(d.session.is_none());
    }

    #[test]
    fn a_device_that_rounds_the_volume_is_not_a_user_change() {
        let out = Fake::new("a", 0.6);
        out.s().grid = 0.05;
        let mut d = Duck::new(one(&out), Marker(None), FADE_DOWN, FADE_UP);
        let now = Instant::now();
        d.duck(0.1, now);
        settle(&mut d);
        assert!(
            d.session.is_some(),
            "every step read back off the requested value"
        );
        assert!(out.get() < 0.1);
        d.restore(now);
        settle(&mut d);
        assert!(close(out.get(), 0.6));
        assert!(d.session.is_none());

        // The marker holds the volume it really ended at, so a crash is still recovered.
        let path = temp_marker("grid");
        let mut d = Duck::new(one(&out), Marker(Some(path.clone())), FADE_DOWN, FADE_UP);
        d.duck(0.1, now);
        settle(&mut d);
        let entry = &Marker(Some(path.clone())).load()[0];
        assert!(close(entry.ducked, out.get()), "{entry:?}");
        std::mem::forget(d);
        drop(Duck::new(
            one(&out),
            Marker(Some(path.clone())),
            FADE_DOWN,
            FADE_UP,
        ));
        assert!(close(out.get(), 0.6));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn muted_or_already_low_output_is_left_alone() {
        let now = Instant::now();
        let muted = Fake::new("a", 0.7);
        muted.s().muted = true;
        let mut d = instant(&muted);
        d.duck(0.1, now);
        d.restore(now);
        assert_eq!(muted.sets(), 0);

        let silent = Fake::new("a", 0.0);
        let mut d = instant(&silent);
        d.duck(0.1, now);
        assert!(d.session.is_none());
        let full = Fake::new("a", 0.5);
        let mut d = instant(&full);
        d.duck(1.0, now);
        assert!(d.session.is_none());
        assert_eq!(silent.sets() + full.sets(), 0);

        // No settable output (HDMI): nothing happens.
        let mut d = Duck::new(
            FakeDevices::new(Vec::new()),
            Marker(None),
            Duration::ZERO,
            Duration::ZERO,
        );
        d.duck(0.1, now);
        d.restore(now);
        assert!(d.session.is_none());
    }

    #[test]
    fn level_is_clamped() {
        assert_eq!(clamp_level(-1.0), 0.0);
        assert_eq!(clamp_level(2.0), 1.0);
        assert_eq!(clamp_level(f32::NAN), 0.0);
        assert_eq!(clamp_level(0.25), 0.25);
        let out = Fake::new("a", 0.6);
        let mut d = instant(&out);
        d.duck(-3.0, Instant::now());
        assert_eq!(out.get(), 0.0);
    }

    #[test]
    fn marker_lines_round_trip() {
        let path = temp_marker("lines");
        let m = Marker(Some(path.clone()));
        let entries = vec![
            Entry {
                original: 0.75,
                ducked: 0.075,
                id: "{0.0.0.00000000}.{guid}".into(),
                wide: false,
            },
            Entry {
                original: 0.5,
                ducked: 0.0,
                id: "AppleUSBAudioEngine:My Headset:1".into(),
                wide: true,
            },
        ];
        m.save(&entries);
        assert_eq!(m.load(), entries);
        m.save([]);
        assert!(!path.exists());
        assert!(m.load().is_empty());
        // Lines without an ID or a range can't be used: dropped.
        fs::write(
            &path,
            "0.5 0.05 wide\n0.5 0.05 exact \n0.5 0.05 x\n0.7 0.07 wide x\n",
        )
        .unwrap();
        assert_eq!(m.load().len(), 1);
        // More than fit: the newest (last) are kept.
        let many: Vec<Entry> = (0..=MAX_LOST)
            .map(|i| Entry {
                id: i.to_string(),
                ..entries[0].clone()
            })
            .collect();
        m.save(&many);
        assert_eq!(m.load(), many[1..]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_device_lost_before_a_restart_is_still_restored_only_at_our_volume() {
        let path = temp_marker("lost-restart");
        let marker = || Marker(Some(path.clone()));
        let headset = Fake::new("headset", 0.6);
        let speakers = Fake::new("speakers", 0.5);
        let devices = || FakeDevices::new(vec![headset.clone(), speakers.clone()]);
        let now = Instant::now();
        let mut d = Duck::new(devices(), marker(), Duration::ZERO, Duration::ZERO);
        d.duck(0.1, now);
        headset.s().gone = true;
        d.restore(now);
        drop(d);
        // Back after a restart, turned up by the user to 0.3: theirs, not restored.
        headset.s().gone = false;
        headset.user_sets(0.3);
        drop(Duck::new(
            devices(),
            marker(),
            Duration::ZERO,
            Duration::ZERO,
        ));
        assert_eq!(headset.get(), 0.3);
        assert!(!path.exists());
        // At exactly the volume we left it at: restored.
        let mut d = Duck::new(devices(), marker(), Duration::ZERO, Duration::ZERO);
        headset.user_sets(0.6);
        d.duck(0.1, now);
        headset.s().gone = true;
        d.restore(now);
        drop(d);
        headset.s().gone = false;
        drop(Duck::new(
            devices(),
            marker(),
            Duration::ZERO,
            Duration::ZERO,
        ));
        assert!(close(headset.get(), 0.6));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_crash_while_ducked_is_recovered_on_the_next_start() {
        let path = temp_marker("crash");
        let marker = || Marker(Some(path.clone()));
        let now = Instant::now();
        let start = |out: &Fake| Duck::new(one(out), marker(), Duration::ZERO, Duration::ZERO);

        // Ducked, then "crashed" (never restored): the file holds the original.
        let out = Fake::new("a", 0.75);
        let mut d = start(&out);
        d.duck(0.2, now);
        assert!(path.exists());
        std::mem::forget(d);
        let d = start(&out);
        assert_eq!(out.get(), 0.75);
        assert!(!path.exists());
        drop(d);

        // Killed mid-fade: between the ducked value and the original still counts.
        let mut d = start(&out);
        d.duck(0.2, now);
        std::mem::forget(d);
        out.user_sets(0.4);
        let d = start(&out);
        assert_eq!(out.get(), 0.75);
        drop(d);

        // Changed by the user since (above the original): left alone, file gone.
        let mut d = start(&out);
        d.duck(0.2, now);
        std::mem::forget(d);
        out.user_sets(0.9);
        let d = start(&out);
        assert_eq!(out.get(), 0.9);
        assert!(!path.exists());
        drop(d);

        // A normal restore removes the file.
        let mut d = start(&out);
        d.duck(0.2, now);
        d.restore(now);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_device_that_goes_away_while_ducked_is_restored_once_back() {
        let path = temp_marker("lost");
        let marker = || Marker(Some(path.clone()));
        let now = Instant::now();
        let headset = Fake::new("headset", 0.6);
        let speakers = Fake::new("speakers", 0.5);
        let devices = || FakeDevices::new(vec![headset.clone(), speakers.clone()]);
        let mut d = Duck::new(devices(), marker(), Duration::ZERO, Duration::ZERO);
        d.duck(0.1, now);
        assert!(close(headset.get(), 0.06));
        // Switched off mid-hold (Windows and macOS keep its volume at 0.06).
        headset.s().gone = true;
        d.restore(now);
        assert!(d.session.is_none());
        let kept = marker().load();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "headset");

        // While it's gone the default is the speakers: ducked and restored as usual, and the
        // headset's entry stays.
        d.devices.default = 1;
        d.duck(0.1, now);
        d.restore(now);
        assert_eq!(speakers.get(), 0.5);
        assert_eq!(marker().load(), kept);

        // AGT restarts while it's still gone: still kept.
        drop(d);
        let mut d = Duck::new(devices(), marker(), Duration::ZERO, Duration::ZERO);
        d.devices.default = 1;
        assert_eq!(marker().load(), kept);

        // Back (the speakers still the default): restored on the next dictation.
        headset.s().gone = false;
        d.duck(0.1, now);
        assert!(close(headset.get(), 0.6));
        d.restore(now);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The headset ducked to 0.06 (original 0.6) and switched off mid-hold.
    fn lost_headset() -> (Fake, Duck<FakeDevices>) {
        let headset = Fake::new("headset", 0.6);
        let mut d = instant(&headset);
        let now = Instant::now();
        d.duck(0.1, now);
        headset.s().gone = true;
        d.restore(now);
        assert_eq!(d.lost.len(), 1);
        headset.s().gone = false;
        headset.s().peak = 0.0;
        (headset, d)
    }

    #[test]
    fn a_lost_device_back_as_the_default_keeps_its_original_without_a_blip() {
        let (headset, mut d) = lost_headset();
        let now = Instant::now();
        d.duck(0.1, now);
        // Never set to its original right before fading down again.
        assert!(headset.s().peak < 0.07, "{}", headset.s().peak);
        assert!(close(d.session.as_ref().unwrap().entry.original, 0.6));
        assert!(d.lost.is_empty());
        d.restore(now);
        assert!(close(headset.get(), 0.6));
    }

    #[test]
    fn a_volume_the_user_set_on_a_returning_device_stands() {
        // The default again: ducked from the user's volume, and back to it.
        let (headset, mut d) = lost_headset();
        headset.user_sets(0.3);
        let now = Instant::now();
        d.duck(0.1, now);
        assert!(close(headset.get(), 0.03));
        d.restore(now);
        assert!(close(headset.get(), 0.3));
        assert!(d.lost.is_empty());

        // Not the default: left alone, and forgotten.
        let (headset, mut d) = lost_headset();
        headset.user_sets(0.3);
        let speakers = Fake::new("speakers", 0.5);
        d.devices.all.push(speakers.clone());
        d.devices.default = 1;
        d.duck(0.1, now);
        d.restore(now);
        assert_eq!(headset.get(), 0.3);
        assert!(d.lost.is_empty());
        assert_eq!(speakers.get(), 0.5);
    }

    #[test]
    fn a_lost_device_that_refuses_the_restore_is_kept() {
        let (headset, mut d) = lost_headset();
        d.devices.all.push(Fake::new("speakers", 0.5));
        d.devices.default = 1;
        headset.s().refuses = true;
        let now = Instant::now();
        d.duck(0.1, now);
        d.restore(now);
        assert_eq!(d.lost.len(), 1);
        assert!(close(headset.get(), 0.06));
        headset.s().refuses = false;
        d.duck(0.1, now);
        d.restore(now);
        assert!(d.lost.is_empty());
        assert!(close(headset.get(), 0.6));
    }

    #[test]
    fn at_most_a_few_lost_devices_are_remembered() {
        let all: Vec<Fake> = (0..10).map(|i| Fake::new(&i.to_string(), 0.5)).collect();
        let mut d = Duck::new(
            FakeDevices::new(all.clone()),
            Marker(None),
            Duration::ZERO,
            Duration::ZERO,
        );
        let now = Instant::now();
        for (i, dev) in all.iter().enumerate() {
            d.devices.default = i;
            d.duck(0.1, now);
            dev.s().gone = true;
            d.restore(now);
        }
        let ids: Vec<&str> = d.lost.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["2", "3", "4", "5", "6", "7", "8", "9"]);
    }

    #[test]
    fn shutdown_returns_after_the_volume_is_restored() {
        let out = Fake::new("a", 0.8);
        let fake = out.clone();
        let ducker = Ducker::spawn(move || {
            Duck::new(one(&fake), Marker(None), Duration::ZERO, Duration::ZERO)
        });
        let guard = ducker.duck(0.1);
        let t = Instant::now();
        while !close(out.get(), 0.08) {
            assert!(t.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(5));
        }
        out.s().set_delay = Duration::from_millis(100);
        ducker.shutdown(Duration::from_secs(5));
        assert_eq!(out.get(), 0.8);
        drop(guard);
    }

    /// Ducks this machine's real default output and checks the volume comes back exactly.
    /// Changes the system volume for about a second: run by hand with
    /// `cargo test -p agt-platform real_output -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_output_is_ducked_and_restored() {
        imp::init_volume_thread();
        let Some(out) = imp::OutputVolume::open_default().unwrap() else {
            panic!("the default output's volume can't be set");
        };
        let before = out.volume().unwrap();
        println!(
            "device {}: before {before} (muted: {})",
            out.id(),
            out.muted().unwrap()
        );
        let again = imp::OutputVolume::open(out.id())
            .unwrap()
            .expect("found by ID");
        assert_eq!(again.id(), out.id());
        let path = temp_marker("real");
        let ducker = Ducker::start(path.clone());
        let guard = ducker.duck(0.1);
        thread::sleep(Duration::from_millis(300));
        let during = out.volume().unwrap();
        println!("during: {during}");
        println!("marker: {:?}", fs::read_to_string(&path).ok());
        drop(guard);
        thread::sleep(Duration::from_millis(500));
        let after = out.volume().unwrap();
        println!("after:  {after}");
        ducker.shutdown(Duration::from_secs(1));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
        assert!((during - before * 0.1).abs() <= TOLERANCE, "{during}");
        assert_eq!(after, before);
    }
}
