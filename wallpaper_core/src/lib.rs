//! Minimal, C-ABI-stable Windows wallpaper core.
//! Playback is provided by dynamically loaded libVLC (LGPL-2.1-or-later).

mod direct_mft;
mod media_event_queue;
#[allow(dead_code)]
mod mf_d3d11;
mod native_audio;
mod playback_clock;
mod vlc;

use std::{
    ffi::CStr,
    os::raw::c_char,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, SetSysColors, COLOR_DESKTOP, MONITORINFO,
            MONITOR_DEFAULTTONEAREST,
        },
        System::{
            Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::*,
    },
};

const WM_SPAWN_WORKER: u32 = 0x052C;
const WINDOW_CLASS: PCWSTR = w!("WallpaperCoreHost");

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum FallbackStatus {
    #[default]
    Idle,
    InProgress,
    Active,
    Failed,
}

impl FallbackStatus {
    fn try_begin(&mut self) -> bool {
        if *self != Self::Idle {
            return false;
        }
        *self = Self::InProgress;
        true
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PlaybackStatus {
    #[default]
    Stopped,
    Starting,
    RunningNative,
    RunningHybrid,
    RunningVlc,
    Stopping,
    Failed,
}

struct PlaybackConfig {
    generation: u64,
    host_window: isize,
    d3d11_media_foundation_available: bool,
    muted: bool,
    volume: i32,
    performance_mode: i32,
    stretch_to_fill: bool,
    paused: bool,
}

#[derive(Default)]
struct PlaybackResources {
    player: Option<vlc::VlcPlayer>,
    native_renderer: Option<direct_mft::DirectMp4Renderer>,
    native_audio: Option<native_audio::NativeAudioRenderer>,
}

impl PlaybackResources {
    fn stop(self) {
        if let Some(player) = self.player {
            player.stop();
        }
        if let Some(renderer) = self.native_renderer {
            renderer.stop();
        }
        if let Some(audio) = self.native_audio {
            audio.stop();
        }
    }
}

struct PreparedPlayback {
    resources: PlaybackResources,
    native_mp4_diagnostic: String,
    native_audio_diagnostic: String,
    native_mp4_failure_code: u32,
    video_fallback_status: FallbackStatus,
    audio_fallback_status: FallbackStatus,
    status: PlaybackStatus,
}

struct EngineState {
    generation: u64,
    playback_status: PlaybackStatus,
    host_window: isize,
    active_media: Option<String>,
    muted: bool,
    mute_when_other_app_open: bool,
    automatically_muted: bool,
    volume: i32,
    performance_mode: i32,
    stretch_to_fill: bool,
    d3d11_media_foundation_available: bool,
    native_mp4_diagnostic: String,
    native_mp4_failure_code: u32,
    native_audio_diagnostic: String,
    video_fallback_status: FallbackStatus,
    audio_fallback_status: FallbackStatus,
    last_error: String,
    player: Option<vlc::VlcPlayer>,
    native_renderer: Option<direct_mft::DirectMp4Renderer>,
    native_audio: Option<native_audio::NativeAudioRenderer>,
    monitor_stop: AtomicBool,
    fullscreen_paused: AtomicBool,
    monitor: Option<JoinHandle<()>>,
}

static ENGINE: OnceLock<Mutex<EngineState>> = OnceLock::new();
static PLAYBACK_START_GATE: OnceLock<Mutex<()>> = OnceLock::new();

fn engine() -> &'static Mutex<EngineState> {
    ENGINE.get_or_init(|| {
        Mutex::new(EngineState {
            generation: 0,
            playback_status: PlaybackStatus::Stopped,
            host_window: 0,
            active_media: None,
            muted: false,
            mute_when_other_app_open: false,
            automatically_muted: false,
            volume: 100,
            performance_mode: 1,
            stretch_to_fill: true,
            d3d11_media_foundation_available: false,
            native_mp4_diagnostic: String::new(),
            native_mp4_failure_code: 0,
            native_audio_diagnostic: String::new(),
            video_fallback_status: FallbackStatus::Idle,
            audio_fallback_status: FallbackStatus::Idle,
            last_error: String::new(),
            player: None,
            native_renderer: None,
            native_audio: None,
            monitor_stop: AtomicBool::new(false),
            fullscreen_paused: AtomicBool::new(false),
            monitor: None,
        })
    })
}

fn playback_start_gate() -> &'static Mutex<()> {
    PLAYBACK_START_GATE.get_or_init(|| Mutex::new(()))
}

