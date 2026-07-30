using System.IO;
using System.Security.Cryptography;
using System.Text;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace WallpaperClient;

internal static class ThumbnailCache
{
    private static readonly string CacheDirectory = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "OpenPaper", "thumbnails");
    internal static Uri? Get(string? path)
    {
        if (path is null) return null;
        var file = FileFor(path);
        return File.Exists(file) ? new Uri(file) : null;
    }
    internal static bool Save(MediaElement player, string path)
    {
        try
        {
            Directory.CreateDirectory(CacheDirectory);
            var bitmap = new RenderTargetBitmap((int)player.ActualWidth, (int)player.ActualHeight, 96, 96, PixelFormats.Pbgra32);
            bitmap.Render(player);
            using var stream = File.Create(FileFor(path));
            var encoder = new PngBitmapEncoder(); encoder.Frames.Add(BitmapFrame.Create(bitmap)); encoder.Save(stream);
            return true;
        }
        catch { return false; }
    }
    private static string FileFor(string path)
    {
        var stamp = File.Exists(path) ? File.GetLastWriteTimeUtc(path).Ticks : 0;
        var bytes = SHA256.HashData(Encoding.UTF8.GetBytes(path + stamp));
        return Path.Combine(CacheDirectory, Convert.ToHexString(bytes) + ".png");
    }
}
