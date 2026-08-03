using System.Collections.ObjectModel;
using System.ComponentModel;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Threading;

namespace WallpaperClient;

public partial class MainWindow : Window
{
    private static readonly SolidColorBrush ActiveAudioBrush = CreateFrozenBrush(20, 20, 20);
    private static readonly SolidColorBrush MutedAudioBrush = CreateFrozenBrush(220, 224, 228);
    private readonly AppSettingsStore _settingsStore = new();
    private readonly ObservableCollection<WallpaperCard> _wallpaperCards = [];
    private readonly DispatcherTimer _volumeSaveTimer = new() { Interval = TimeSpan.FromMilliseconds(350) };
    private readonly DispatcherTimer _previewTimer = new() { Interval = TimeSpan.FromMilliseconds(450) };
    private readonly DispatcherTimer _previewRevealTimer = new() { Interval = TimeSpan.FromMilliseconds(120) };
    private AppSettings _settings = new();
    private bool _loading;
    private bool _muted;
    private bool _syncingVolume;
    private bool _syncingLibrarySelection;
    private bool _volumeSettingsPending;
    private int _lastVolume = 70;
    private string? _availableUpdateVersion;
    private WallpaperCard? _previewCard;
    private WallpaperCard? _applyingCard;
    private string? _previewPath;
    private Border? _previewTarget;
    private long _applyRequestId;

    public MainWindow()
    {
        InitializeComponent();
        LibraryCards.ItemsSource = _wallpaperCards;
        _volumeSaveTimer.Tick += VolumeSaveTimer_Tick;
        _previewTimer.Tick += PreviewTimer_Tick;
        _previewRevealTimer.Tick += PreviewRevealTimer_Tick;
        Loaded += async (_, _) => await UpdateChecker.CheckAsync(this);
        Closed += (_, _) =>
        {
            UpdateChecker.UpdateReady -= UpdateReady;
            StopPreview();
            FlushVolumeSettings();
        };
        UpdateChecker.UpdateReady += UpdateReady;
        LibraryPanel.Visibility = Visibility.Visible;
        SettingsPanel.Visibility = Visibility.Collapsed;
        _loading = true;
        _settings = _settingsStore.Load();
        AutoStartCheckBox.IsChecked = _settings.StartWithWindows;
        OtherAppsMuteCheckBox.IsChecked = _settings.MuteWhenOtherAppOpen;
        StretchToFillCheckBox.IsChecked = _settings.StretchToFill;
        VolumeSlider.Value = Math.Clamp(_settings.Volume, 0, 100);
        _muted = VolumeSlider.Value == 0;
        _lastVolume = _muted ? 70 : (int)VolumeSlider.Value;
        SyncLibrary(_settings.WallpaperPath, updateSelection: true);
        ApplyLanguage();
        _loading = false;

        if (!WallpaperEngineInterop.InitEngine()) { SetStatus($"Engine error: {WallpaperEngineInterop.GetLastErrorMessage()}", $"Ошибка движка: {WallpaperEngineInterop.GetLastErrorMessage()}"); return; }
        WallpaperEngineInterop.SetVolume((int)VolumeSlider.Value);
        WallpaperEngineInterop.SetMuteWhenOtherAppOpen(_settings.MuteWhenOtherAppOpen);
        WallpaperEngineInterop.SetPerformanceMode(_settings.PerformanceMode);
        WallpaperEngineInterop.SetStretchToFill(_settings.StretchToFill);
        if (!string.IsNullOrWhiteSpace(_settings.WallpaperPath) && File.Exists(_settings.WallpaperPath))
            _ = ApplyWallpaperAsync(
                _settings.WallpaperPath!,
                false,
                _wallpaperCards.FirstOrDefault(card => string.Equals(card.Path, _settings.WallpaperPath, StringComparison.OrdinalIgnoreCase)));
        else SetStatus("Engine ready. Upload a wallpaper to start.", "Движок готов. Загрузите обои, чтобы начать.");
    }