/// Locates the WorkerW below desktop icons and attaches our child render target.
#[no_mangle]
pub extern "C" fn init_engine() -> bool {
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    if state.host_window != 0 {
        state.last_error.clear();
        return true;
    }

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        state.d3d11_media_foundation_available = mf_d3d11::hardware_pipeline_available();
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).unwrap_or_default();
        if progman.0.is_null() {
            state.last_error = "Progman desktop window was not found.".into();
            return false;
        }

        // Explorer creates a WorkerW sibling after receiving this documented-in-practice message.
        let mut ignored = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            WM_SPAWN_WORKER,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            1_000,
            Some(&mut ignored),
        );

        let mut worker = HWND::default();
        let _ = EnumWindows(
            Some(find_workerw),
            LPARAM((&mut worker as *mut HWND) as isize),
        );
        // The owner of SHELLDLL_DefView is the actual desktop composition
        // parent: Progman on this Explorer topology, WorkerW on others.
        if worker.0.is_null() {
            state.last_error = "Desktop icon layer was not found.".into();
            return false;
        }

        let instance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_window_proc),
            hInstance: instance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc); // already registered is harmless
        let host = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("WallpaperCore render host"),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            worker,
            None,
            instance,
            None,
        );
        let host = match host {
            Ok(value) => value,
            Err(_) => {
                state.last_error = "Could not create the wallpaper host window.".into();
                return false;
            }
        };
        if host.0.is_null() {
            state.last_error = "Could not create the wallpaper host window.".into();
            return false;
        }
        state.host_window = host.0 as isize;
        resize_to_parent(host, worker);
        // A new child window is initially topmost among its siblings, which
        // covers desktop icons.  HWND_BOTTOM is too far down on modern
        // Explorer: it ends up behind Explorer's own background surface.
        // Put the renderer immediately *after* SHELLDLL_DefView instead:
        // the icon view remains above it while the video remains drawable.
        let icon_view = FindWindowExW(
            worker,
            HWND::default(),
            w!("SHELLDLL_DefView"),
            PCWSTR::null(),
        )
        .unwrap_or_default();
        let _ = SetWindowPos(
            host,
            if icon_view.0.is_null() {
                HWND_BOTTOM
            } else {
                icon_view
            },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    state.monitor_stop.store(false, Ordering::Release);
    state.last_error.clear();
    let core = engine();
    state.monitor = Some(thread::spawn(move || fullscreen_monitor(core)));
    true
}

/// Accepts UTF-8 absolute or relative path. The renderer implementation owns no
/// pointer from the caller: the path is copied into Rust-owned `String`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // C ABI keeps this callable from safe P/Invoke.
pub extern "C" fn set_wallpaper(file_path: *const c_char) -> bool {
    // Explorer can recreate WorkerW after the app has started. Retry initialization
    // at the public API boundary instead of leaving the caller with stale state.
    if !init_engine() {
        return false;
    }
    if file_path.is_null() {
        set_last_error("Wallpaper path is null.");
        return false;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("Wallpaper path is not valid UTF-8.");
        return false;
    };
    let valid_extension = ["mp4", "gif", "webp"].iter().any(|x| {
        Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(x))
    });
    if !valid_extension || !Path::new(&path).is_file() {
        set_last_error("File does not exist or has an unsupported extension.");
        return false;
    }

    start_wallpaper(path)
}

fn start_wallpaper(path: String) -> bool {
    let (config, previous) = {
        let mut state = match engine().lock() {
            Ok(value) => value,
            Err(_) => return false,
        };
        if state.playback_status == PlaybackStatus::Stopping {
            state.last_error = "Engine shutdown is in progress.".into();
            return false;
        }
        state.generation = state.generation.wrapping_add(1);
        state.playback_status = PlaybackStatus::Starting;
        state.active_media = Some(path.clone());
        state.video_fallback_status = FallbackStatus::Idle;
        state.audio_fallback_status = FallbackStatus::Idle;
        state.native_mp4_failure_code = 0;
        state.last_error.clear();
        let config = PlaybackConfig {
            generation: state.generation,
            host_window: state.host_window,
            d3d11_media_foundation_available: state.d3d11_media_foundation_available,
            muted: state.muted || state.automatically_muted,
            volume: state.volume,
            performance_mode: state.performance_mode,
            stretch_to_fill: state.stretch_to_fill,
            paused: state.fullscreen_paused.load(Ordering::Acquire),
        };
        let previous = PlaybackResources {
            player: state.player.take(),
            native_renderer: state.native_renderer.take(),
            native_audio: state.native_audio.take(),
        };
        (config, previous)
    };

    // Driver, COM and libVLC teardown must never hold the engine state mutex.
    previous.stop();

    // Only one pipeline may create a swap chain for the shared wallpaper HWND.
    // Newer requests can supersede a waiter before it performs expensive work.
    let _start_guard = match playback_start_gate().lock() {
        Ok(guard) => guard,
        Err(_) => {
            set_last_error("Playback startup gate failed.");
            return false;
        }
    };
    if !generation_is_current(config.generation) {
        return false;
    }

    let prepared = match prepare_playback(&path, &config) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Ok(mut state) = engine().lock() {
                if state.generation == config.generation {
                    state.playback_status = PlaybackStatus::Failed;
                    state.active_media = None;
                    state.last_error = error;
                }
            }
            return false;
        }
    };

    let mut state = match engine().lock() {
        Ok(state) => state,
        Err(_) => {
            prepared.resources.stop();
            return false;
        }
    };
    if !is_current_playback_request(state.generation, state.playback_status, config.generation) {
        drop(state);
        prepared.resources.stop();
        return false;
    }
    let current_mute = state.muted || state.automatically_muted;
    let current_volume = state.volume;
    let current_pause = state.fullscreen_paused.load(Ordering::Acquire);
    if let Some(player) = prepared.resources.player.as_ref() {
        unsafe {
            player.set_muted(current_mute);
            player.set_volume(current_volume);
            player.set_paused(current_pause);
        }
    }
    if let Some(renderer) = prepared.resources.native_renderer.as_ref() {
        renderer.set_paused(current_pause);
    }
    if let Some(audio) = prepared.resources.native_audio.as_ref() {
        audio.set_muted(current_mute);
        audio.set_volume(current_volume);
        audio.set_paused(current_pause);
    }
    state.player = prepared.resources.player;
    state.native_renderer = prepared.resources.native_renderer;
    state.native_audio = prepared.resources.native_audio;
    state.native_mp4_diagnostic = prepared.native_mp4_diagnostic;
    state.native_audio_diagnostic = prepared.native_audio_diagnostic;
    state.native_mp4_failure_code = prepared.native_mp4_failure_code;
    state.video_fallback_status = prepared.video_fallback_status;
    state.audio_fallback_status = prepared.audio_fallback_status;
    state.playback_status = prepared.status;
    state.last_error.clear();
    true
}

