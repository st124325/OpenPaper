using System.IO;
using System.Text.Json;

namespace WallpaperClient;

internal sealed class AppSettingsStore
{
    private static readonly string SettingsPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "OpenPaper", "settings.json");
    private static readonly string LegacySettingsPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "OpenWallpaper", "settings.json");
    internal AppSettings Load()
    {
        try
        {
            var source = File.Exists(SettingsPath) ? SettingsPath : LegacySettingsPath;
            return File.Exists(source)
                ? JsonSerializer.Deserialize<AppSettings>(File.ReadAllText(source)) ?? new AppSettings()
                : new AppSettings();
        }
        catch (JsonException) { return new AppSettings(); }
    }
    internal void Save(AppSettings settings)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(SettingsPath)!);
        File.WriteAllText(SettingsPath, JsonSerializer.Serialize(settings));
    }
}

internal sealed record AppSettings(
    string? WallpaperPath = null,
    bool StartWithWindows = false,
    string Language = "ru",
    List<string>? Library = null,
    int Volume = 100,
    Dictionary<string, string>? LibraryTitles = null,
    bool MuteWhenOtherAppOpen = false,
    int PerformanceMode = 1);
