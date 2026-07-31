using System.Net.Http;
using System.Text.Json;
using System.Windows;

namespace WallpaperClient;

internal static class UpdateChecker
{
    private const string LatestReleaseApi = "https://api.github.com/repos/st124325/OpenPaper/releases/latest";
    private static readonly HttpClient Client = new();
    private static int _checking;

    internal static async Task CheckAsync(Window? owner = null)
    {
        if (Interlocked.Exchange(ref _checking, 1) != 0) return;
        try
        {
            using var request = new HttpRequestMessage(HttpMethod.Get, LatestReleaseApi);
            request.Headers.UserAgent.ParseAdd("OpenPaper/0.0");
            using var response = await Client.SendAsync(request);
            response.EnsureSuccessStatusCode();
            var json = await response.Content.ReadAsStringAsync();
            using var document = JsonDocument.Parse(json);
            var tag = document.RootElement.GetProperty("tag_name").GetString()?.TrimStart('v');
            var current = typeof(UpdateChecker).Assembly.GetName().Version?.ToString(3);
            var page = document.RootElement.GetProperty("html_url").GetString();
            var notes = document.RootElement.TryGetProperty("body", out var body) ? body.GetString() : null;
            if (string.IsNullOrWhiteSpace(tag) || string.IsNullOrWhiteSpace(page) ||
                !Version.TryParse(tag, out var available) || !Version.TryParse(current, out var installed) ||
                available <= installed) return;
            await System.Windows.Application.Current.Dispatcher.InvokeAsync(() =>
            {
                var dialog = new UpdateDialog(tag, page, notes);
                if (owner is { IsVisible: true }) dialog.Owner = owner;
                dialog.ShowDialog();
            });
        }
        catch { /* Offline or no release: application works normally. */ }
        finally { Interlocked.Exchange(ref _checking, 0); }
    }
}
