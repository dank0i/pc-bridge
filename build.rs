fn main() {
    // Check target OS, not host OS (important for cross-compilation)
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "PC Bridge");
        res.set("FileDescription", "PC Bridge");
        res.set("CompanyName", "dank0i");
        res.set("OriginalFilename", "PC Bridge.exe");
        res.compile().expect("Failed to compile Windows resources");
    }
}
