# OpenWallpaper MVP

Рабочий MVP для Windows x64. Rust DLL управляет WorkerW, playback и паузой; WPF/.NET 8 предоставляет UI. Исходный код проекта распространяется по MIT. Видеодвижок — динамически загружаемый libVLC LGPL-2.1-or-later.

## Сборка и запуск

```powershell
cd WallpaperClient
dotnet run -p:Platform=x64
```

MSBuild собирает Rust Core в `Release`, загружает официальный пакет `VideoLAN.LibVLC.Windows` 3.0.23.1 и копирует native runtime с плагинами в `libvlc/win-x64` рядом с приложением.

## Возможности MVP

- `init_engine` создаёт дочернее окно WorkerW позади иконок рабочего стола.
- `set_wallpaper` безопасно принимает UTF-8 путь и направляет video output libVLC в это окно.
- Для локальных MP4, GIF и WEBP используется pipeline libVLC; запрашивается D3D11 hardware decoding (`--avcodec-hw=d3d11va`).
- Монитор foreground-окна раз в 750 мс определяет fullscreen-приложение и вызывает реальную паузу/возобновление libVLC.
- Ошибки FFI читаются через `get_last_error` и показываются в UI.
- Последний успешный путь и настройка автозапуска сохраняются в `%AppData%\OpenWallpaper\settings.json`.
- Закрытие главного окна сворачивает приложение в системный трей; выход из tray корректно останавливает Rust Core.

Подробнее о лицензионных обязательствах runtime — в [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Производственные доработки

1. Коды ошибок FFI (`get_last_error`) вместо единственного `bool`.
2. Multi-monitor политика, пользовательские исключения и автозапуск.
3. Восстановление после перезапуска Explorer и GPU device reset.
4. Installer, подпись и тесты на матрице GPU/кодеков.
