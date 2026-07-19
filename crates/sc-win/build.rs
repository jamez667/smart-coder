//! Build script: embed the SC app icon into the Windows executable (taskbar/Explorer).
//! No-op on non-Windows targets.

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/logo/sc.ico");
        // winresource needs the Windows SDK `rc.exe` on PATH; it isn't here, so point it at
        // the SDK bin dir directly (newest installed version that has an x64 rc.exe).
        for sdk in [
            r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64",
            r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.19041.0\x64",
        ] {
            if std::path::Path::new(&format!("{sdk}\\rc.exe")).exists() {
                res.set_toolkit_path(sdk);
                break;
            }
        }
        // Surface a failure as a build warning instead of silently shipping no icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=app icon embed failed: {e}");
        }
    }
}
