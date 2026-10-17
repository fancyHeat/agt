//! macOS system audio: a *global* Core Audio process tap (everything every process plays,
//! mixed to mono) on a private aggregate device, which cpal then opens as an ordinary input.
//!
//! Not cpal's own loopback (`build_input_stream` on an output device): that taps one output
//! device, so it has to be rebuilt when the output changes (AirPods connecting), and on a
//! device that also has a microphone (a headset, which Core Audio shows as one device) it opens
//! the microphone instead. A global tap isn't bound to any device, and the aggregate device
//! holds nothing but the tap (no sub-devices), so it keeps recording across output changes.
//! The tap and aggregate setup otherwise mirrors cpal 0.18's `macos/loopback.rs`.
//!
//! Needs the "System Audio Recording" permission (`NSAudioCaptureUsageDescription`); until it
//! is granted the tap delivers silence, not an error.

use crate::{Error, OWN_TAP_PREFIX};
use cpal::traits::HostTrait;
use objc2::{AnyThread, runtime::AnyClass};
use objc2_core_audio::{
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap, AudioObjectID,
    CATapDescription, CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioObjectUnknown,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSString};
use std::{
    ffi::CStr,
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

/// What [`current_device_id`](crate::current_device_id) and `Capture::device_id` report for
/// system output on macOS: the tap follows whatever plays, so it never goes stale.
pub(crate) const DEVICE_ID: &str = "agt:global-tap";

/// Makes every tap and aggregate device of this process unique (several captures, or a new
/// one started while the old one is being dropped).
static INSTANCE: AtomicU32 = AtomicU32::new(0);

/// A global tap and the private aggregate device that exposes it. Dropping destroys both
/// (the aggregate device first): drop the stream on it before.
pub(crate) struct GlobalTap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    /// The aggregate device's UID: its cpal device id.
    uid: String,
}

/// Process taps (`CATapDescription` and the `AudioHardware*ProcessTap` functions) shipped in
/// macOS 14.2. Without the class, objc2 would *panic* on `CATapDescription::alloc()`, so
/// [`GlobalTap::new`] asks the runtime first; `class_found` is that lookup.
fn require_taps(class_found: bool) -> Result<(), Error> {
    if class_found {
        Ok(())
    } else {
        Err("system audio needs macOS 14.2 or later".into())
    }
}

/// The tap's and the aggregate device's name; `input_device_names` and choosing a microphone
/// by name skip it (see [`crate::is_own_tap_device`]).
fn tap_name(pid: u32, n: u32) -> String {
    format!("{OWN_TAP_PREFIX}{pid}.{n}")
}