fn generation_is_current(generation: u64) -> bool {
    engine()
        .lock()
        .map(|state| {
            is_current_playback_request(state.generation, state.playback_status, generation)
        })
        .unwrap_or(false)
}

fn is_current_playback_request(
    current_generation: u64,
    status: PlaybackStatus,
    request_generation: u64,
) -> bool {
    current_generation == request_generation && status == PlaybackStatus::Starting
}

fn prepare_playback(path: &str, config: &PlaybackConfig) -> Result<PreparedPlayback, String> {
    // Deterministic concurrency tests can keep one generation in the startup
    // phase while a newer request supersedes it. The client never sets this.
    if let Some(delay_ms) = std::env::var("OPENPAPER_TEST_STARTUP_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    {
        thread::sleep(Duration::from_millis(delay_ms.min(5_000)));
    }
    let is_mp4 = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("mp4"));
    let playback_clock = Arc::new(playback_clock::PlaybackClock::new());
    let mut native_mp4_failure_code = 0;
    let (native_renderer, native_mp4_diagnostic) = if is_mp4
        && config.d3d11_media_foundation_available
    {
        match direct_mft::DirectMp4Renderer::start(
            path.to_owned(),
            HWND(config.host_window as _),
            Arc::clone(&playback_clock),
            config.stretch_to_fill,
        ) {
            Ok(renderer) => (
                Some(renderer),
                "Direct Media Foundation/DXVA renderer is active.".into(),
            ),
            Err(error) => {
                native_mp4_failure_code = direct_mft::renderer_failure_code(&error);
                (
                        None,
                        format!(
                            "Direct renderer preflight failed; libVLC D3D11VA fallback is active: {error}"
                        ),
                    )
            }
        }
    } else {
        (
            None,
            if is_mp4 {
                "D3D11 Media Foundation is unavailable; libVLC D3D11VA fallback is active.".into()
            } else {
                "GIF/WEBP playback uses libVLC.".into()
            },
        )
    };

    let (native_audio, mut native_audio_diagnostic) = if native_renderer.is_some() {
        match native_audio::NativeAudioRenderer::start(
            path.to_owned(),
            config.muted,
            config.volume,
            Arc::clone(&playback_clock),
        ) {
            Ok(audio) => (
                Some(audio),
                "Native Media Foundation/WASAPI audio is active.".into(),
            ),
            Err(error) => (
                None,
                format!("Native audio preflight failed; libVLC audio fallback is active: {error}"),
            ),
        }
    } else {
        (None, "libVLC owns audio for this wallpaper.".into())
    };

    let show_vlc_video = native_renderer.is_none();
    let needs_vlc = show_vlc_video || native_audio.is_none();
    let player = if needs_vlc {
        match unsafe {
            vlc::VlcPlayer::start(
                path,
                config.host_window as usize,
                config.performance_mode,
                show_vlc_video,
                config.stretch_to_fill,
            )
        } {
            Ok(player) => Some(player),
            Err(error) if native_renderer.is_some() => {
                native_audio_diagnostic = format!(
                    "Native audio and libVLC audio fallback both failed; video remains active: {error}"
                );
                None
            }
            Err(error) => {
                PlaybackResources {
                    player: None,
                    native_renderer,
                    native_audio,
                }
                .stop();
                return Err(error);
            }
        }
    } else {
        None
    };

    if let Some(player) = player.as_ref() {
        unsafe {
            player.set_muted(config.muted);
            player.set_volume(config.volume);
            player.set_paused(config.paused);
        }
    }
    if let Some(renderer) = native_renderer.as_ref() {
        renderer.set_paused(config.paused);
        renderer.activate();
    }
    if let Some(audio) = native_audio.as_ref() {
        audio.set_paused(config.paused);
        audio.activate();
    }

    let video_fallback_status = if show_vlc_video && player.is_some() {
        FallbackStatus::Active
    } else {
        FallbackStatus::Idle
    };
    let audio_fallback_status = if native_audio.is_none() && player.is_some() {
        FallbackStatus::Active
    } else {
        FallbackStatus::Idle
    };
    let status = if native_renderer.is_none() {
        PlaybackStatus::RunningVlc
    } else if player.is_some() || native_audio.is_none() {
        PlaybackStatus::RunningHybrid
    } else {
        PlaybackStatus::RunningNative
    };
    Ok(PreparedPlayback {
        resources: PlaybackResources {
            player,
            native_renderer,
            native_audio,
        },
        native_mp4_diagnostic,
        native_audio_diagnostic,
        native_mp4_failure_code,
        video_fallback_status,
        audio_fallback_status,
        status,
    })
}

/// 0=eco, 1=balanced, 2=quality. New mode is applied when playback restarts.
#[no_mangle]
pub extern "C" fn set_performance_mode(mode: i32) -> bool {
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    state.performance_mode = mode.clamp(0, 2);
    state.last_error.clear();
    true
}

/// Controls scaling for the next playback start. `true` fills the complete
/// desktop even when that changes the source aspect ratio; `false` preserves
/// the source aspect ratio and leaves black bars where necessary.
#[no_mangle]
pub extern "C" fn set_stretch_to_fill(enabled: bool) -> bool {
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    state.stretch_to_fill = enabled;
    state.last_error.clear();
    true
}

