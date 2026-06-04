fn main() {
    windows_icon_build::compile_windows_icon_resource(
        "resources/core-server.rc",
        "resources/overlay-core.ico",
        "core-server.res",
    );
}
