//! Narrow runtime binding for the stable libVLC 3 C API.
//! We deliberately load it dynamically: the LGPL library remains replaceable.

use std::{
    env,
    ffi::{c_char, c_int, c_void, CString},
    mem,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use url::Url;
use windows::{
    core::{PCSTR, PCWSTR},
    Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{GetProcAddress, LoadLibraryW, SetDllDirectoryW},
    },
};

type LibvlcNew = unsafe extern "C" fn(c_int, *const *const c_char) -> *mut c_void;
type LibvlcRelease = unsafe extern "C" fn(*mut c_void);
type MediaNewLocation = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type MediaAddOption = unsafe extern "C" fn(*mut c_void, *const c_char);
type MediaRelease = unsafe extern "C" fn(*mut c_void);
type PlayerNewFromMedia = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type PlayerRelease = unsafe extern "C" fn(*mut c_void);
type PlayerSetHwnd = unsafe extern "C" fn(*mut c_void, *mut c_void);
type PlayerPlay = unsafe extern "C" fn(*mut c_void) -> c_int;
type PlayerSetPause = unsafe extern "C" fn(*mut c_void, c_int);
type PlayerStop = unsafe extern "C" fn(*mut c_void);
type AudioSetMute = unsafe extern "C" fn(*mut c_void, c_int);
type AudioSetVolume = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

pub struct VlcPlayer {
    module: isize,
    instance: usize,
    media: usize,
    player: usize,
    release: LibvlcRelease,
    media_release: MediaRelease,
    player_release: PlayerRelease,
    player_set_pause: PlayerSetPause,
    player_stop: PlayerStop,
    audio_set_mute: AudioSetMute,
    audio_set_volume: AudioSetVolume,
}

