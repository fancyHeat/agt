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
//! - While ducked, the original volume is kept in a small marker file; if AGT dies before
//!   restoring, the next start restores it (if the volume is still the ducked one).

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
/// How close the volume must be to the one we set to count as untouched by the user: well
/// under the smallest step a volume key makes (Windows: 0.02; macOS: 1/16 or 1/64), well
/// over the rounding of a set-then-get.
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
    fn volume(&mut self) -> Option<f32>;
    fn muted(&mut self) -> bool;
    fn set_volume(&mut self, v: f32) -> bool;
}

impl Output for imp::OutputVolume {
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

/// Where the original volume is kept while ducked (`None` in tests: no file).
struct Marker(Option<PathBuf>);

impl Marker {
    fn write(&self, original: f32, ducked: f32) {
        if let Some(p) = &self.0
            && let Err(e) = fs::write(p, format!("{original} {ducked}\n"))
        {
            tracing::debug!("cannot write {}: {e}", p.display());
        }
    }

    fn remove(&self) {
        if let Some(p) = &self.0 {
            let _ = fs::remove_file(p);
        }
    }

    /// `(original, ducked)` from a previous run, if any.
    fn read(&self) -> Option<(f32, f32)> {
        let text = fs::read_to_string(self.0.as_ref()?).ok()?;
        let mut parts = text.split_whitespace().map(str::parse::<f32>);
        match (parts.next(), parts.next()) {
            (Some(Ok(original)), Some(Ok(ducked))) => Some((original, ducked)),
            _ => None,
        }
    }
}

/// The volume we lowered, from the first duck until it's restored (or the user took over).
struct Session<O> {
    out: O,
    /// The volume before the first duck: what restoring goes back to.
    original: f32,
    /// The last volume we set.
    set: f32,
}

impl<O: Output> Session<O> {
    /// Whether the volume is still the last one we set (the user hasn't changed it).
    fn untouched(&mut self) -> bool {
        self.out
            .volume()
            .is_some_and(|v| (v - self.set).abs() <= TOLERANCE)
    }

