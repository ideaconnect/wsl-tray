// Links the pre-built Windows resource object (exe icon, application manifest
// with per-monitor DPI v2 + common controls v6, version info) into the binary.
// The objects are COFF files with a .rsrc section, one per target machine; the
// MSVC linker treats files with an unknown extension as object files, so no
// rc.exe is needed at build time. Regenerate them with go-winres from
// winres/winres.json (see README).
fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let name = match arch.as_str() {
        "x86_64" => "wsl-tray-amd64.res.obj",
        "aarch64" => "wsl-tray-arm64.res.obj",
        other => {
            println!("cargo:warning=no resource object for target arch {other}; building without icon/manifest");
            return;
        }
    };
    let obj = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("res")
        .join(name);
    println!("cargo:rerun-if-changed={}", obj.display());
    println!("cargo:rustc-link-arg-bins={}", obj.display());
}
