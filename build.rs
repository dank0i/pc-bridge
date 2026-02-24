fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Reduce default stack size from 1 MB → 256 KB (saves ~768 KB RSS)
    // pc-bridge doesn't use deep recursion; 256 KB is generous for our workload.
    if target_os == "windows" {
        // MSVC linker flag
        println!("cargo:rustc-link-arg=/STACK:262144");
    } else if target_os == "linux" {
        // GNU ld / LLD flag
        println!("cargo:rustc-link-arg=-Wl,-z,stacksize=262144");
    }
    // macOS: thread stack set at runtime, not via linker (no-op here)

    // Windows resources (icon, metadata)
    if target_os == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "PC Bridge");
        res.set("FileDescription", "PC Bridge");
        res.set("CompanyName", "PC Bridge");
        res.set("OriginalFilename", "PC Bridge.exe");
        res.compile().expect("Failed to compile Windows resources");
    }
}