    fn set(&mut self, v: f32) -> bool {
        let ok = self.out.set_volume(v);
        if ok {
            self.set = v;
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
struct Duck<O> {
    session: Option<Session<O>>,
    fade: Option<Fade>,
    /// When the next fade step is due.
    next: Option<Instant>,
    marker: Marker,
    fade_down: Duration,
    fade_up: Duration,
}

impl<O: Output> Duck<O> {
    fn new(marker: Marker, fade_down: Duration, fade_up: Duration) -> Self {
        Self {
            session: None,
            fade: None,
            next: None,
            marker,
            fade_down,
            fade_up,
        }
    }

    /// Lowers the volume to `level` × the original. `open` opens the default output (`None`:
    /// there's none whose volume can be set).
    fn duck(&mut self, level: f32, open: impl FnOnce() -> Option<O>, now: Instant) {
        let level = clamp_level(level);
        // Still ducked, or fading back up: keep the original from before the first duck.
        let kept = self.session.as_mut().is_some_and(Session::untouched);
        let session = match self.session.take() {
            Some(s) if kept => s,
            // None yet, or the user changed the volume since: that's the new original.
            _ => {
                self.marker.remove();
                self.end();
                let Some(mut out) = open() else { return };
                if out.muted() {
                    return;
                }
                let Some(v) = out.volume() else { return };
                if v - v * level <= TOLERANCE {
                    return; // already silent, or a level of (nearly) 1: nothing to lower
                }
                self.marker.write(v, v * level);
                Session {
                    out,
                    original: v,
                    set: v,
                }
            }
        };
        let to = session.original * level;
        tracing::debug!("ducking other audio: volume {:.3} -> {to:.3}", session.set);
        self.fade = Some(Fade {
            from: session.set,
            to,
            start: now,
            len: self.fade_down,
            restoring: false,
        });
        self.session = Some(session);
        self.tick(now);
    }

    /// Fades back to the original volume, unless the user changed it meanwhile.
    fn restore(&mut self, now: Instant) {
        let Some(s) = &mut self.session else { return };
        if !s.untouched() {
            tracing::debug!("output volume changed while ducked; leaving it");
            return self.end();
        }
        tracing::debug!(
            "restoring other audio: volume {:.3} -> {:.3}",
            s.set,
            s.original
        );
        self.fade = Some(Fade {
            from: s.set,
            to: s.original,
            start: now,
            len: self.fade_up,
            restoring: true,
        });
        self.tick(now);
    }

    /// Takes the fade's next step (due at [`Self::next`]).
    fn tick(&mut self, now: Instant) {
        self.next = None;
        let (Some(s), Some(fade)) = (&mut self.session, &self.fade) else {
            return;
        };
        if !s.untouched() {
            tracing::debug!("output volume changed while ducking; leaving it");
            return self.end();
        }
        let (v, done) = fade.at(now);
        if !s.set(v) {
            return self.end(); // e.g. the device went away
        }
        if !done {
            self.next = Some(now + STEP);
        } else if fade.restoring {
            self.end();
        } else {
            self.fade = None;
        }
    }

    /// Restores the original volume at once (no fade), e.g. when AGT exits.
    fn finish(&mut self) {
        if let Some(s) = &mut self.session
            && s.untouched()
        {
            let original = s.original;
            s.set(original);
        }
        self.end();
    }

    fn end(&mut self) {
        if self.session.take().is_some() {
            self.marker.remove();
        }
        self.fade = None;
        self.next = None;
    }
}

/// Restores a volume a previous run left ducked (it died before restoring), if it's still the
/// ducked one.
fn recover<O: Output>(marker: &Marker, open: impl FnOnce() -> Option<O>) -> bool {
    let Some((original, ducked)) = marker.read() else {
        marker.remove(); // unreadable: nothing to go on
        return false;
    };
    marker.remove();
    let Some(mut out) = open() else { return false };
    let restore = out
        .volume()
        .is_some_and(|v| (v - ducked).abs() <= TOLERANCE);
    restore && out.set_volume(original)
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
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("duck".into())
            .spawn(move || run(rx, Marker(Some(marker))));
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

fn run(rx: Receiver<Cmd>, marker: Marker) {
    imp::init_volume_thread();
    let mut said_unsupported = false;
    let mut open = || match imp::OutputVolume::open_default() {
        Ok(Some(out)) => Some(out),
        Ok(None) => {
            if !said_unsupported {
                said_unsupported = true;
                tracing::debug!("the default output's volume can't be set; not ducking");
            }
            None
        }
        Err(e) => {
            tracing::debug!("cannot open the default output's volume: {e}");
            None
        }
    };
    if recover(&marker, &mut open) {
        tracing::info!("restored the output volume a previous run left lowered");
    }
    let mut duck = Duck::new(marker, FADE_DOWN, FADE_UP);
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
            Some(Cmd::Duck(level)) => duck.duck(level, &mut open, now),
            Some(Cmd::Restore) => duck.restore(now),
            Some(Cmd::Shutdown(_done)) => break,
        }
    }
    duck.finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    /// A volume control; clones share it, as with the real device.
    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<FakeState>>);

    #[derive(Default)]
    struct FakeState {
        volume: f32,
        muted: bool,
        sets: usize,
    }

    impl Fake {
        fn new(volume: f32) -> Self {
            let f = Self::default();
            f.0.borrow_mut().volume = volume;
            f
        }
        fn get(&self) -> f32 {
            self.0.borrow().volume
        }
        /// The user moving the slider.
        fn user_sets(&self, v: f32) {
            self.0.borrow_mut().volume = v;
        }
    }

    impl Output for Fake {
        fn volume(&mut self) -> Option<f32> {
            Some(self.0.borrow().volume)
        }
        fn muted(&mut self) -> bool {
            self.0.borrow().muted
        }
        fn set_volume(&mut self, v: f32) -> bool {
            let mut s = self.0.borrow_mut();
            s.volume = v;
            s.sets += 1;
            true
        }
    }

    /// No fades: every step lands at once.
    fn instant() -> Duck<Fake> {
        Duck::new(Marker(None), Duration::ZERO, Duration::ZERO)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn ducks_and_restores_exactly() {
        let out = Fake::new(0.62);
        let mut d = instant();
        let now = Instant::now();
        d.duck(0.1, || Some(out.clone()), now);
        assert!(close(out.get(), 0.062));
        d.restore(now);
        assert_eq!(out.get(), 0.62);
        assert!(d.session.is_none());
    }

    #[test]
    fn a_volume_the_user_changed_is_left_alone() {
        let out = Fake::new(0.5);
        let mut d = instant();
        let now = Instant::now();
        d.duck(0.1, || Some(out.clone()), now);
        out.user_sets(0.3);
        d.restore(now);
        assert_eq!(out.get(), 0.3);
        assert!(d.session.is_none());
        // Also when it happens mid-fade.
        let mut d = Duck::new(Marker(None), Duration::from_secs(1), Duration::from_secs(1));
        out.user_sets(0.5);
        d.duck(0.1, || Some(out.clone()), now);
        out.user_sets(0.8);
        d.tick(now + Duration::from_millis(500));
        assert_eq!(out.get(), 0.8);
        assert!(d.session.is_none());
        // And on exit.
        let mut d = instant();
        d.duck(0.1, || Some(out.clone()), now);
        out.user_sets(0.2);
        d.finish();
        assert_eq!(out.get(), 0.2);
    }

    #[test]
    fn overlapping_dictations_do_not_ratchet() {
        let out = Fake::new(0.8);
        let mut d = Duck::new(Marker(None), Duration::ZERO, Duration::from_millis(100));
        let now = Instant::now();
        d.duck(0.5, || Some(out.clone()), now);
        assert!(close(out.get(), 0.4));
        // Pressed again halfway through the fade back up...
        d.restore(now);
        d.tick(now + Duration::from_millis(50));
        assert!(out.get() > 0.4 && out.get() < 0.8);
        d.duck(0.5, || panic!("the session continues"), now);
        // ...ducks to half the original, not half the current volume.
        assert!(close(out.get(), 0.4));
        // Pressed again with no restore in between (a never-released hold): same.
        d.duck(0.5, || panic!("the session continues"), now);
        assert!(close(out.get(), 0.4));
        d.restore(now);
        d.tick(now + Duration::from_millis(200));
        assert_eq!(out.get(), 0.8);
        assert!(d.session.is_none());
    }

    #[test]
    fn fades_in_steps() {
        let out = Fake::new(1.0);
        let mut d = Duck::new(Marker(None), FADE_DOWN, FADE_UP);
        let now = Instant::now();
        d.duck(0.0, || Some(out.clone()), now);
        assert_eq!(out.get(), 1.0); // the first step is the starting volume
        assert_eq!(d.next, Some(now + STEP));
        d.tick(now + FADE_DOWN / 2);
        assert!(close(out.get(), 0.5));
        d.tick(now + FADE_DOWN);
        assert_eq!(out.get(), 0.0);
        assert_eq!(d.next, None); // stays ducked
        assert!(d.session.is_some());
    }

    #[test]
    fn muted_or_already_low_output_is_left_alone() {
        let now = Instant::now();
        let muted = Fake::new(0.7);
        muted.0.borrow_mut().muted = true;
        let mut d = instant();
        d.duck(0.1, || Some(muted.clone()), now);
        d.restore(now);
        assert_eq!(muted.0.borrow().sets, 0);

        let silent = Fake::new(0.0);
        d.duck(0.1, || Some(silent.clone()), now);
        assert!(d.session.is_none());
        let full = Fake::new(0.5);
        d.duck(1.0, || Some(full.clone()), now);
        assert!(d.session.is_none());
        assert_eq!(silent.0.borrow().sets + full.0.borrow().sets, 0);

        // No settable output (HDMI): nothing happens.
        d.duck(0.1, || None, now);
        d.restore(now);
        assert!(d.session.is_none());
    }

    #[test]
    fn level_is_clamped() {
        assert_eq!(clamp_level(-1.0), 0.0);
        assert_eq!(clamp_level(2.0), 1.0);
        assert_eq!(clamp_level(f32::NAN), 0.0);
        assert_eq!(clamp_level(0.25), 0.25);
        let out = Fake::new(0.6);
        let mut d = instant();
        d.duck(-3.0, || Some(out.clone()), Instant::now());
        assert_eq!(out.get(), 0.0);
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
        println!("before: {before} (muted: {})", out.muted().unwrap());
        let dir = std::env::temp_dir().join(format!("agt-duck-real-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ducker = Ducker::start(dir.join("ducked-volume"));
        let guard = ducker.duck(0.1);
        thread::sleep(Duration::from_millis(300));
        let during = out.volume().unwrap();
        println!("during: {during}");
        drop(guard);
        thread::sleep(Duration::from_millis(500));
        let after = out.volume().unwrap();
        println!("after:  {after}");
        ducker.shutdown(Duration::from_secs(1));
        let _ = fs::remove_dir_all(&dir);
        assert!((during - before * 0.1).abs() <= TOLERANCE, "{during}");
        assert_eq!(after, before);
    }

    #[test]
    fn a_crash_while_ducked_is_recovered_on_the_next_start() {
        let dir = std::env::temp_dir().join(format!("agt-duck-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ducked-volume");
        let marker = || Marker(Some(path.clone()));
        let now = Instant::now();

        // Ducked, then "crashed" (never restored): the file holds the original.
        let out = Fake::new(0.75);
        let mut d = Duck::new(marker(), Duration::ZERO, Duration::ZERO);
        d.duck(0.2, || Some(out.clone()), now);
        assert!(path.exists());
        std::mem::forget(d);
        assert!(recover(&marker(), || Some(out.clone())));
        assert_eq!(out.get(), 0.75);
        assert!(!path.exists());

        // Changed by the user since: left alone, and the file is gone either way.
        let mut d = Duck::new(marker(), Duration::ZERO, Duration::ZERO);
        d.duck(0.2, || Some(out.clone()), now);
        std::mem::forget(d);
        out.user_sets(0.4);
        assert!(!recover(&marker(), || Some(out.clone())));
        assert_eq!(out.get(), 0.4);
        assert!(!path.exists());

        // A normal restore removes the file.
        let mut d = Duck::new(marker(), Duration::ZERO, Duration::ZERO);
        d.duck(0.2, || Some(out.clone()), now);
        d.restore(now);
        assert!(!path.exists());
        assert!(!recover(&marker(), || -> Option<Fake> {
            panic!("no marker")
        }));
        let _ = fs::remove_dir_all(&dir);
    }
}
