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
