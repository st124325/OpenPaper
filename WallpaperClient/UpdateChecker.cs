using System.Diagnostics;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using System.Windows;

namespace WallpaperClient;

/// <summary>Downloads a newer release in the background and applies it on normal exit.</summary>
internal static class UpdateChecker
{
    private const string LatestReleaseApi = "https://api.github.com/repos/st124325/OpenPaper/releases/latest";
    private const string InstallerName = "OpenPaper-Setup-win-x64.exe";
    private static readonly HttpClient Client = new();
    private static int _checking;
    private static string? _pendingInstaller;

    internal static event Action<string>? UpdateReady;

    internal static async Task CheckAsync(Window? owner = null)
    {
        if (Interlocked.Exchange(ref _checking, 1) != 0) return;

        try
        {
            using var request = new HttpRequestMessage(HttpMethod.Get, LatestReleaseApi);
            request.Headers.UserAgent.ParseAdd("OpenPaper/0.0");
            using var response = await Client.SendAsync(request);
            response.EnsureSuccessStatusCode();
            using var document = JsonDocument.Parse(await response.Content.ReadAsStringAsync());

            var release = document.RootElement;
            var versionText = release.GetProperty("tag_name").GetString()?.TrimStart('v');
            var currentText = typeof(UpdateChecker).Assembly.GetName().Version?.ToString(3);
            if (!Version.TryParse(versionText, out var available) ||
                !Version.TryParse(currentText, out var current) ||
                available <= current)
            {
                return;
            }

            string? downloadUrl = null;
            long expectedSize = 0;
            foreach (var asset in release.GetProperty("assets").EnumerateArray())
            {
                if (!string.Equals(asset.GetProperty("name").GetString(), InstallerName, StringComparison.OrdinalIgnoreCase)) continue;
                downloadUrl = asset.GetProperty("browser_download_url").GetString();
                expectedSize = asset.GetProperty("size").GetInt64();
                break;
            }
            if (!Uri.TryCreate(downloadUrl, UriKind.Absolute, out var installerUri) ||
                !string.Equals(installerUri.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase)) return;

            var updateDirectory = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "OpenPaper", "updates");
            Directory.CreateDirectory(updateDirectory);
            var installerPath = Path.Combine(updateDirectory, $"OpenPaper-{available}-Setup.exe");
            if (!File.Exists(installerPath) || (expectedSize > 0 && new FileInfo(installerPath).Length != expectedSize))
            {
                var temporaryPath = installerPath + ".download";
                using var downloadResponse = await Client.GetAsync(installerUri, HttpCompletionOption.ResponseHeadersRead);
                downloadResponse.EnsureSuccessStatusCode();
                await using var source = await downloadResponse.Content.ReadAsStreamAsync();
                await using var destination = new FileStream(temporaryPath, FileMode.Create, FileAccess.Write, FileShare.None);
                await source.CopyToAsync(destination);
                await destination.FlushAsync();
                if (expectedSize > 0 && new FileInfo(temporaryPath).Length != expectedSize)
                {
                    File.Delete(temporaryPath);
                    return;
                }
                File.Move(temporaryPath, installerPath, true);
            }

            _pendingInstaller = installerPath;
            await System.Windows.Application.Current.Dispatcher.InvokeAsync(() => UpdateReady?.Invoke(available.ToString(3)));
        }
        catch
        {
            // The application must stay usable without network access or an update server.
        }
        finally
        {
            Interlocked.Exchange(ref _checking, 0);
        }
    }

    internal static bool TryLaunchPendingInstaller()
    {
        var installer = _pendingInstaller;
        if (string.IsNullOrWhiteSpace(installer) || !File.Exists(installer)) return false;
        try
        {
            Process.Start(new ProcessStartInfo(installer!)
            {
                UseShellExecute = true,
                Arguments = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS"
            });
            return true;
        }
        catch
        {
            return false;
        }
    }
}