/// Mutes or unmutes the active native or fallback audio session.
#[no_mangle]
pub extern "C" fn set_muted(muted: bool) -> bool {
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    state.muted = muted;
    if let Some(player) = state.player.as_ref() {
        unsafe { player.set_muted(state.muted || state.automatically_muted) };
    }
    if let Some(audio) = state.native_audio.as_ref() {
        audio.set_muted(state.muted || state.automatically_muted);
    }
    state.last_error.clear();
    true
}

/// Enables automatic mute while any ordinary foreground application is active.
#[no_mangle]
pub extern "C" fn set_mute_when_other_app_open(enabled: bool) -> bool {
    // Apply the new policy immediately instead of waiting for the monitor's
    // next polling tick. This also makes the settings toggle deterministic.
    let automatically_muted = enabled && unsafe { foreground_is_external_application() };
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    state.mute_when_other_app_open = enabled;
    state.automatically_muted = automatically_muted;
    if let Some(player) = state.player.as_ref() {
        unsafe { player.set_muted(state.muted || state.automatically_muted) };
    }
    if let Some(audio) = state.native_audio.as_ref() {
        audio.set_muted(state.muted || state.automatically_muted);
    }
    state.last_error.clear();
    true
}

/// Sets wallpaper audio volume in the inclusive 0..=100 range. The value is
/// retained even before a wallpaper is started, so UI settings are reliable.
#[no_mangle]
pub extern "C" fn set_volume(volume: i32) -> bool {
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    state.volume = volume.clamp(0, 100);
    if let Some(player) = state.player.as_ref() {
        unsafe { player.set_volume(state.volume) };
    }
    if let Some(audio) = state.native_audio.as_ref() {
        audio.set_volume(state.volume);
    }
    state.last_error.clear();
    true
}

/// Copies the most recent UTF-8 error into a caller-owned buffer and returns
/// the complete message length (excluding the terminating NUL).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // C ABI keeps this callable from safe P/Invoke.
pub extern "C" fn get_last_error(buffer: *mut c_char, capacity: usize) -> usize {
    let message = engine()
        .lock()
        .map(|state| state.last_error.clone())
        .unwrap_or_else(|_| "Engine state lock failed.".into());
    let bytes = message.as_bytes();
    if !buffer.is_null() && capacity != 0 {
        let count = bytes.len().min(capacity - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), count);
            *buffer.add(count) = 0;
        }
    }
    bytes.len()
}

/// Stops the renderer, monitor and the child window. Safe to call repeatedly.
#[no_mangle]
pub extern "C" fn stop_engine() {
    let (monitor, resources, host_window) = {
        let mut state = match engine().lock() {
            Ok(value) => value,
            Err(_) => return,
        };
        state.generation = state.generation.wrapping_add(1);
        state.playback_status = PlaybackStatus::Stopping;
        state.monitor_stop.store(true, Ordering::Release);
        state.active_media = None;
        state.video_fallback_status = FallbackStatus::Idle;
        state.audio_fallback_status = FallbackStatus::Idle;
        state.native_mp4_failure_code = 0;
        state.last_error.clear();
        let resources = PlaybackResources {
            player: state.player.take(),
            native_renderer: state.native_renderer.take(),
            native_audio: state.native_audio.take(),
        };
        (state.monitor.take(), resources, state.host_window)
    };
    resources.stop();

    // Wait for an already-running startup to observe the new generation and
    // dispose its uncommitted pipeline before the shared HWND is destroyed.
    let _start_guard = playback_start_gate().lock().ok();
    if host_window != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(host_window as _));
        }
    }
    if let Ok(mut state) = engine().lock() {
        if state.playback_status == PlaybackStatus::Stopping {
            state.host_window = 0;
            state.playback_status = PlaybackStatus::Stopped;
        }
    }
    if let Some(thread) = monitor {
        let _ = thread.join();
    }
}

/// Reports whether the native Media Foundation + D3D11 renderer can be used
/// on this computer. Individual files are still preflighted before activation.
#[no_mangle]
pub extern "C" fn is_native_renderer_available() -> bool {
    engine()
        .lock()
        .map(|state| state.d3d11_media_foundation_available)
        .unwrap_or(false)
}

/// Reports whether a direct D3D11-aware H.264/HEVC hardware MFT is available
/// for the next native backend. This does not claim MP4 playback readiness.
#[no_mangle]
pub extern "C" fn is_direct_mft_decoder_available() -> bool {
    direct_mft::has_d3d11_hardware_decoder()
}

/// Returns 1 for H.264, 2 for HEVC, or 0 when the direct MP4 demuxer cannot
/// prepare this file. The detailed reason is available through get_last_error.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn probe_direct_mp4_demuxer(file_path: *const c_char) -> i32 {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return 0;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return 0;
    };
    match direct_mft::probe_mp4_demuxer(&path) {
        Ok(direct_mft::DirectCodec::H264) => 1,
        Ok(direct_mft::DirectCodec::Hevc) => 2,
        Err(error) => {
            set_last_error(&error);
            0
        }
    }
}

/// Executes a bounded Media Source/Media Stream asynchronous demux test.
/// The return value is the number of real MP4 samples observed; diagnostic
/// details remain available through get_last_error when startup fails.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn probe_direct_mp4_event_loop(file_path: *const c_char) -> u32 {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return 0;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return 0;
    };
    match direct_mft::probe_direct_mp4_event_loop(&path, Duration::from_secs(3)) {
        Ok(stats) if stats.samples > 0 => stats.samples,
        Ok(stats) => {
            set_last_error(&format!(
                "MP4 direct event loop returned no samples (source events: {}, stream events: {}, started: {}).",
                stats.source_events, stats.stream_events, stats.stream_started
            ));
            0
        }
        Err(error) => {
            set_last_error(&error);
            0
        }
    }
}

