using System.Runtime.InteropServices;
using System.Text;

namespace WallpaperClient;

internal static partial class WallpaperEngineInterop
{
    private const string LibraryName = "wallpaper_core";

    [LibraryImport(LibraryName, EntryPoint = "init_engine")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool InitEngine();

    // StringMarshalling.Utf8 creates a temporary NUL-terminated buffer and frees it
    // after the call. Rust copies it immediately, so neither side retains ownership.
    [LibraryImport(LibraryName, EntryPoint = "set_wallpaper", StringMarshalling = StringMarshalling.Utf8)]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetWallpaper(string filePath);

    [LibraryImport(LibraryName, EntryPoint = "stop_engine")]
    internal static partial void StopEngine();

    [LibraryImport(LibraryName, EntryPoint = "set_black_desktop")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetBlackDesktop();

    [LibraryImport(LibraryName, EntryPoint = "set_muted")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetMuted([MarshalAs(UnmanagedType.I1)] bool muted);

    [LibraryImport(LibraryName, EntryPoint = "set_volume")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetVolume(int volume);

    [LibraryImport(LibraryName, EntryPoint = "set_mute_when_other_app_open")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetMuteWhenOtherAppOpen([MarshalAs(UnmanagedType.I1)] bool enabled);

    [LibraryImport(LibraryName, EntryPoint = "set_performance_mode")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetPerformanceMode(int mode);

    [LibraryImport(LibraryName, EntryPoint = "set_stretch_to_fill")]
    [return: MarshalAs(UnmanagedType.I1)]
    internal static partial bool SetStretchToFill([MarshalAs(UnmanagedType.I1)] bool enabled);

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_frame_count")]
    internal static partial ulong GetNativeRendererPresentedFrameCount();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_decoded_frame_count")]
    internal static partial ulong GetNativeRendererDecodedFrameCount();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_dropped_frame_count")]
    internal static partial ulong GetNativeRendererDroppedFrameCount();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_frame_latency_drop_count")]
    internal static partial ulong GetNativeRendererFrameLatencyDropCount();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_compositor_busy_drop_count")]
    internal static partial ulong GetNativeRendererCompositorBusyDropCount();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_late_drop_count")]
    internal static partial ulong GetNativeRendererLateDropCount();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_last_present_age_ms")]
    internal static partial ulong GetNativeRendererLastPresentAgeMilliseconds();

    [LibraryImport(LibraryName, EntryPoint = "get_native_renderer_failure_code")]
    internal static partial uint GetNativeRendererFailureCode();

    [LibraryImport(LibraryName, EntryPoint = "get_native_video_fallback_status")]
    internal static partial uint GetNativeVideoFallbackStatus();

    [LibraryImport(LibraryName, EntryPoint = "get_native_audio_fallback_status")]
    internal static partial uint GetNativeAudioFallbackStatus();

    [LibraryImport(LibraryName, EntryPoint = "get_last_error")]
    private static unsafe partial nuint GetLastError(byte* buffer, nuint capacity);

    internal static unsafe string GetLastErrorMessage()
    {
        var buffer = new byte[2048];
        fixed (byte* pointer = buffer)
        {
            var length = GetLastError(pointer, (nuint)buffer.Length);
            var copiedLength = (int)Math.Min(length, (nuint)(buffer.Length - 1));
            return Encoding.UTF8.GetString(buffer, 0, copiedLength);
        }
    }
}
