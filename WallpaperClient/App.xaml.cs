using System.IO;
using System.Windows;
using Forms = System.Windows.Forms;

namespace WallpaperClient;

public partial class App : System.Windows.Application
{
    private Forms.NotifyIcon? _trayIcon;
    internal bool IsExiting { get; private set; }

    protected override void OnStartup(StartupEventArgs e)
    {
        var vlcRuntime = Path.Combine(AppContext.BaseDirectory, "libvlc", "win-x64");
        Environment.SetEnvironmentVariable("OPENWALLPAPER_LIBVLC_DIR", vlcRuntime);
        base.OnStartup(e);
        CreateTrayIcon();
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
        _trayIcon = new Forms.NotifyIcon
        {
            Text = "OpenPaper",
            Icon = System.Drawing.SystemIcons.Application,
            Visible = true,
            ContextMenuStrip = menu,
        };
        _trayIcon.DoubleClick += (_, _) => ShowMainWindow();
    }

    private void ExitApplication()
    {
        IsExiting = true;
        Shutdown();
    }

    protected override void OnExit(ExitEventArgs e)
    {
        _trayIcon?.Dispose();
        WallpaperEngineInterop.StopEngine();
        base.OnExit(e);
    }
}