/// Verifies that a hardware H.264/HEVC MFT accepts this MP4's compressed
/// media type. This is diagnostic-only until the native renderer owns both
/// the decoder's D3D11 device manager and its output surfaces.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn can_configure_direct_mft_decoder(file_path: *const c_char) -> bool {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return false;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return false;
    };
    match direct_mft::can_configure_hardware_decoder_for_mp4(&path) {
        Ok(true) => true,
        Ok(false) => {
            set_last_error("No hardware MFT accepted this MP4 video media type.");
            false
        }
        Err(error) => {
            set_last_error(&error);
            false
        }
    }
}

/// Diagnostic for the native renderer: verifies that Windows can bind an MFT
/// decoder to a D3D11 DXGI device manager for this exact MP4. It does not yet
/// route user playback away from the known-good libVLC fallback.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn can_create_direct_gpu_decoder_session(file_path: *const c_char) -> bool {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return false;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return false;
    };
    match direct_mft::create_gpu_decoder_session_for_mp4(&path) {
        Ok(session) => unsafe { session.decoder.GetAttributes().is_ok() },
        Err(error) => {
            set_last_error(&error);
            false
        }
    }
}

/// Diagnostic milestone for the native backend: pushes one real compressed
/// MP4 video sample through `IMFTransform::ProcessInput` on the D3D11 decoder.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn can_process_direct_mp4_sample(file_path: *const c_char) -> bool {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return false;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return false;
    };
    match direct_mft::can_process_first_direct_mp4_sample(&path) {
        Ok(result) => result,
        Err(error) => {
            set_last_error(&error);
            false
        }
    }
}

/// Executes one native decode output pull and verifies that the result is an
/// NV12 sample supplied by the hardware MFT. Presentation is kept separate.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn can_decode_direct_mp4_sample_to_nv12(file_path: *const c_char) -> bool {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return false;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return false;
    };
    match direct_mft::decode_first_direct_mp4_sample_to_nv12(&path) {
        Ok(sample) => {
            drop(sample);
            true
        }
        Err(error) => {
            set_last_error(&error);
            false
        }
    }
}

/// End-to-end diagnostic for the experimental native path: decode one MP4
/// sample to an NV12 GPU surface and present it to the wallpaper host window.
/// Normal playback still remains on libVLC until this probe is proven stable.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn can_present_direct_mp4_sample(file_path: *const c_char) -> bool {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return false;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return false;
    };
    let host = engine()
        .lock()
        .ok()
        .map(|state| state.host_window)
        .filter(|host| *host != 0);
    let Some(host) = host else {
        set_last_error("Wallpaper host window is not initialized.");
        return false;
    };
    match direct_mft::decode_and_present_first_direct_mp4_sample(
        &path,
        windows::Win32::Foundation::HWND(host as _),
    ) {
        Ok(()) => true,
        Err(error) => {
            set_last_error(&error);
            false
        }
    }
}

/// Runs the bounded native MP4 smoke test. Returns the number of presented
/// GPU frames; detailed errors are available through get_last_error.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn run_native_mp4_smoke_test(file_path: *const c_char) -> u32 {
    if file_path.is_null() {
        set_last_error("MP4 path is null.");
        return 0;
    }
    let path = unsafe { CStr::from_ptr(file_path) }
        .to_str()
        .ok()
        .map(str::to_owned);
    let Some(path) = path else {
        set_last_error("MP4 path is not valid UTF-8.");
        return 0;
    };
    let host = engine()
        .lock()
        .ok()
        .map(|state| state.host_window)
        .filter(|host| *host != 0);
    let Some(host) = host else {
        set_last_error("Wallpaper host window is not initialized.");
        return 0;
    };
    match direct_mft::run_native_mp4_smoke_test(
        &path,
        windows::Win32::Foundation::HWND(host as _),
        Duration::from_secs(8),
    ) {
        Ok(stats) => {
            set_last_error(&format!(
                "Native smoke test passed: {} input samples, {} presented GPU frames, {} frame-latency drops, {} compositor-busy drops.",
                stats.input_samples,
                stats.output_frames,
                stats.dropped_frame_latency,
                stats.dropped_compositor_busy
            ));
            stats.output_frames
        }
        Err(error) => {
            set_last_error(&error);
            0
        }
    }
}

/// Reports whether the active native renderer has actually presented at least
/// one GPU frame. This is deliberately stricter than successful initialization.
#[no_mangle]
pub extern "C" fn is_native_mp4_pipeline_ready() -> bool {
    engine()
        .lock()
        .map(|state| {
            state
                .native_renderer
                .as_ref()
                .is_some_and(|renderer| !renderer.has_failed() && renderer.frames_presented() > 0)
        })
        .unwrap_or(false)
}

/// Diagnostic counter used by automated native-renderer smoke tests. It
/// advances only after D3D11 VideoProcessorBlt and Present have both succeeded.
#[no_mangle]
pub extern "C" fn get_native_renderer_frame_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.frames_presented())
        })
        .unwrap_or(0)
}

/// Number of GPU NV12 frames produced by the hardware decoder. This can be
/// greater than the visible-frame count when scheduling or DWM drops a frame.
#[no_mangle]
pub extern "C" fn get_native_renderer_decoded_frame_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.frames_decoded())
        })
        .unwrap_or(0)
}