impl GlobalTap {
    /// Fails, rather than panicking, before macOS 14.2.
    pub(crate) fn new() -> Result<GlobalTap, Error> {
        require_taps(AnyClass::get(c"CATapDescription").is_some())?;
        let (pid, n) = (std::process::id(), INSTANCE.fetch_add(1, Ordering::Relaxed));
        let name = tap_name(pid, n);
        // SAFETY: a freshly allocated description, initialized once with a valid (empty)
        // array; the setters take plain values.
        let description = unsafe {
            // Nothing excluded: AGT plays no audio of its own.
            let d = CATapDescription::initMonoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &NSArray::new(),
            );
            d.setMuteBehavior(CATapMuteBehavior::Unmuted); // still plays on the speakers
            d.setName(&NSString::from_str(&name));
            d.setPrivate(true);
            d
        };
        let mut tap = kAudioObjectUnknown;
        // SAFETY: a valid description and a valid out pointer.
        let status = unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap) };
        if status != 0 {
            return Err(format!("cannot create the system audio tap (OSStatus {status})").into());
        }
        let mut this = GlobalTap {
            tap,
            aggregate: kAudioObjectUnknown,
            uid: format!("agt.system-audio.{pid}.{n}"),
        };
        // SAFETY: the description is alive; `UUID` is always set (a new one per instance).
        let tap_uid = unsafe { description.UUID().UUIDString() }.to_string();
        let spec = aggregate_description(&this.uid, &name, &tap_uid);
        // SAFETY: a valid dictionary and a valid out pointer. On failure `this` drops and
        // destroys the tap.
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(spec.as_opaque(), NonNull::from(&mut this.aggregate))
        };
        if status != 0 {
            return Err(
                format!("cannot create the system audio device (OSStatus {status})").into(),
            );
        }
        Ok(this)
    }

    /// The aggregate device as a cpal input device. The device list catches up with a new
    /// aggregate device a moment after it's created: a lookup right away missed it in 2 of 7
    /// runs (macOS 15.5, `global_tap_aggregate_is_visible_to_cpal`), the next one found it.
    /// So this retries, for up to a second.
    pub(crate) fn device(&self, host: &cpal::Host) -> Result<cpal::Device, Error> {
        let id = cpal::DeviceId::new(cpal::HostId::CoreAudio, &self.uid);
        let t = Instant::now();
        loop {
            if let Some(device) = host.device_by_id(&id) {
                return Ok(device);
            }
            if t.elapsed() > Duration::from_secs(1) {
                return Err("the system audio device did not appear".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for GlobalTap {
    fn drop(&mut self) {
        // SAFETY: both were created by this value and are destroyed once, the device using the
        // tap first.
        unsafe {
            if self.aggregate != kAudioObjectUnknown {
                let status = AudioHardwareDestroyAggregateDevice(self.aggregate);
                if status != 0 {
                    tracing::warn!("cannot destroy the system audio device (OSStatus {status})");
                }
            }
            let status = AudioHardwareDestroyProcessTap(self.tap);
            if status != 0 {
                tracing::warn!("cannot destroy the system audio tap (OSStatus {status})");
            }
        }
    }
}

fn key(k: &CStr) -> CFRetained<CFString> {
    CFString::from_str(&k.to_string_lossy())
}

/// The private aggregate device `uid` whose only input is the tap `tap_uid`, started with it
/// (tap auto-start) and drift-compensated. No sub-devices and no main sub-device: those would
/// tie it to one output device again.
fn aggregate_description(
    uid: &str,
    name: &str,
    tap_uid: &str,
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let yes = CFBoolean::new(true);
    let tap = CFDictionary::<CFString, CFType>::from_slices(
        &[
            &key(kAudioSubTapUIDKey),
            &key(kAudioSubTapDriftCompensationKey),
        ],
        &[&CFString::from_str(tap_uid), yes],
    );
    let taps = CFArray::from_objects(&[&*tap]);
    CFDictionary::<CFString, CFType>::from_slices(
        &[
            &key(kAudioAggregateDeviceNameKey),
            &key(kAudioAggregateDeviceUIDKey),
            &key(kAudioAggregateDeviceTapListKey),
            &key(kAudioAggregateDeviceTapAutoStartKey),
            &key(kAudioAggregateDeviceIsPrivateKey),
        ],
        &[
            &CFString::from_str(name),
            &CFString::from_str(uid),
            &taps,
            yes,
            yes,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_audio::{
        kAudioAggregateDeviceClockDeviceKey, kAudioAggregateDeviceMainSubDeviceKey,
        kAudioAggregateDeviceSubDeviceListKey,
    };

    fn string(d: &CFDictionary<CFString, CFType>, k: &CStr) -> Option<String> {
        let v = d.get(&key(k))?;
        Some(v.downcast_ref::<CFString>()?.to_string())
    }

    fn flag(d: &CFDictionary<CFString, CFType>, k: &CStr) -> Option<bool> {
        let v = d.get(&key(k))?;
        Some(v.downcast_ref::<CFBoolean>()?.as_bool())
    }

    /// Creates a real tap and aggregate device but starts no stream, so no permission prompt
    /// (that comes with the first IO): `cargo test -p agt-audio
    /// global_tap_aggregate_is_visible_to_cpal -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn global_tap_aggregate_is_visible_to_cpal() {
        use cpal::traits::DeviceTrait;
        let host = cpal::default_host();
        let tap = GlobalTap::new().expect("create the tap and aggregate device");
        let id = cpal::DeviceId::new(cpal::HostId::CoreAudio, &tap.uid);
        println!("listed at once: {}", host.device_by_id(&id).is_some());
        {
            let t = Instant::now();
            let device = tap.device(&host).expect("aggregate device not found");
            println!("found after {:?}", t.elapsed());
            let config = device.default_input_config().expect("no input config");
            println!(
                "{} ({:?}): {} Hz, {} ch, {}",
                tap.uid,
                device.description().map(|d| d.name().to_owned()),
                config.sample_rate(),
                config.channels(),
                config.sample_format()
            );
            assert!(config.channels() >= 1);
            let names = crate::input_device_names();
            println!("microphones offered meanwhile: {names:?}");
            assert!(names.iter().all(|n| !crate::is_own_tap_device(n)));
        }
        drop(tap);
        let t = Instant::now();
        while host.device_by_id(&id).is_some() && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("gone after drop: {}", host.device_by_id(&id).is_none());
        assert!(
            host.device_by_id(&id).is_none(),
            "aggregate device left behind"
        );
    }

    #[test]
    fn taps_missing_is_an_error_not_a_panic() {
        // What `Capture::start(&Source::SystemOutput, ..)` returns before macOS 14.2: the
        // error from `GlobalTap::new`, passed through unchanged.
        let e = require_taps(false).unwrap_err();
        assert_eq!(e.to_string(), "system audio needs macOS 14.2 or later");
        // This Mac has them (the lookup `new` makes).
        assert!(require_taps(AnyClass::get(c"CATapDescription").is_some()).is_ok());
    }

    #[test]
    fn the_tap_is_hidden_from_the_microphone_list() {
        assert!(crate::is_own_tap_device(&tap_name(4321, 7)));
    }

    #[test]
    fn the_aggregate_device_holds_only_the_tap() {
        let d = aggregate_description("agt.test.1", "AGT test", "TAP-UUID");
        assert_eq!(
            string(&d, kAudioAggregateDeviceUIDKey).as_deref(),
            Some("agt.test.1")
        );
        assert_eq!(
            string(&d, kAudioAggregateDeviceNameKey).as_deref(),
            Some("AGT test")
        );
        assert_eq!(flag(&d, kAudioAggregateDeviceIsPrivateKey), Some(true));
        assert_eq!(flag(&d, kAudioAggregateDeviceTapAutoStartKey), Some(true));
        for k in [
            kAudioAggregateDeviceSubDeviceListKey,
            kAudioAggregateDeviceMainSubDeviceKey,
            kAudioAggregateDeviceClockDeviceKey,
        ] {
            assert!(d.get(&key(k)).is_none(), "{k:?}");
        }
        let taps = d
            .get(&key(kAudioAggregateDeviceTapListKey))
            .expect("tap list");
        let taps = taps
            .downcast_ref::<CFArray>()
            .expect("tap list is an array");
        assert_eq!(taps.len(), 1);
        // SAFETY: built above as an array of `CFDictionary<CFString, CFType>`.
        let taps = unsafe { taps.cast_unchecked::<CFDictionary<CFString, CFType>>() };
        let tap = taps.get(0).expect("one tap");
        assert_eq!(
            string(&tap, kAudioSubTapUIDKey).as_deref(),
            Some("TAP-UUID")
        );
        assert_eq!(flag(&tap, kAudioSubTapDriftCompensationKey), Some(true));
    }
}
