using System.ComponentModel;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;

namespace WallpaperClient;

public partial class MainWindow : Window
{
    private readonly AppSettingsStore _settingsStore = new();
    private AppSettings _settings = new();
    private bool _loading;
    private bool _muted;
    private int _lastVolume = 70;

    public MainWindow()
    {
        InitializeComponent();
        LibraryPanel.Visibility = Visibility.Visible;
        SettingsPanel.Visibility = Visibility.Collapsed;
        _loading = true;
        _settings = _settingsStore.Load();
        AutoStartCheckBox.IsChecked = _settings.StartWithWindows;
        OtherAppsMuteCheckBox.IsChecked = _settings.MuteWhenOtherAppOpen;
        PerformanceComboBox.SelectedIndex = Math.Clamp(_settings.PerformanceMode, 0, 2);
        VolumeSlider.Value = Math.Clamp(_settings.Volume, 0, 100);
        _muted = VolumeSlider.Value == 0;
        _lastVolume = _muted ? 70 : (int)VolumeSlider.Value;
        RefreshLibrary(_settings.WallpaperPath);
        ApplyLanguage();
        _loading = false;

        if (!WallpaperEngineInterop.InitEngine()) { SetStatus($"Engine error: {WallpaperEngineInterop.GetLastErrorMessage()}", $"Ошибка движка: {WallpaperEngineInterop.GetLastErrorMessage()}"); return; }
        WallpaperEngineInterop.SetVolume((int)VolumeSlider.Value);
        WallpaperEngineInterop.SetMuteWhenOtherAppOpen(_settings.MuteWhenOtherAppOpen);
        WallpaperEngineInterop.SetPerformanceMode(_settings.PerformanceMode);
        if (!string.IsNullOrWhiteSpace(_settings.WallpaperPath) && File.Exists(_settings.WallpaperPath)) ApplyWallpaper(_settings.WallpaperPath!, false);
        else SetStatus("Engine ready. Upload a wallpaper to start.", "Движок готов. Загрузите обои, чтобы начать.");
    }

    private bool IsRussian => _settings.Language != "en";
    private string T(string en, string ru) => IsRussian ? ru : en;
    private void SetStatus(string en, string ru)
    {
        if (en == "Language updated.") return;
        StatusText.Text = T(en, ru);
    }

    private void ApplyLanguage()
    {
        TitleText.Text = "OpenPaper";
        SettingsButton.Content = SettingsPanel.Visibility == Visibility.Visible ? T("Library", "Библиотека") : T("Settings", "Настройки");
        FormatText.Text = T("Your wallpapers are stored locally. Hover a video card to preview it.", "Ваши обои хранятся локально. Наведите на карточку видео для предпросмотра.");
        LibraryTitleText.Text = T("My library", "Моя библиотека");
        AddLibraryButton.Content = T("Upload wallpaper", "Загрузить обои");
        EmptyLibraryText.Text = T("The library is empty. Upload an MP4, GIF, or WEBP wallpaper below.", "Библиотека пуста. Загрузите ниже обои в формате MP4, GIF или WEBP.");
        LanguageSettingText.Text = T("Application language", "Язык приложения");
        AutoStartLabelText.Text = T("Start with Windows", "Запускать вместе с Windows");
        OtherAppsMuteLabelText.Text = T("Mute wallpaper when another app is active", "Отключать звук обоев при открытом приложении");
        PerformanceLabelText.Text = T("Performance", "Производительность");
        MuteButton.ToolTip = T(_muted ? "Turn sound on" : "Turn sound off", _muted ? "Включить звук" : "Выключить звук");
        MuteButton.Background = _muted
            ? new SolidColorBrush(System.Windows.Media.Color.FromRgb(220, 224, 228))
            : new SolidColorBrush(System.Windows.Media.Color.FromRgb(20, 20, 20));
        MuteButton.Foreground = _muted ? System.Windows.Media.Brushes.Black : System.Windows.Media.Brushes.White;
        SoundOnIcon.Visibility = _muted ? Visibility.Collapsed : Visibility.Visible;
        SoundWaveInnerIcon.Visibility = _muted ? Visibility.Collapsed : Visibility.Visible;
        SoundWaveOuterIcon.Visibility = _muted || VolumeSlider.Value < 50 ? Visibility.Collapsed : Visibility.Visible;
        SoundMutedIcon.Visibility = _muted ? Visibility.Visible : Visibility.Collapsed;
        VolumeValueText.Text = ((int)VolumeSlider.Value).ToString();
        UpdateLanguagePill();
    }