/// Total number of normal, non-fatal presentation drops.
#[no_mangle]
pub extern "C" fn get_native_renderer_dropped_frame_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.frames_dropped())
        })
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn get_native_renderer_frame_latency_drop_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.frames_dropped_frame_latency())
        })
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn get_native_renderer_compositor_busy_drop_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.frames_dropped_compositor_busy())
        })
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn get_native_renderer_late_drop_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.frames_dropped_late())
        })
        .unwrap_or(0)
}

/// Milliseconds since the last successful desktop Present. `u64::MAX` means
/// that this renderer has not shown a frame yet or native playback is inactive.
#[no_mangle]
pub extern "C" fn get_native_renderer_last_present_age_ms() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.last_present_age_ms())
        })
        .unwrap_or(u64::MAX)
}

/// Stable diagnostic category for the most recent native video failure.
/// Zero means that the current wallpaper has not encountered a native failure.
#[no_mangle]
pub extern "C" fn get_native_renderer_failure_code() -> u32 {
    engine()
        .lock()
        .map(|state| state.native_mp4_failure_code)
        .unwrap_or(u32::MAX)
}

/// 0=idle, 1=in progress, 2=active, 3=failed.
#[no_mangle]
pub extern "C" fn get_native_video_fallback_status() -> u32 {
    engine()
        .lock()
        .map(|state| state.video_fallback_status as u32)
        .unwrap_or(FallbackStatus::Failed as u32)
}

/// 0=idle, 1=in progress, 2=active, 3=failed.
#[no_mangle]
pub extern "C" fn get_native_audio_fallback_status() -> u32 {
    engine()
        .lock()
        .map(|state| state.audio_fallback_status as u32)
        .unwrap_or(FallbackStatus::Failed as u32)
}

/// 0=stopped, 1=starting, 2=native, 3=hybrid, 4=libVLC, 5=stopping, 6=failed.
#[no_mangle]
pub extern "C" fn get_engine_playback_status() -> u32 {
    engine()
        .lock()
        .map(|state| state.playback_status as u32)
        .unwrap_or(PlaybackStatus::Failed as u32)
}

/// Monotonically changing request generation used to reject stale startups.
#[no_mangle]
pub extern "C" fn get_engine_generation() -> u64 {
    engine()
        .lock()
        .map(|state| state.generation)
        .unwrap_or(u64::MAX)
}

/// Counts callbacks delivered by Media Foundation. A zero value after the
/// watchdog interval distinguishes a stalled source reader from a D3D11
/// presentation failure.
#[no_mangle]
pub extern "C" fn get_native_renderer_callback_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.callbacks_received())
        })
        .unwrap_or(0)
}

/// HRESULT reported by the latest Source Reader callback, or zero when no
/// callback has arrived yet. Exposed for automated diagnostics only.
#[no_mangle]
pub extern "C" fn get_native_renderer_last_callback_status() -> i32 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.last_callback_status())
        })
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn get_native_renderer_last_callback_flags() -> u32 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.last_callback_flags())
        })
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn get_native_renderer_last_request_status() -> i32 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_renderer
                .as_ref()
                .map(|renderer| renderer.last_request_status())
        })
        .unwrap_or(0)
}

/// Returns a diagnostic for the last native MP4 preflight. This is separate
/// from `get_last_error`: the wallpaper can succeed via libVLC fallback.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn get_native_mp4_diagnostic(buffer: *mut c_char, capacity: usize) -> usize {
    let message = engine()
        .lock()
        .map(|state| state.native_mp4_diagnostic.clone())
        .unwrap_or_else(|_| "Engine state lock failed.".into());
    let bytes = message.as_bytes();
    if !buffer.is_null() && capacity != 0 {
        let count = bytes.len().min(capacity - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), count);
            *buffer.add(count) = 0;
        }
    }
    bytes.len()
}

/// True only after native PCM has been submitted to the WASAPI endpoint.
#[no_mangle]
pub extern "C" fn is_native_audio_ready() -> bool {
    engine()
        .lock()
        .map(|state| {
            state
                .native_audio
                .as_ref()
                .is_some_and(|audio| !audio.has_failed() && audio.frames_written() > 0)
        })
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn get_native_audio_frame_count() -> u64 {
    engine()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .native_audio
                .as_ref()
                .map(|audio| audio.frames_written())
        })
        .unwrap_or(0)
}

#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn get_native_audio_diagnostic(buffer: *mut c_char, capacity: usize) -> usize {
    let message = engine()
        .lock()
        .map(|state| state.native_audio_diagnostic.clone())
        .unwrap_or_else(|_| "Engine state lock failed.".into());
    let bytes = message.as_bytes();
    if !buffer.is_null() && capacity != 0 {
        let count = bytes.len().min(capacity - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), count);
            *buffer.add(count) = 0;
        }
    }
    bytes.len()
}

/// Clears the static wallpaper and paints the desktop background solid black.
/// This is intentionally separate from `stop_engine`: exiting OpenPaper must
/// not unexpectedly alter a user's desktop, while selecting "No wallpaper" does.
#[no_mangle]
pub extern "C" fn set_black_desktop() -> bool {
    clear_playback_preserving_host();
    let color_indexes = [COLOR_DESKTOP.0];
    let black = [COLORREF(0)];
    let color_result = unsafe { SetSysColors(1, color_indexes.as_ptr(), black.as_ptr()).is_ok() };
    let wallpaper_result = unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            None,
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .is_ok()
    };

    if let Ok(mut state) = engine().lock() {
        if color_result && wallpaper_result {
            state.last_error.clear();
        } else {
            state.last_error = "Could not set the desktop background to black.".into();
        }
    }
    color_result && wallpaper_result
}