impl VlcPlayer {
    /// Starts local media in the caller-owned Win32 host window.
    /// `OPENWALLPAPER_LIBVLC_DIR` must point at the NuGet `libvlc/win-x64` directory.
    pub unsafe fn start(
        path: &str,
        hwnd: usize,
        performance_mode: i32,
        show_video: bool,
    ) -> Result<Self, String> {
        let runtime = env::var_os("OPENWALLPAPER_LIBVLC_DIR")
            .map(PathBuf::from)
            .filter(|directory| directory.is_dir())
            .ok_or_else(|| "libVLC runtime directory is missing.".to_string())?;
        let library = runtime.join("libvlc.dll");
        let runtime_wide: Vec<u16> = runtime.as_os_str().encode_wide().chain(Some(0)).collect();
        SetDllDirectoryW(PCWSTR(runtime_wide.as_ptr()))
            .map_err(|error| format!("Cannot configure libVLC DLL search path: {error}"))?;
        let wide: Vec<u16> = library.as_os_str().encode_wide().chain(Some(0)).collect();
        let module = LoadLibraryW(PCWSTR(wide.as_ptr()))
            .map_err(|error| format!("Cannot load libvlc.dll: {error}"))?;

        macro_rules! symbol {
            ($name:literal, $type:ty) => {{
                let address = GetProcAddress(module, PCSTR(concat!($name, "\0").as_ptr()));
                let address =
                    address.ok_or_else(|| format!("libVLC export '{}' is missing.", $name))?;
                mem::transmute::<unsafe extern "system" fn() -> isize, $type>(address)
            }};
        }

        let libvlc_new: LibvlcNew = symbol!("libvlc_new", LibvlcNew);
        let release: LibvlcRelease = symbol!("libvlc_release", LibvlcRelease);
        let media_new_location: MediaNewLocation =
            symbol!("libvlc_media_new_location", MediaNewLocation);
        let media_add_option: MediaAddOption = symbol!("libvlc_media_add_option", MediaAddOption);
        let media_release: MediaRelease = symbol!("libvlc_media_release", MediaRelease);
        let player_new_from_media: PlayerNewFromMedia =
            symbol!("libvlc_media_player_new_from_media", PlayerNewFromMedia);
        let player_release: PlayerRelease = symbol!("libvlc_media_player_release", PlayerRelease);
        let player_set_hwnd: PlayerSetHwnd = symbol!("libvlc_media_player_set_hwnd", PlayerSetHwnd);
        let player_play: PlayerPlay = symbol!("libvlc_media_player_play", PlayerPlay);
        let player_set_pause: PlayerSetPause =
            symbol!("libvlc_media_player_set_pause", PlayerSetPause);
        let player_stop: PlayerStop = symbol!("libvlc_media_player_stop", PlayerStop);
        let audio_set_mute: AudioSetMute = symbol!("libvlc_audio_set_mute", AudioSetMute);
        let audio_set_volume: AudioSetVolume = symbol!("libvlc_audio_set_volume", AudioSetVolume);

        let plugin_path = CString::new(format!(
            "--plugin-path={}",
            runtime.join("plugins").display()
        ))
        .map_err(|_| "Invalid libVLC plugin path.".to_string())?;
        let mut options = vec![
            CString::new("--no-video-title-show").unwrap(),
            CString::new("--no-osd").unwrap(),
            CString::new("--no-media-library").unwrap(),
            // `--repeat` repeats a playlist item in VLC's UI. libVLC embeds
            // one media item without that playlist controller, so it can
            // still stop at EOF. Input repeat is owned by the demux/input
            // layer and works for MP4, GIF and WebP alike.
            CString::new("--input-repeat=-1").unwrap(),
            // Keep D3D11VA decoder surfaces and the presentation swap chain
            // in the same D3D11 pipeline. Without an explicit D3D11 vout,
            // VLC may choose a different renderer and add a GPU/CPU copy.
            CString::new("--vout=direct3d11").unwrap(),
            CString::new("--avcodec-hw=d3d11va").unwrap(),
            plugin_path,
        ];
        if performance_mode <= 1 {
            options.push(CString::new("--drop-late-frames").unwrap());
        }
        if performance_mode == 0 {
            options.push(CString::new("--skip-frames").unwrap());
        }
        if !show_video {
            options.push(CString::new("--no-video").unwrap());
        }
        let arguments: Vec<*const c_char> = options.iter().map(|value| value.as_ptr()).collect();
        let instance = libvlc_new(arguments.len() as c_int, arguments.as_ptr());
        if instance.is_null() {
            let _ = FreeLibrary(module);
            return Err("libVLC could not create a playback instance.".into());
        }

        let location = Url::from_file_path(Path::new(path))
            .map_err(|_| "Could not convert the wallpaper path to a file URI.".to_string())?;
        let media_location = CString::new(location.as_str())
            .map_err(|_| "Wallpaper URI contains an invalid NUL byte.".to_string())?;
        let media = media_new_location(instance, media_location.as_ptr());
        if media.is_null() {
            release(instance);
            let _ = FreeLibrary(module);
            return Err("libVLC could not open the media URI.".into());
        }
        // Per-media loop: a wallpaper must never fall back to a VLC splash or
        // a stopped black frame at end-of-file. -1 means infinite repeats.
        let repeat = CString::new(":input-repeat=-1").unwrap();
        media_add_option(media, repeat.as_ptr());
        let player = player_new_from_media(media);
        if player.is_null() {
            media_release(media);
            release(instance);
            let _ = FreeLibrary(module);
            return Err("libVLC could not create a media player.".into());
        }

        if show_video {
            player_set_hwnd(player, hwnd as *mut c_void);
        }
        if player_play(player) != 0 {
            player_release(player);
            media_release(media);
            release(instance);
            let _ = FreeLibrary(module);
            return Err("libVLC rejected the playback request.".into());
        }

        Ok(Self {
            module: module.0 as isize,
            instance: instance as usize,
            media: media as usize,
            player: player as usize,
            release,
            media_release,
            player_release,
            player_set_pause,
            player_stop,
            audio_set_mute,
            audio_set_volume,
        })
    }

    pub unsafe fn set_paused(&self, paused: bool) {
        (self.player_set_pause)(self.player as *mut c_void, i32::from(paused));
    }

    pub unsafe fn set_muted(&self, muted: bool) {
        (self.audio_set_mute)(self.player as *mut c_void, i32::from(muted));
    }

    pub unsafe fn set_volume(&self, volume: i32) {
        let _ = (self.audio_set_volume)(self.player as *mut c_void, volume.clamp(0, 100));
    }

    pub fn stop(self) {
        unsafe {
            (self.player_stop)(self.player as *mut c_void);
            (self.player_release)(self.player as *mut c_void);
            (self.media_release)(self.media as *mut c_void);
            (self.release)(self.instance as *mut c_void);
            let _ = FreeLibrary(windows::Win32::Foundation::HMODULE(self.module as _));
        }
    }
}
