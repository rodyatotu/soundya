fn main() {
    // libasound is opened at runtime with dlopen; on glibc 2.23 dlopen lives in libdl.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "linux" {
        println!("cargo:rustc-link-lib=dl");
    }
}
