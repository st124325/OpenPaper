using System.Windows;

namespace WallpaperClient;

public partial class WallpaperNameDialog : Window
{
    public string WallpaperTitle => TitleBox.Text.Trim();
    public WallpaperNameDialog(string initialTitle) { InitializeComponent(); TitleBox.Text = initialTitle; TitleBox.SelectAll(); TitleBox.Focus(); }
    private void Add_Click(object sender, RoutedEventArgs e) { if (string.IsNullOrWhiteSpace(WallpaperTitle)) return; DialogResult = true; }
    private void Cancel_Click(object sender, RoutedEventArgs e) => DialogResult = false;
}
