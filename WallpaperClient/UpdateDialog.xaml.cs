using System.Diagnostics;
using System.Windows;

namespace WallpaperClient;

public partial class UpdateDialog : Window
{
    private readonly string _releasePage;
    public UpdateDialog(string version, string releasePage, string? notes)
    {
        InitializeComponent();
        _releasePage = releasePage;
        VersionText.Text = $"Версия {version} готова к скачиванию";
        NotesText.Text = string.IsNullOrWhiteSpace(notes) ? "Вышла новая версия OpenPaper с улучшениями и исправлениями." : notes;
    }
    private void Later_Click(object sender, RoutedEventArgs e) => Close();
    private void Download_Click(object sender, RoutedEventArgs e)
    {
        Process.Start(new ProcessStartInfo(_releasePage) { UseShellExecute = true });
        Close();
    }
}
