using System.IO;
using System.Windows;
using Forms = System.Windows.Forms;

namespace WallpaperClient;

public partial class App : System.Windows.Application
{
    private Forms.NotifyIcon? _trayIcon;
    private System.Drawing.Icon? _trayIconImage;
    internal bool IsExiting { get; private set; }

    protected override void OnStartup(StartupEventArgs e)
    {
        var vlcRuntime = Path.Combine(AppContext.BaseDirectory, "libvlc", "win-x64");
        Environment.SetEnvironmentVariable("OPENWALLPAPER_LIBVLC_DIR", vlcRuntime);
        base.OnStartup(e);
        CreateTrayIcon();
        UpdateChecker.UpdateReady += OnUpdateReady;
    }

    internal void ShowMainWindow()
    {
        var window = MainWindow;
        if (window is null) return;
        window.Show();
        window.WindowState = WindowState.Normal;
        window.Activate();
    }

    private void CreateTrayIcon()
    {
        var menu = new Forms.ContextMenuStrip();
        menu.Items.Add("Open", null, (_, _) => ShowMainWindow());
        menu.Items.Add("Exit", null, (_, _) => ExitApplication());
        var executablePath = Environment.ProcessPath ?? Path.Combine(AppContext.BaseDirectory, "OpenPaper.exe");
        _trayIconImage = System.Drawing.Icon.ExtractAssociatedIcon(executablePath);
        _trayIcon = new Forms.NotifyIcon
        {
            Text = "OpenPaper",
            Icon = _trayIconImage ?? System.Drawing.SystemIcons.Application,
            Visible = true,
            ContextMenuStrip = menu,
        };
        _trayIcon.DoubleClick += (_, _) => ShowMainWindow();
    }

    private void ExitApplication()
    {
        ApplyPendingUpdate();
        if (IsExiting) return;
        IsExiting = true;
        Shutdown();
    }

    /// <summary>Starts the already verified installer and exits only for the update.</summary>
    internal bool ApplyPendingUpdate()
    {
        if (!UpdateChecker.TryLaunchPendingInstaller()) return false;
        IsExiting = true;
        Shutdown();
        return true;
    }

    private void OnUpdateReady(string version)
    {
        _trayIcon?.ShowBalloonTip(
            4_000,
            "OpenPaper",
            $"Обновление {version} загружено и установится при выходе из приложения.",
            Forms.ToolTipIcon.Info);
    }

    protected override void OnExit(ExitEventArgs e)
    {
        _trayIcon?.Dispose();
        _trayIconImage?.Dispose();
        UpdateChecker.UpdateReady -= OnUpdateReady;
        WallpaperEngineInterop.StopEngine();
        base.OnExit(e);
    }
}
