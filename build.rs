fn main() {
    println!("cargo:rerun-if-changed=assets/PTLabel.ico");
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/PTLabel.ico");
        res.set("ProductName", "PTLabel");
        res.set("FileDescription", "PTLabel — Brother P-touch Label Printer");
        if let Err(e) = res.compile() {
            panic!("failed to compile Windows resources: {e}");
        }
    }
}
