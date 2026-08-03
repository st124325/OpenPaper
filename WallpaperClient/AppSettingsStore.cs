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
            if (!File.Exists(source)) return new AppSettings();
            var json = File.ReadAllText(source);
            var settings = JsonSerializer.Deserialize<AppSettings>(json) ?? new AppSettings();
            using var document = JsonDocument.Parse(json);
            return document.RootElement.TryGetProperty(nameof(AppSettings.StretchToFill), out _)
                ? settings
                : settings with { StretchToFill = true };
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
    int PerformanceMode = 1,
    bool StretchToFill = true);
