//! Overlay check, outside the app.
//!
//! ```sh
//! cargo run --release -p agt-overlay --example demo -- --png <dir>   # render each state to PNGs (off-screen)
//! cargo run --release -p agt-overlay --example demo -- --show        # show it on screen for ~4 s and measure
//! cargo run --release -p agt-overlay --example demo -- --notice      # the "not in a text box" notice; click Copy
//! cargo run --release -p agt-overlay --example demo -- --getting-ready [png]   # "Getting ready...", optionally captured
//! ```
//!
//! `--show` drives the overlay from a winit event loop the way the app does (status events
//! from another thread through an `EventLoopProxy`, frames via `ControlFlow::WaitUntil`) with a
//! synthetic microphone level, and prints: event → visible latency, frame rate and CPU time
//! while visible, loop wake-ups and CPU time while hidden, and whether the foreground window
//! changed. It never takes focus or sends input.

use agt_overlay::{
    FRAME, GETTING_READY_FOR, Mode, NOTICE_FOR, Overlay, Position,
    canvas::Canvas,
    scene::{self, BARS, Scene},
};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::WindowId,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--png") if args.len() == 2 => write_pngs(Path::new(&args[1])),
        Some("--show") => show(args.get(1).map(Into::into)),
        Some("--notice") => notice(),
        Some("--getting-ready") => getting_ready(args.get(1).map(Into::into)),
        _ => eprintln!("usage: demo --png <dir> | --show | --notice | --getting-ready [png]"),
    }
}

/// A plausible speech envelope for the waveform, `0..=1`.
fn fake_speech(t: f32) -> f32 {
    let syllables = (t * 9.0).sin().abs() * (t * 2.3).sin().abs();
    0.15 + 0.85 * syllables * (0.6 + 0.4 * (t * 0.7).cos())
}

/// Microphone RMS that produces roughly `fake_speech` on the overlay's dB scale.
fn fake_rms(t: f32) -> f32 {
    10f32.powf((fake_speech(t) * 42.0 - 54.0) / 20.0)
}

fn write_pngs(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create output dir");
    let mut levels = [0.0; BARS];
    for (i, l) in levels.iter_mut().enumerate() {
        *l = fake_speech(i as f32 * 0.042);
    }
    for (name, mode, t) in [
        ("recording", Mode::Recording, 0.0),
        ("transcribing", Mode::Transcribing, 0.45),
        ("error", Mode::Error, 0.0),
    ] {
        let scale = 2.0;
        let (w, h) = scene::pixel_size(scale);
        let mut canvas = Canvas::new(w, h);
        scene::render(
            &Scene {
                mode,
                t,
                levels: &levels,
                scale,
            },
            &mut canvas,
        );
        save_png(
            &dir.join(format!("overlay-{name}.png")),
            w,
            h,
            &canvas.to_rgba8(),
        );
    }
}

/// Events from the "pipeline" thread, timestamped when sent.
enum Ev {
    Mode(Mode, Instant),
    /// The notice's Copy button was clicked.
    Copy,
    Done,
}

#[derive(Default)]
struct Phase {
    name: &'static str,
    start: Option<Instant>,
    cpu: Duration,
    frames: u32,
    wakeups: u32,
}

struct Demo {
    overlay: Option<Overlay>,
    t0: Instant,
    phase: Phase,
    foreground: usize,
    /// Where to save a screen capture of the pill (taken just before it switches to transcribing).
    capture: Option<PathBuf>,
    /// `--notice`: show the notice at start; its Copy button posts [`Ev::Copy`] here.
    notice: Option<EventLoopProxy<Ev>>,
    /// `--getting-ready`: say "Getting ready..." at start.
    getting_ready: bool,
}

impl Demo {
    fn end_phase(&mut self, next: &'static str) {
        let now = Instant::now();
        if let Some(start) = self.phase.start {
            let wall = now - start;
            let cpu = cpu_time().saturating_sub(self.phase.cpu);
            println!(
                "  {:<13} {:>5.2} s | {:>3} frames ({:>5.1} fps) | {:>3} loop wake-ups | CPU {:>6.1} ms ({:.2}% of one core)",
                self.phase.name,
                wall.as_secs_f64(),
                self.phase.frames,
                self.phase.frames as f64 / wall.as_secs_f64(),
                self.phase.wakeups,
                cpu.as_secs_f64() * 1e3,
                cpu.as_secs_f64() / wall.as_secs_f64() * 100.0,
            );
        }
        self.phase = Phase {
            name: next,
            start: Some(now),
            cpu: cpu_time(),
            ..Phase::default()
        };
    }
}

