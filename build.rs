fn main() {
    println!("cargo:rerun-if-env-changed=OMNI_CEF_DIR");
    let dir = std::env::var("OMNI_CEF_DIR").unwrap_or_else(|_| "/usr/lib64/cef".into());
    assert!(
        std::path::Path::new(&dir).join("libcef.so").is_file(),
        "Chromium Embedded Framework is required: install cef, or set OMNI_CEF_DIR"
    );
    println!("cargo:rustc-env=OMNI_CEF_DIR={dir}");
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=dylib=cef");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
}
