//! Minimal, C-ABI-stable Windows wallpaper core.
//! Playback is provided by dynamically loaded libVLC (LGPL-2.1-or-later).

mod mf_d3d11;
mod vlc;

use std::{
    ffi::CStr,
    os::raw::c_char,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
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

struct EngineState {
    host_window: isize,
    active_media: Option<String>,
    muted: bool,
    mute_when_other_app_open: bool,
    automatically_muted: bool,
    volume: i32,
    performance_mode: i32,
    d3d11_media_foundation_available: bool,
    native_mp4_diagnostic: String,
    last_error: String,
    player: Option<vlc::VlcPlayer>,
    native_renderer: Option<mf_d3d11::NativeMp4Renderer>,
    monitor_stop: AtomicBool,
    fullscreen_paused: AtomicBool,
    monitor: Option<JoinHandle<()>>,
}

static ENGINE: OnceLock<Mutex<EngineState>> = OnceLock::new();

fn engine() -> &'static Mutex<EngineState> {
    ENGINE.get_or_init(|| {
        Mutex::new(EngineState {
            host_window: 0,
            active_media: None,
            muted: false,
            mute_when_other_app_open: false,
            automatically_muted: false,
            volume: 100,
            performance_mode: 1,
            d3d11_media_foundation_available: false,
            native_mp4_diagnostic: String::new(),
            last_error: String::new(),
            player: None,
            native_renderer: None,
            monitor_stop: AtomicBool::new(false),
            fullscreen_paused: AtomicBool::new(false),
            monitor: None,
        })
    })
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

    // Async Source Reader startup never waits for a decoded frame. A watchdog
    // in the fullscreen monitor falls back to libVLC if no GPU frame appears.
    let experimental_native_requested =
        std::env::var("OPENPAPER_EXPERIMENTAL_D3D11").as_deref() == Ok("1");
    let mut state = match engine().lock() {
        Ok(value) => value,
        Err(_) => return false,
    };
    let native_mp4_requested = experimental_native_requested
        && Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("mp4"));
    state.native_mp4_diagnostic = if native_mp4_requested {
        "Native renderer is starting asynchronously; GPU-frame watchdog is armed.".into()
    } else if experimental_native_requested {
        "Native Media Foundation rendering currently supports MP4 only.".into()
    } else {
        "Native renderer is disabled in the stable build; libVLC D3D11VA is active.".into()
    };
    if let Some(player) = state.player.take() {
        player.stop();
    }
    if let Some(renderer) = state.native_renderer.take() {
        renderer.stop();
    }
    // The native D3D11 renderer is intentionally opt-in until it succeeds
    // reliably on varied real-world MP4 samples. Stable releases keep the
    // proven libVLC D3D11VA output enabled by default.
    let native_renderer = if native_mp4_requested && state.d3d11_media_foundation_available {
        match mf_d3d11::NativeMp4Renderer::start(path.clone(), HWND(state.host_window as _)) {
            Ok(renderer) => Some(renderer),
            Err(error) => {
                state.native_mp4_diagnostic = format!("Native renderer did not start: {error}");
                None
            }
        }
    } else {
        None
    };
    let show_vlc_video = native_renderer.is_none();
    let player = match unsafe {
        vlc::VlcPlayer::start(
            &path,
            state.host_window as usize,
            state.performance_mode,
            show_vlc_video,
        )
    } {
        Ok(player) => player,
        Err(error) => {
            if let Some(renderer) = native_renderer {
                renderer.stop();
            }
            state.last_error = error;
            return false;
        }
    };
    state.active_media = Some(path);
    unsafe { player.set_muted(state.muted || state.automatically_muted) };
    unsafe { player.set_volume(state.volume) };
    state.player = Some(player);
    state.native_renderer = native_renderer;
    state.last_error.clear();
    true
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

/// Mutes or unmutes the currently running libVLC player.
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
    let monitor = {
        let mut state = match engine().lock() {
            Ok(value) => value,
            Err(_) => return,
        };
        state.monitor_stop.store(true, Ordering::Release);
        state.active_media = None;
        state.last_error.clear();
        if let Some(player) = state.player.take() {
            player.stop();
        }
        if let Some(renderer) = state.native_renderer.take() {
            renderer.stop();
        }
        if state.host_window != 0 {
            unsafe {
                let _ = DestroyWindow(HWND(state.host_window as _));
            }
            state.host_window = 0;
        }
        state.monitor.take()
    };
    if let Some(thread) = monitor {
        let _ = thread.join();
    }
}

/// Reports whether the native Media Foundation + D3D11 renderer can be used
/// on this computer. The current release still falls back to libVLC playback
/// while the native MP4 decoder and presenter are introduced incrementally.
#[no_mangle]
pub extern "C" fn is_native_renderer_available() -> bool {
    engine()
        .lock()
        .map(|state| state.d3d11_media_foundation_available)
        .unwrap_or(false)
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

/// Clears the static wallpaper and paints the desktop background solid black.
/// This is intentionally separate from `stop_engine`: exiting OpenPaper must
/// not unexpectedly alter a user's desktop, while selecting "No wallpaper" does.
#[no_mangle]
pub extern "C" fn set_black_desktop() -> bool {
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
    loop {
        let should_stop = core
            .lock()
            .map(|s| s.monitor_stop.load(Ordering::Acquire))
            .unwrap_or(true);
        if should_stop {
            break;
        }
        let fullscreen = unsafe { foreground_is_fullscreen() };
        let other_app_open = unsafe { foreground_is_external_application() };
        if let Ok(mut state) = core.lock() {
            fallback_to_vlc_if_native_failed(&mut state);
            let was_paused = state.fullscreen_paused.swap(fullscreen, Ordering::AcqRel);
            if was_paused != fullscreen {
                if let Some(player) = state.player.as_ref() {
                    unsafe {
                        player.set_paused(fullscreen);
                    }
                }
            }
            let automatic_mute = state.mute_when_other_app_open && other_app_open;
            if state.automatically_muted != automatic_mute {
                state.automatically_muted = automatic_mute;
                if let Some(player) = state.player.as_ref() {
                    unsafe { player.set_muted(state.muted || automatic_mute) };
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
    let native_failed = state
        .native_renderer
        .as_ref()
        .is_some_and(|renderer| renderer.has_failed());
    if !native_failed {
        return;
    }
    if let Some(renderer) = state.native_renderer.take() {
        renderer.stop();
    }
    if let Some(player) = state.player.take() {
        player.stop();
    }
    let Some(path) = state.active_media.as_deref() else {
        state.last_error =
            "Native renderer stopped and no wallpaper path is available for fallback.".into();
        return;
    };
    match unsafe {
        vlc::VlcPlayer::start(
            path,
            state.host_window as usize,
            state.performance_mode,
            true,
        )
    } {
        Ok(player) => {
            unsafe {
                player.set_muted(state.muted || state.automatically_muted);
                player.set_volume(state.volume);
                player.set_paused(state.fullscreen_paused.load(Ordering::Acquire));
            }
            state.player = Some(player);
            state.native_mp4_diagnostic =
                "Native renderer failed during playback; libVLC D3D11VA fallback is active.".into();
            state.last_error.clear();
        }
        Err(error) => {
            state.last_error =
                format!("Native renderer failed and libVLC fallback could not start: {error}")
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
