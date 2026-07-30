using System.Diagnostics;
using System.Net.Http;
using System.Text.Json;
using System.Windows;

namespace WallpaperClient;

internal static class UpdateChecker
{
    private const string LatestReleaseApi = "https://api.github.com/repos/st124325/OpenPaper/releases/latest";
    private static readonly HttpClient Client = new();

    internal static async Task CheckAsync()
    {
        try
        {
            Client.DefaultRequestHeaders.UserAgent.ParseAdd("OpenPaper/1.0");
            var json = await Client.GetStringAsync(LatestReleaseApi);
            using var document = JsonDocument.Parse(json);
            var tag = document.RootElement.GetProperty("tag_name").GetString()?.TrimStart('v');
            var current = typeof(UpdateChecker).Assembly.GetName().Version?.ToString(3);
            var page = document.RootElement.GetProperty("html_url").GetString();
            if (string.IsNullOrWhiteSpace(tag) || string.IsNullOrWhiteSpace(page) || tag == current) return;
            await System.Windows.Application.Current.Dispatcher.InvokeAsync(() =>
            {
                if (System.Windows.MessageBox.Show($"Доступна версия {tag}. Открыть страницу обновления?", "OpenPaper", MessageBoxButton.YesNo, MessageBoxImage.Information) == MessageBoxResult.Yes)
                    Process.Start(new ProcessStartInfo(page) { UseShellExecute = true });
            });
        }
        catch { /* Offline or no release: application works normally. */ }
    }
}