fn clear_playback_preserving_host() {
    let (generation, resources) = {
        let mut state = match engine().lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.generation = state.generation.wrapping_add(1);
        state.playback_status = PlaybackStatus::Stopping;
        state.active_media = None;
        state.video_fallback_status = FallbackStatus::Idle;
        state.audio_fallback_status = FallbackStatus::Idle;
        state.native_mp4_failure_code = 0;
        let resources = PlaybackResources {
            player: state.player.take(),
            native_renderer: state.native_renderer.take(),
            native_audio: state.native_audio.take(),
        };
        (state.generation, resources)
    };
    resources.stop();
    let _start_guard = playback_start_gate().lock().ok();
    if let Ok(mut state) = engine().lock() {
        if state.generation == generation && state.playback_status == PlaybackStatus::Stopping {
            state.playback_status = PlaybackStatus::Stopped;
        }
    }
}

fn set_last_error(message: &str) {
    if let Ok(mut state) = engine().lock() {
        state.last_error = message.into();
    }
}

unsafe extern "system" fn find_workerw(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // Place the render child in the same top-level parent as the desktop icon
    // view, then position it below that view in the child Z-order.
    if !FindWindowExW(
        hwnd,
        HWND::default(),
        w!("SHELLDLL_DefView"),
        PCWSTR::null(),
    )
    .unwrap_or_default()
    .0
    .is_null()
    {
        let output = lparam.0 as *mut HWND;
        *output = hwnd;
        return BOOL(0);
    }
    BOOL(1)
}