    private bool IsRussian => _settings.Language != "en";
    private string T(string en, string ru) => IsRussian ? ru : en;
    private static SolidColorBrush CreateFrozenBrush(byte red, byte green, byte blue)
    {
        var brush = new SolidColorBrush(System.Windows.Media.Color.FromRgb(red, green, blue));
        brush.Freeze();
        return brush;
    }

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
        StretchToFillLabelText.Text = T("Stretch wallpaper to fill the screen", "Растягивать обои на весь экран");
        VersionLabelText.Text = T("Installed version", "Текущая версия");
        VersionValueText.Text = $"v{typeof(MainWindow).Assembly.GetName().Version?.ToString(3) ?? "0.0.0"}";
        UpdateAudioUi();
        UpdateLanguagePill();
        UpdateBannerText.Text = _availableUpdateVersion is null
            ? string.Empty
            : T($"OpenPaper {_availableUpdateVersion} is ready.", $"OpenPaper {_availableUpdateVersion} готово к установке.");
        UpdateNowButton.Content = T("Update now", "Обновить сейчас");
    }

    private void UpdateLanguagePill()
    {
        var active = new SolidColorBrush(System.Windows.Media.Color.FromRgb(20, 20, 20));
        RuButton.Background = IsRussian ? active : System.Windows.Media.Brushes.Transparent;
        RuButton.Foreground = IsRussian ? System.Windows.Media.Brushes.White : System.Windows.Media.Brushes.Black;
        EnButton.Background = IsRussian ? System.Windows.Media.Brushes.Transparent : active;
        EnButton.Foreground = IsRussian ? System.Windows.Media.Brushes.Black : System.Windows.Media.Brushes.White;
        UpdatePerformancePill();
    }

    private void UpdateAudioUi()
    {
        var muted = _muted || VolumeSlider.Value <= 0;
        MuteButton.ToolTip = T(muted ? "Turn sound on" : "Turn sound off", muted ? "Включить звук" : "Выключить звук");
        MuteButton.Background = muted ? MutedAudioBrush : ActiveAudioBrush;
        MuteButton.Foreground = muted ? System.Windows.Media.Brushes.Black : System.Windows.Media.Brushes.White;
        AudioWavePath.Visibility = muted ? Visibility.Collapsed : Visibility.Visible;
        AudioMuteSlashPath.Visibility = muted ? Visibility.Visible : Visibility.Collapsed;
        AudioIconText.Text = muted ? "🔇" : VolumeSlider.Value < 50 ? "🔉" : "🔊";
        VolumeValueText.Text = $"{(int)VolumeSlider.Value}%";
    }

    private void UpdatePerformancePill()
    {
        var active = new SolidColorBrush(System.Windows.Media.Color.FromRgb(20, 20, 20));
        var buttons = new[] { PerformanceEcoButton, PerformanceBalanceButton, PerformanceQualityButton };
        for (var index = 0; index < buttons.Length; index++)
        {
            buttons[index].Background = index == _settings.PerformanceMode ? active : System.Windows.Media.Brushes.Transparent;
            buttons[index].Foreground = index == _settings.PerformanceMode ? System.Windows.Media.Brushes.White : System.Windows.Media.Brushes.Black;
        }
        PerformanceEcoButton.Content = T("Eco", "Экономия");
        PerformanceBalanceButton.Content = T("Balanced", "Баланс");
        PerformanceQualityButton.Content = T("Quality", "Качество");
    }

    private void Settings_Click(object sender, RoutedEventArgs e)
    {
        var showSettings = SettingsPanel.Visibility != Visibility.Visible;
        SettingsPanel.Visibility = showSettings ? Visibility.Visible : Visibility.Collapsed;
        LibraryPanel.Visibility = showSettings ? Visibility.Collapsed : Visibility.Visible;
        ApplyLanguage();
    }

    private void AudioControl_MouseLeftButtonDown(object sender, System.Windows.Input.MouseButtonEventArgs e)
    {
        if (e.OriginalSource is DependencyObject source && IsInside(source, MuteButton)) return;
        if (AudioControl.ActualWidth <= 0) return;
        var ratio = e.GetPosition(AudioControl).X / AudioControl.ActualWidth;
        SetVolume((int)Math.Round(Math.Clamp(ratio, 0, 1) * 100));
        e.Handled = true;
    }

    private static bool IsInside(DependencyObject child, DependencyObject ancestor)
    {
        for (DependencyObject? current = child; current is not null; current = VisualTreeHelper.GetParent(current))
            if (ReferenceEquals(current, ancestor)) return true;
        return false;
    }

    private void UpdateReady(string version)
    {
        _availableUpdateVersion = version;
        UpdateBanner.Visibility = Visibility.Visible;
        ApplyLanguage();
    }

    private void UpdateNow_Click(object sender, RoutedEventArgs e)
    {
        if (((App)System.Windows.Application.Current).ApplyPendingUpdate()) return;
        SetStatus("The update is not ready yet. Please try again shortly.", "Обновление ещё не готово. Попробуйте через несколько секунд.");
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
    private void ChangeLanguage(string language)
    {
        _settings = _settings with { Language = language };
        _settingsStore.Save(_settings);
        if (_wallpaperCards.Count > 0 && _wallpaperCards[0].IsEmpty)
            _wallpaperCards[0].UpdateTitle(T("No wallpaper", "Без обоев"));
        ApplyLanguage();
        SetStatus("Language updated.", "Язык обновлён.");
    }

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
        SyncLibrary(null, updateSelection: false);
        SetStatus("Wallpaper uploaded. Click its card to apply it.", "Обои загружены. Нажмите на их карточку, чтобы применить.");
    }

    private async void LibraryCardsSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading || _syncingLibrarySelection || LibraryCards.SelectedItem is not WallpaperCard card) return;
        if (card.IsEmpty)
        {
            var requestId = Interlocked.Increment(ref _applyRequestId);
            SetApplyingCard(card);
            SetStatus("Applying black background...", "Установка чёрного фона...");
            (bool Applied, string Error) result;
            try
            {
                result = await Task.Run(() =>
                {
                    var applied = WallpaperEngineInterop.SetBlackDesktop();
                    return (applied, applied ? string.Empty : WallpaperEngineInterop.GetLastErrorMessage());
                });
            }
            catch (Exception error)
            {
                result = (false, error.Message);
            }
            if (requestId != Volatile.Read(ref _applyRequestId)) return;
            SetApplyingCard(null);
            if (!result.Applied)
            {
                SetStatus($"Could not set black background: {result.Error}", $"Не удалось установить чёрный фон: {result.Error}");
                return;
            }
            _settings = _settings with { WallpaperPath = null };
            _settingsStore.Save(_settings);
            SetStatus("Black background applied.", "Установлен чёрный фон.");
            return;
        }
        await ApplyWallpaperAsync(card.Path!, true, card);
    }

    private async Task ApplyWallpaperAsync(string path, bool save, WallpaperCard? card = null)
    {
        var requestId = Interlocked.Increment(ref _applyRequestId);
        SetApplyingCard(card);
        SetStatus("Applying wallpaper...", "Запуск обоев...");
        (bool Applied, string Error) result;
        try
        {
            result = await Task.Run(() =>
            {
                var applied = WallpaperEngineInterop.SetWallpaper(path);
                return (applied, applied ? string.Empty : WallpaperEngineInterop.GetLastErrorMessage());
            });
        }
        catch (Exception error)
        {
            result = (false, error.Message);
        }
        if (requestId != Volatile.Read(ref _applyRequestId)) return;
        SetApplyingCard(null);
        if (!result.Applied)
        {
            SetStatus($"Could not apply wallpaper: {result.Error}", $"Не удалось применить обои: {result.Error}");
            return;
        }
        WallpaperEngineInterop.SetVolume((int)VolumeSlider.Value);
        if (save) { _settings = _settings with { WallpaperPath = path }; _settingsStore.Save(_settings); }
        SetStatus("Wallpaper is playing.", "Обои воспроизводятся.");
    }

    private void SetApplyingCard(WallpaperCard? card)
    {
        if (ReferenceEquals(_applyingCard, card)) return;
        _applyingCard?.SetApplying(false);
        _applyingCard = card;
        _applyingCard?.SetApplying(true);
    }

    private void CardPreviewEnter(object sender, System.Windows.Input.MouseEventArgs e)
    {
        if (sender is not Border card || card.DataContext is not WallpaperCard item || !item.IsVideo) return;
        StopPreview();
        _previewCard = item;
        _previewPath = item.Path;
        _previewTarget = card;
        PreviewHost.Visibility = Visibility.Collapsed;
        PreviewPlayer.Source = new Uri(item.Path!, UriKind.Absolute);
        PreviewPlayer.Position = TimeSpan.Zero;
        PreviewPlayer.Play();
    }

    private void CardPreviewMediaOpened(object sender, RoutedEventArgs e)
    {
        if (_previewTarget is null || _previewPath is null) return;
        _previewRevealTimer.Stop();
        _previewRevealTimer.Start();
        if (_previewCard?.ThumbnailUri is null && _previewPath is not null)
        {
            _previewTimer.Stop();
            _previewTimer.Start();
        }
    }

    private void PreviewRevealTimer_Tick(object? sender, EventArgs e)
    {
        _previewRevealTimer.Stop();
        var target = _previewTarget;
        if (target is null || _previewPath is null) return;
        var position = target.TranslatePoint(new System.Windows.Point(0, 0), PreviewOverlay);
        Canvas.SetLeft(PreviewHost, position.X);
        Canvas.SetTop(PreviewHost, position.Y);
        PreviewHost.Visibility = Visibility.Visible;
    }

    private void PreviewTimer_Tick(object? sender, EventArgs e)
    {
        _previewTimer.Stop();
        var card = _previewCard;
        var path = _previewPath;
        if (_previewTarget is null || card is null || path is null || card.Path != path) return;
        if (PreviewPlayer.ActualWidth > 0 && PreviewPlayer.ActualHeight > 0 && ThumbnailCache.Save(PreviewPlayer, path))
            card.RefreshThumbnail();
    }

    private void CardPreviewLeave(object sender, System.Windows.Input.MouseEventArgs e)
    {
        if (sender is Border card && ReferenceEquals(_previewTarget, card)) StopPreview();
    }

    private void CardPreviewEnded(object sender, RoutedEventArgs e)
    {
        if (_previewTarget is null) return;
        PreviewPlayer.Position = TimeSpan.Zero;
        PreviewPlayer.Play();
    }

    private void StopPreview()
    {
        _previewTimer.Stop();
        _previewRevealTimer.Stop();
        PreviewPlayer.Stop();
        PreviewPlayer.Source = null;
        PreviewHost.Visibility = Visibility.Collapsed;
        _previewCard = null;
        _previewPath = null;
        _previewTarget = null;
    }

    private List<string> GetLibrary() => (_settings.Library ?? []).Where(File.Exists).Distinct(StringComparer.OrdinalIgnoreCase).ToList();
    private Dictionary<string, string> GetTitles() => new(_settings.LibraryTitles ?? [], StringComparer.OrdinalIgnoreCase);
    private void SaveLibrary(List<string> library, Dictionary<string, string>? titles = null)
    {
        _settings = _settings with { Library = library, LibraryTitles = titles ?? GetTitles() };
        _settingsStore.Save(_settings);
    }
    private void SyncLibrary(string? selectPath, bool updateSelection)
    {
        var titles = GetTitles();
        var paths = GetLibrary();

        if (_wallpaperCards.Count == 0 || !_wallpaperCards[0].IsEmpty)
            _wallpaperCards.Insert(0, WallpaperCard.Empty(T("No wallpaper", "Без обоев")));
        else
            _wallpaperCards[0].UpdateTitle(T("No wallpaper", "Без обоев"));

        var wantedPaths = new HashSet<string>(paths, StringComparer.OrdinalIgnoreCase);
        for (var index = _wallpaperCards.Count - 1; index >= 1; index--)
        {
            var path = _wallpaperCards[index].Path;
            if (path is null || !wantedPaths.Contains(path)) _wallpaperCards.RemoveAt(index);
        }

        var cardsByPath = _wallpaperCards
            .Skip(1)
            .Where(card => card.Path is not null)
            .ToDictionary(card => card.Path!, StringComparer.OrdinalIgnoreCase);
        foreach (var path in paths)
        {
            var title = titles.GetValueOrDefault(path, Path.GetFileNameWithoutExtension(path));
            if (!cardsByPath.TryGetValue(path, out var existing))
                _wallpaperCards.Add(new WallpaperCard(path, title));
            else
                existing.UpdateTitle(title);
        }

        EmptyLibraryText.Visibility = paths.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        if (!updateSelection) return;

        _syncingLibrarySelection = true;
        try
        {
            LibraryCards.SelectedItem = _wallpaperCards.FirstOrDefault(card =>
                string.Equals(card.Path, selectPath, StringComparison.OrdinalIgnoreCase)) ?? _wallpaperCards[0];
        }
        finally
        {
            _syncingLibrarySelection = false;
        }
    }

    private void VolumeChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        VolumeValueText.Text = $"{(int)e.NewValue}%";
        if (_loading || _syncingVolume) return;
        var volume = (int)e.NewValue;
        if (volume == 0) _muted = true;
        else { _muted = false; _lastVolume = volume; }
        WallpaperEngineInterop.SetMuted(_muted);
        if (WallpaperEngineInterop.SetVolume(volume))
        {
            _settings = _settings with { Volume = volume };
            ScheduleVolumeSettingsSave();
        }
        UpdateAudioUi();
    }

    private void ScheduleVolumeSettingsSave()
    {
        _volumeSettingsPending = true;
        _volumeSaveTimer.Stop();
        _volumeSaveTimer.Start();
    }

    private void VolumeSaveTimer_Tick(object? sender, EventArgs e) => FlushVolumeSettings();

    private void FlushVolumeSettings()
    {
        _volumeSaveTimer.Stop();
        if (!_volumeSettingsPending) return;
        _volumeSettingsPending = false;
        _settingsStore.Save(_settings);
    }
    private void Mute_Click(object sender, RoutedEventArgs e)
    {
        if (!_muted && VolumeSlider.Value > 0) _lastVolume = (int)VolumeSlider.Value;
        SetVolume(_muted ? Math.Max(1, _lastVolume) : 0);
    }
    private void SetVolume(int volume)
    {
        _syncingVolume = true;
        VolumeSlider.Value = Math.Clamp(volume, 0, 100);
        _syncingVolume = false;
        VolumeChanged(this, new RoutedPropertyChangedEventArgs<double>(0, VolumeSlider.Value));
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
    private void PerformanceMode_Click(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        if (sender is not System.Windows.Controls.Button { Tag: string tag } || !int.TryParse(tag, out var mode)) return;
        WallpaperEngineInterop.SetPerformanceMode(mode);
        _settings = _settings with { PerformanceMode = mode };
        _settingsStore.Save(_settings);
        UpdatePerformancePill();
        if (!string.IsNullOrWhiteSpace(_settings.WallpaperPath) && File.Exists(_settings.WallpaperPath))
            _ = ApplyWallpaperAsync(_settings.WallpaperPath, false, LibraryCards.SelectedItem as WallpaperCard);
    }
    private void StretchToFillChanged(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        var enabled = StretchToFillCheckBox.IsChecked == true;
        if (!WallpaperEngineInterop.SetStretchToFill(enabled)) return;
        _settings = _settings with { StretchToFill = enabled };
        _settingsStore.Save(_settings);
        if (!string.IsNullOrWhiteSpace(_settings.WallpaperPath) && File.Exists(_settings.WallpaperPath))
            _ = ApplyWallpaperAsync(_settings.WallpaperPath, false, LibraryCards.SelectedItem as WallpaperCard);
    }
    private void AutoStartChanged(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        try { var enabled = AutoStartCheckBox.IsChecked == true; WindowsStartup.SetEnabled(enabled); _settings = _settings with { StartWithWindows = enabled }; _settingsStore.Save(_settings); }
        catch (System.Security.SecurityException) { SetStatus("Windows denied the startup setting.", "Windows отклонила настройку автозапуска."); }
    }
    protected override void OnClosing(CancelEventArgs e)
    {
        FlushVolumeSettings();
        StopPreview();
        if (!((App)System.Windows.Application.Current).IsExiting)
        {
            e.Cancel = true;
            Hide();
        }
        base.OnClosing(e);
    }

    private sealed class WallpaperCard : INotifyPropertyChanged
    {
        private string _title;
        private Uri? _thumbnailUri;
        private bool _isApplying;

        public WallpaperCard(string? path, string title)
        {
            Path = path;
            _title = title;
            _thumbnailUri = ThumbnailCache.Get(path);
        }

        public event PropertyChangedEventHandler? PropertyChanged;
        public string? Path { get; }
        public string Title => _isApplying ? $"••• {_title}" : _title;
        public static WallpaperCard Empty(string title) => new(null, title);
        public bool IsEmpty => Path is null;
        public string FileName => Path is null ? "Windows" : System.IO.Path.GetFileName(Path);
        public string Extension => Path is null ? "OFF" : System.IO.Path.GetExtension(Path).TrimStart('.').ToUpperInvariant();
        public Uri? ThumbnailUri => _thumbnailUri;
        public string PreviewSymbol => Path is null ? "×" : "▶";
        public bool IsVideo => string.Equals(Extension, "MP4", StringComparison.OrdinalIgnoreCase);
        public bool IsApplying => _isApplying;

        public void SetApplying(bool value)
        {
            if (_isApplying == value) return;
            _isApplying = value;
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(IsApplying)));
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Title)));
        }

        public void UpdateTitle(string title)
        {
            if (_title == title) return;
            _title = title;
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Title)));
        }

        public void RefreshThumbnail()
        {
            var thumbnail = ThumbnailCache.Get(Path);
            if (Equals(_thumbnailUri, thumbnail)) return;
            _thumbnailUri = thumbnail;
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(ThumbnailUri)));
        }
    }
}
