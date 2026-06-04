fn main() {
    windows_icon_build::compile_windows_icon_resource(
        "resources/desktop-window-monitor.rc",
        "resources/overlay-desktop-monitor.ico",
        "desktop-window-monitor.res",
    );
}