unsafe fn resize_to_parent(host: HWND, parent: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(parent, &mut rect).is_ok() {
        let _ = SetWindowPos(
            host,
            HWND::default(),
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
}

fn fullscreen_monitor(core: &'static Mutex<EngineState>) {
    // Deterministic end-to-end tests can opt out without changing the normal
    // user policy. The client never sets this diagnostic-only variable.
    let fullscreen_pause_disabled =
        std::env::var("OPENPAPER_DISABLE_FULLSCREEN_PAUSE").as_deref() == Ok("1");
    loop {
        let should_stop = core
            .lock()
            .map(|s| s.monitor_stop.load(Ordering::Acquire))
            .unwrap_or(true);
        if should_stop {
            break;
        }
        let fullscreen = !fullscreen_pause_disabled && unsafe { foreground_is_fullscreen() };
        let other_app_open = unsafe { foreground_is_external_application() };
        if let Ok(mut state) = core.lock() {
            fallback_to_vlc_if_native_failed(&mut state);
            fallback_audio_to_vlc_if_native_failed(&mut state);
            let was_paused = state.fullscreen_paused.swap(fullscreen, Ordering::AcqRel);
            if was_paused != fullscreen {
                if let Some(player) = state.player.as_ref() {
                    unsafe {
                        player.set_paused(fullscreen);
                    }
                }
                if let Some(renderer) = state.native_renderer.as_ref() {
                    renderer.set_paused(fullscreen);
                }
                if let Some(audio) = state.native_audio.as_ref() {
                    audio.set_paused(fullscreen);
                }
            }
            let automatic_mute = state.mute_when_other_app_open && other_app_open;
            if state.automatically_muted != automatic_mute {
                state.automatically_muted = automatic_mute;
                if let Some(player) = state.player.as_ref() {
                    unsafe { player.set_muted(state.muted || automatic_mute) };
                }
                if let Some(audio) = state.native_audio.as_ref() {
                    audio.set_muted(state.muted || automatic_mute);
                }
            }
        }
        thread::sleep(Duration::from_millis(750));
    }
}

/// The native renderer is experimental. If its dedicated thread exits after a
/// successful start, restore libVLC's proven D3D11VA visual output instead of
/// leaving an audio-only wallpaper running.
fn fallback_to_vlc_if_native_failed(state: &mut EngineState) {
    let failure = state
        .native_renderer
        .as_ref()
        .filter(|renderer| renderer.has_failed())
        .map(|renderer| {
            let diagnostic = renderer.failure_diagnostic();
            let code = renderer.failure_kind_code();
            let diagnostic = if diagnostic.is_empty() {
                format!("[FailureCode={code}] Native renderer stopped without a diagnostic.")
            } else {
                diagnostic
            };
            (code, diagnostic)
        });
    let Some((failure_code, failure)) = failure else {
        return;
    };
    if !state.video_fallback_status.try_begin() {
        return;
    }
    state.native_mp4_failure_code = failure_code;
    if let Some(renderer) = state.native_renderer.take() {
        renderer.stop();
    }
    if let Some(audio) = state.native_audio.take() {
        audio.stop();
    }
    if let Some(player) = state.player.take() {
        player.stop();
    }
    let Some(path) = state.active_media.clone() else {
        state.video_fallback_status = FallbackStatus::Failed;
        state.audio_fallback_status = FallbackStatus::Failed;
        state.playback_status = PlaybackStatus::Failed;
        state.last_error =
            "Native renderer stopped and no wallpaper path is available for fallback.".into();
        return;
    };
    match unsafe {
        vlc::VlcPlayer::start(
            &path,
            state.host_window as usize,
            state.performance_mode,
            true,
            state.stretch_to_fill,
        )
    } {
        Ok(player) => {
            unsafe {
                player.set_muted(state.muted || state.automatically_muted);
                player.set_volume(state.volume);
                player.set_paused(state.fullscreen_paused.load(Ordering::Acquire));
            }
            state.player = Some(player);
            state.video_fallback_status = FallbackStatus::Active;
            state.audio_fallback_status = FallbackStatus::Active;
            state.playback_status = PlaybackStatus::RunningVlc;
            state.native_mp4_diagnostic = format!(
                "Native renderer failed during playback; libVLC D3D11VA fallback is active. Cause: {failure}"
            );
            state.last_error.clear();
        }
        Err(error) => {
            state.video_fallback_status = FallbackStatus::Failed;
            state.audio_fallback_status = FallbackStatus::Failed;
            state.playback_status = PlaybackStatus::Failed;
            state.last_error = format!(
                "Native renderer failed and libVLC fallback could not start. Cause: {failure}. Fallback error: {error}"
            )
        }
    }
}

/// A late WASAPI/device failure must not stop the native D3D11 video. Restore
/// only libVLC audio and leave the direct video renderer running.
fn fallback_audio_to_vlc_if_native_failed(state: &mut EngineState) {
    let failure = state
        .native_audio
        .as_ref()
        .filter(|audio| audio.has_failed())
        .map(|audio| audio.failure_diagnostic());
    let Some(failure) = failure else {
        return;
    };
    if !state.audio_fallback_status.try_begin() {
        return;
    }
    if let Some(audio) = state.native_audio.take() {
        audio.stop();
    }
    if state.player.is_some() {
        state.audio_fallback_status = FallbackStatus::Active;
        state.playback_status = PlaybackStatus::RunningHybrid;
        return;
    }
    let Some(path) = state.active_media.clone() else {
        state.audio_fallback_status = FallbackStatus::Failed;
        state.playback_status = PlaybackStatus::RunningHybrid;
        state.last_error =
            "Native audio stopped and no wallpaper path is available for fallback.".into();
        return;
    };
    match unsafe {
        vlc::VlcPlayer::start(
            &path,
            state.host_window as usize,
            state.performance_mode,
            false,
            state.stretch_to_fill,
        )
    } {
        Ok(player) => {
            unsafe {
                player.set_muted(state.muted || state.automatically_muted);
                player.set_volume(state.volume);
                player.set_paused(state.fullscreen_paused.load(Ordering::Acquire));
            }
            state.player = Some(player);
            state.audio_fallback_status = FallbackStatus::Active;
            state.playback_status = PlaybackStatus::RunningHybrid;
            state.native_audio_diagnostic = format!(
                "Native audio failed during playback; libVLC audio fallback is active. Cause: {failure}"
            );
            state.last_error.clear();
        }
        Err(error) => {
            state.audio_fallback_status = FallbackStatus::Failed;
            state.playback_status = PlaybackStatus::RunningHybrid;
            state.last_error = format!(
                "Native audio failed and libVLC audio fallback could not start. Cause: {failure}. Fallback error: {error}"
            )
        }
    }
}

unsafe fn foreground_is_external_application() -> bool {
    let hwnd = GetForegroundWindow();
    if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return false;
    }
    let mut class_name = [0u16; 64];
    let length = GetClassNameW(hwnd, &mut class_name);
    let class_name = String::from_utf16_lossy(&class_name[..length as usize]);
    if matches!(
        class_name.as_str(),
        "Progman" | "WorkerW" | "WallpaperCoreHost" | "Shell_TrayWnd"
    ) {
        return false;
    }
    let mut process_id = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    // OpenPaper's own window is also an ordinary foreground application.
    // The user enabled this policy to silence wallpaper sound while working,
    // including while the OpenPaper UI itself is open.
    process_id != 0
}

unsafe fn foreground_is_fullscreen() -> bool {
    let hwnd = GetForegroundWindow();
    if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return false;
    }
    // When the user minimizes every app, Explorer's desktop window becomes the
    // foreground window. It covers the monitor by design, but it is not a
    // game/video app and must never pause the wallpaper itself.
    let mut class_name = [0u16; 64];
    let class_length = GetClassNameW(hwnd, &mut class_name);
    let class_name = String::from_utf16_lossy(&class_name[..class_length as usize]);
    if matches!(
        class_name.as_str(),
        "Progman" | "WorkerW" | "WallpaperCoreHost" | "Shell_TrayWnd"
    ) {
        return false;
    }
    let mut window = RECT::default();
    if GetWindowRect(hwnd, &mut window).is_err() {
        return false;
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return false;
    }
    let desktop = info.rcMonitor;
    // A tolerance handles invisible DWM borders while avoiding pauses for maximized windows.
    (window.left - desktop.left).abs() <= 2
        && (window.top - desktop.top).abs() <= 2
        && (window.right - desktop.right).abs() <= 2
        && (window.bottom - desktop.bottom).abs() <= 2
}

unsafe extern "system" fn host_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::{is_current_playback_request, FallbackStatus, PlaybackStatus};

    #[test]
    fn fallback_can_begin_only_once_until_the_next_wallpaper_resets_it() {
        let mut status = FallbackStatus::Idle;

        assert!(status.try_begin());
        assert_eq!(status, FallbackStatus::InProgress);
        assert!(!status.try_begin());

        status = FallbackStatus::Active;
        assert!(!status.try_begin());

        status = FallbackStatus::Failed;
        assert!(!status.try_begin());

        status = FallbackStatus::Idle;
        assert!(status.try_begin());
    }

    #[test]
    fn only_the_latest_starting_generation_can_commit_resources() {
        assert!(is_current_playback_request(8, PlaybackStatus::Starting, 8));
        assert!(!is_current_playback_request(9, PlaybackStatus::Starting, 8));
        assert!(!is_current_playback_request(8, PlaybackStatus::Stopping, 8));
        assert!(!is_current_playback_request(8, PlaybackStatus::Failed, 8));
    }
}
