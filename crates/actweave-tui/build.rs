fn main() {
    println!("cargo:rerun-if-changed=res/icon.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("res/icon.ico");
        resource.compile().expect("cannot embed Windows icon");
    }
}