    private void UpdateLanguagePill()
    {
        var active = new SolidColorBrush(System.Windows.Media.Color.FromRgb(20, 20, 20));
        RuButton.Background = IsRussian ? active : System.Windows.Media.Brushes.Transparent;
        RuButton.Foreground = IsRussian ? System.Windows.Media.Brushes.White : System.Windows.Media.Brushes.Black;
        EnButton.Background = IsRussian ? System.Windows.Media.Brushes.Transparent : active;
        EnButton.Foreground = IsRussian ? System.Windows.Media.Brushes.Black : System.Windows.Media.Brushes.White;
    }

    private void Settings_Click(object sender, RoutedEventArgs e)
    {
        var showSettings = SettingsPanel.Visibility != Visibility.Visible;
        SettingsPanel.Visibility = showSettings ? Visibility.Visible : Visibility.Collapsed;
        LibraryPanel.Visibility = showSettings ? Visibility.Collapsed : Visibility.Visible;
        ApplyLanguage();
    }

    private void Window_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        // Real responsive layout: the gallery receives the actual available
        // width and WrapPanel reflows cards instead of scaling the whole UI.
        var compact = e.NewSize.Width < 580;
        MainSurface.Padding = compact ? new Thickness(16) : new Thickness(28);
        TitleText.FontSize = compact ? 20 : 24;
        SettingsButton.Padding = compact ? new Thickness(12, 8, 12, 8) : new Thickness(18, 11, 18, 11);
        HeaderAudioColumn.Width = new GridLength(compact ? 158 : 214);
    }
    private void Russian_Click(object sender, RoutedEventArgs e) => ChangeLanguage("ru");
    private void English_Click(object sender, RoutedEventArgs e) => ChangeLanguage("en");
    private void ChangeLanguage(string language) { _settings = _settings with { Language = language }; _settingsStore.Save(_settings); ApplyLanguage(); SetStatus("Language updated.", "Язык обновлён."); }

    private void AddLibrary_Click(object sender, RoutedEventArgs e)
    {
        var dialog = new Microsoft.Win32.OpenFileDialog { Filter = T("Wallpapers|*.mp4;*.gif;*.webp", "Обои|*.mp4;*.gif;*.webp"), Multiselect = false };
        if (dialog.ShowDialog() != true) return;
        var nameDialog = new WallpaperNameDialog(Path.GetFileNameWithoutExtension(dialog.FileName)) { Owner = this };
        if (nameDialog.ShowDialog() != true) return;
        var library = GetLibrary();
        var path = Path.GetFullPath(dialog.FileName);
        if (!library.Contains(path, StringComparer.OrdinalIgnoreCase)) library.Add(path);
        var titles = GetTitles();
        titles[path] = nameDialog.WallpaperTitle;
        SaveLibrary(library, titles);
        RefreshLibrary(path);
        SetStatus("Wallpaper uploaded. Click its card to apply it.", "Обои загружены. Нажмите на их карточку, чтобы применить.");
    }

    private void LibraryCardsSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading || LibraryCards.SelectedItem is not WallpaperCard card) return;
        if (card.IsEmpty)
        {
            WallpaperEngineInterop.StopEngine();
            _settings = _settings with { WallpaperPath = null };
            _settingsStore.Save(_settings);
            SetStatus("Wallpaper stopped.", "Обои отключены.");
            return;
        }
        ApplyWallpaper(card.Path!, true);
    }
    private void ApplyWallpaper(string path, bool save)
    {
        if (!WallpaperEngineInterop.SetWallpaper(path)) { SetStatus($"Could not apply wallpaper: {WallpaperEngineInterop.GetLastErrorMessage()}", $"Не удалось применить обои: {WallpaperEngineInterop.GetLastErrorMessage()}"); return; }
        WallpaperEngineInterop.SetVolume((int)VolumeSlider.Value);
        if (save) { _settings = _settings with { WallpaperPath = path }; _settingsStore.Save(_settings); }
        SetStatus("Wallpaper is playing.", "Обои воспроизводятся.");
    }

    private void CardPreviewEnter(object sender, System.Windows.Input.MouseEventArgs e)
    {
        if (sender is not Border card || card.DataContext is not WallpaperCard item || !item.IsVideo) return;
        var player = FindChild<MediaElement>(card);
        if (player is not null) { player.Position = TimeSpan.Zero; player.Play(); }
    }
    private void CardPreviewLeave(object sender, System.Windows.Input.MouseEventArgs e)
    {
        if (sender is not Border card) return;
        var player = FindChild<MediaElement>(card);
        if (player is not null) { player.Stop(); player.Position = TimeSpan.Zero; }
    }
    private void CardPreviewEnded(object sender, RoutedEventArgs e)
    {
        if (sender is MediaElement player) { player.Position = TimeSpan.Zero; player.Play(); }
    }
    private static TChild? FindChild<TChild>(DependencyObject root) where TChild : DependencyObject
    {
        for (var i = 0; i < VisualTreeHelper.GetChildrenCount(root); i++)
        {
            var child = VisualTreeHelper.GetChild(root, i);
            if (child is TChild match) return match;
            var nested = FindChild<TChild>(child);
            if (nested is not null) return nested;
        }
        return null;
    }

    private List<string> GetLibrary() => (_settings.Library ?? []).Where(File.Exists).Distinct(StringComparer.OrdinalIgnoreCase).ToList();
    private Dictionary<string, string> GetTitles() => new(_settings.LibraryTitles ?? [], StringComparer.OrdinalIgnoreCase);
    private void SaveLibrary(List<string> library, Dictionary<string, string>? titles = null)
    {
        _settings = _settings with { Library = library, LibraryTitles = titles ?? GetTitles() };
        _settingsStore.Save(_settings);
    }
    private void RefreshLibrary(string? selectPath = null)
    {
        var titles = GetTitles();
        var cards = new List<WallpaperCard> { WallpaperCard.Empty(T("No wallpaper", "Без обоев")) };
        cards.AddRange(GetLibrary().Select(path => new WallpaperCard(path, titles.GetValueOrDefault(path, Path.GetFileNameWithoutExtension(path)))));
        LibraryCards.ItemsSource = cards;
        EmptyLibraryText.Visibility = cards.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        var wanted = selectPath ?? _settings.WallpaperPath;
        LibraryCards.SelectedItem = cards.FirstOrDefault(card => string.Equals(card.Path, wanted, StringComparison.OrdinalIgnoreCase)) ?? cards[0];
    }

    private void VolumeChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        VolumeValueText.Text = ((int)e.NewValue).ToString();
        if (_loading) return;
        var volume = (int)e.NewValue;
        if (volume == 0) _muted = true;
        else { _muted = false; _lastVolume = volume; }
        WallpaperEngineInterop.SetMuted(_muted);
        if (WallpaperEngineInterop.SetVolume(volume)) { _settings = _settings with { Volume = volume }; _settingsStore.Save(_settings); }
        ApplyLanguage();
    }
    private void Mute_Click(object sender, RoutedEventArgs e)
    {
        if (!_muted && VolumeSlider.Value > 0) _lastVolume = (int)VolumeSlider.Value;
        VolumeSlider.Value = _muted ? Math.Max(1, _lastVolume) : 0;
    }
    private void OtherAppsMuteChanged(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        var enabled = OtherAppsMuteCheckBox.IsChecked == true;
        if (WallpaperEngineInterop.SetMuteWhenOtherAppOpen(enabled))
        {
            _settings = _settings with { MuteWhenOtherAppOpen = enabled };
            _settingsStore.Save(_settings);
        }
    }
    private void PerformanceChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading) return;
        var mode = PerformanceComboBox.SelectedIndex;
        if (mode < 0) return;
        WallpaperEngineInterop.SetPerformanceMode(mode);
        _settings = _settings with { PerformanceMode = mode };
        _settingsStore.Save(_settings);
        if (!string.IsNullOrWhiteSpace(_settings.WallpaperPath) && File.Exists(_settings.WallpaperPath))
            ApplyWallpaper(_settings.WallpaperPath, false);
    }
    private void AutoStartChanged(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        try { var enabled = AutoStartCheckBox.IsChecked == true; WindowsStartup.SetEnabled(enabled); _settings = _settings with { StartWithWindows = enabled }; _settingsStore.Save(_settings); }
        catch (System.Security.SecurityException) { SetStatus("Windows denied the startup setting.", "Windows отклонила настройку автозапуска."); }
    }
    protected override void OnClosing(CancelEventArgs e) { if (!((App)System.Windows.Application.Current).IsExiting) { e.Cancel = true; Hide(); } base.OnClosing(e); }

    private sealed record WallpaperCard(string? Path, string Title)
    {
        public static WallpaperCard Empty(string title) => new(null, title);
        public bool IsEmpty => Path is null;
        public string FileName => Path is null ? "Windows" : System.IO.Path.GetFileName(Path);
        public string Extension => Path is null ? "OFF" : System.IO.Path.GetExtension(Path).TrimStart('.').ToUpperInvariant();
        public Uri? PreviewUri => Path is null ? null : new Uri(Path, UriKind.Absolute);
        public string PreviewSymbol => Path is null ? "×" : "▶";
        public bool IsVideo => string.Equals(Extension, "MP4", StringComparison.OrdinalIgnoreCase);
    }
}