impl ApplicationHandler<Ev> for Demo {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init {
            let proxy = self.notice.clone();
            let on_copy = move || {
                if let Some(p) = &proxy {
                    let _ = p.send_event(Ev::Copy);
                }
            };
            let mut overlay = Overlay::new(Position::Bottom, on_copy).expect("create overlay");
            if self.notice.is_some() {
                overlay.show_notice(Instant::now());
            }
            if self.getting_ready {
                overlay.show_getting_ready(0.0, Instant::now());
                println!("showing: {:?}", overlay.mode());
            }
            self.overlay = Some(overlay);
            self.foreground = foreground().0;
            self.end_phase("start");
        }
        self.phase.wakeups += 1;
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, ev: Ev) {
        match ev {
            Ev::Mode(mode, sent) => {
                let name = match mode {
                    Mode::Hidden => "hidden",
                    Mode::Recording => "recording",
                    Mode::Transcribing => "transcribing",
                    Mode::Error => "error",
                };
                if mode == Mode::Transcribing
                    && let Some(path) = self.capture.take()
                {
                    capture_overlay(&path);
                }
                self.end_phase(name);
                let level = fake_rms(self.t0.elapsed().as_secs_f32());
                if let Some(o) = self.overlay.as_mut() {
                    o.set_mode(mode, level, Instant::now());
                }
                let shown = Instant::now();
                let (fg, ours) = foreground();
                println!(
                    "{name}: event sent → on screen {:.2} ms (of which set_mode {:.2} ms); foreground window {}{}",
                    (shown - sent).as_secs_f64() * 1e3,
                    (shown - self.phase.start.unwrap()).as_secs_f64() * 1e3,
                    if fg == self.foreground {
                        "unchanged"
                    } else {
                        "changed (by the user)"
                    },
                    if ours {
                        " - IS OURS: focus was stolen!"
                    } else {
                        ""
                    },
                );
                if mode != Mode::Hidden {
                    self.phase.frames += 1;
                }
            }
            Ev::Copy => {
                println!("Copy clicked; foreground window {}", foreground().0);
                if let Some(o) = self.overlay.as_mut() {
                    o.notice_copied(Instant::now());
                }
            }
            Ev::Done => {
                self.end_phase("");
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(o) = self.overlay.as_mut() else {
            return;
        };
        if let Some(due) = o.next_frame() {
            let now = Instant::now();
            if now >= due {
                o.tick(fake_rms(self.t0.elapsed().as_secs_f32()), now);
                self.phase.frames += 1;
            }
        }
        event_loop.set_control_flow(match o.next_frame() {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn show(capture: Option<PathBuf>) {
    let event_loop = EventLoop::<Ev>::with_user_event()
        .build()
        .expect("event loop");
    benchmark_render();
    let proxy = event_loop.create_proxy();
    thread::spawn(move || {
        let send = |m| {
            let _ = proxy.send_event(Ev::Mode(m, Instant::now()));
        };
        thread::sleep(Duration::from_millis(1000)); // idle, hidden
        send(Mode::Recording);
        thread::sleep(Duration::from_millis(2000));
        send(Mode::Transcribing);
        thread::sleep(Duration::from_millis(1000));
        send(Mode::Hidden);
        thread::sleep(Duration::from_millis(3000)); // idle, hidden again
        let _ = proxy.send_event(Ev::Done);
    });
    let mut demo = Demo {
        overlay: None,
        t0: Instant::now(),
        phase: Phase::default(),
        foreground: 0,
        capture,
        notice: None,
        getting_ready: false,
    };
    event_loop.run_app(&mut demo).expect("run");
}

/// Says "Getting ready..." (as for a key press while the model loads) until it hides itself,
/// saving a screen capture of it to `capture` if given.
fn getting_ready(capture: Option<PathBuf>) {
    let event_loop = EventLoop::<Ev>::with_user_event()
        .build()
        .expect("event loop");
    let proxy = event_loop.create_proxy();
    thread::spawn(move || {
        thread::sleep(GETTING_READY_FOR / 2);
        if let Some(path) = capture {
            capture_overlay(&path);
        }
        thread::sleep(GETTING_READY_FOR);
        let _ = proxy.send_event(Ev::Done);
    });
    let mut demo = Demo {
        overlay: None,
        t0: Instant::now(),
        phase: Phase::default(),
        foreground: 0,
        capture: None,
        notice: None,
        getting_ready: true,
    };
    event_loop.run_app(&mut demo).expect("run");
}

/// Shows the "not in a text box" notice until it hides itself, printing Copy clicks.
fn notice() {
    let event_loop = EventLoop::<Ev>::with_user_event()
        .build()
        .expect("event loop");
    let proxy = event_loop.create_proxy();
    let done = proxy.clone();
    thread::spawn(move || {
        thread::sleep(NOTICE_FOR + Duration::from_secs(1));
        let _ = done.send_event(Ev::Done);
    });
    println!("frontmost app (pid) at start: {}", foreground().0);
    let mut demo = Demo {
        overlay: None,
        t0: Instant::now(),
        phase: Phase::default(),
        foreground: 0,
        capture: None,
        notice: Some(proxy),
        getting_ready: false,
    };
    event_loop.run_app(&mut demo).expect("run");
}

fn benchmark_render() {
    println!("frame interval {:.2} ms", FRAME.as_secs_f64() * 1e3);
    cpu_time(); // calibrate before measuring
    for scale in [1.0, 1.5, 2.0] {
        let (w, h) = scene::pixel_size(scale);
        let (mut bg, mut canvas) = (Canvas::new(w, h), Canvas::new(w, h));
        scene::render_background(Mode::Recording, scale, &mut bg);
        let levels = [0.6; BARS];
        let n = 2000;
        let t = Instant::now();
        for i in 0..n {
            canvas.copy_from(&bg);
            let s = Scene {
                mode: Mode::Recording,
                t: i as f32 * 0.016,
                levels: &levels,
                scale,
            };
            scene::render_content(&s, &mut canvas);
        }
        println!(
            "render at {scale}x ({w}x{h} px): {:.1} µs/frame",
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        );
    }
}

/// The foreground window, and whether it belongs to this process (it never should).
#[cfg(windows)]
fn foreground() -> (usize, bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    let hwnd = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (hwnd as usize, pid == std::process::id())
}

/// macOS: the frontmost app's pid, and whether it's this process.
#[cfg(target_os = "macos")]
fn foreground() -> (usize, bool) {
    use objc2::{
        msg_send,
        runtime::{AnyClass, AnyObject},
    };
    let Some(cls) = AnyClass::get(c"NSWorkspace") else {
        return (0, false);
    };
    let pid: i32 = unsafe {
        let ws: *mut AnyObject = msg_send![cls, sharedWorkspace];
        let app: *mut AnyObject = msg_send![ws, frontmostApplication];
        if app.is_null() {
            return (0, false);
        }
        msg_send![app, processIdentifier]
    };
    (pid as usize, pid as u32 == std::process::id())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn foreground() -> (usize, bool) {
    (0, false)
}

fn save_png(path: &Path, w: usize, h: usize, rgba: &[u8]) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).expect("create png"));
    let mut enc = png::Encoder::new(file, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png data");
    println!("wrote {}", path.display());
}

/// Copies what's on screen where the visible overlay window is (the pill or the notice,
/// composited over whatever is below it) into a PNG.
#[cfg(windows)]
fn capture_overlay(path: &Path) {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::{FindWindowExW, GetWindowRect, IsWindowVisible},
    };
    let class: Vec<u16> = "AGT.Overlay\0".encode_utf16().collect();
    unsafe {
        let mut hwnd = std::ptr::null_mut();
        loop {
            hwnd = FindWindowExW(std::ptr::null_mut(), hwnd, class.as_ptr(), std::ptr::null());
            if hwnd.is_null() || IsWindowVisible(hwnd) != 0 {
                break;
            }
        }
        let mut r: RECT = std::mem::zeroed();
        if hwnd.is_null() || GetWindowRect(hwnd, &mut r) == 0 {
            eprintln!("overlay window not found");
            return;
        }
        let (w, h) = ((r.right - r.left) as usize, (r.bottom - r.top) as usize);
        println!("overlay window at ({}, {}), {w}x{h} px", r.left, r.top);
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w as i32, h as i32);
        let old = SelectObject(dc, bmp);
        BitBlt(dc, 0, 0, w as i32, h as i32, screen, r.left, r.top, SRCCOPY);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w as i32;
        info.bmiHeader.biHeight = -(h as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut bgra = vec![0u8; w * h * 4];
        GetDIBits(
            dc,
            bmp,
            0,
            h as u32,
            bgra.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = 255;
        }
        save_png(path, w, h, &bgra);
    }
}

#[cfg(not(windows))]
fn capture_overlay(_: &Path) {
    // macOS: window capture needs the Screen Recording permission, even for our own window.
    eprintln!("capture is only implemented on Windows");
}

/// CPU time used by this process (all threads, user + kernel). `GetProcessTimes` only ticks
/// every 15.6 ms, so this counts CPU cycles instead and converts them with a rate measured
/// once by spinning.
#[cfg(windows)]
fn cpu_time() -> Duration {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::{
        Threading::GetCurrentProcess, WindowsProgramming::QueryProcessCycleTime,
    };
    fn cycles() -> u64 {
        let mut c = 0;
        unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut c) };
        c
    }
    static RATE: OnceLock<f64> = OnceLock::new();
    let rate = *RATE.get_or_init(|| {
        let (c0, t0) = (cycles(), Instant::now());
        while t0.elapsed() < Duration::from_millis(200) {
            std::hint::spin_loop();
        }
        (cycles() - c0) as f64 / t0.elapsed().as_secs_f64()
    });
    Duration::from_secs_f64(cycles() as f64 / rate)
}

#[cfg(not(windows))]
fn cpu_time() -> Duration {
    // getrusage: user + system time of the whole process.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}
