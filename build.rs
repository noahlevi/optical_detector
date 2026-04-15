fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        return;
    }

    // Argus + EGLStream headers (Jetson Multimedia API)
    // Common locations on L4T r35.x:
    //   /usr/src/jetson_multimedia_api/argus/include
    //   /usr/include/Argus  (if libargus-dev is installed)
    let argus_include = std::env::var("ARGUS_INCLUDE")
        .unwrap_or_else(|_| "/usr/src/jetson_multimedia_api/argus/include".into());

    cc::Build::new()
        .cpp(true)
        .flag("-std=c++11")
        .flag("-Wno-deprecated-declarations")
        .include(&argus_include)
        .include("/usr/include")         // nvbufsurface.h
        .file("src/argus_wrapper.cpp")
        .compile("argus_wrapper");

    // libargus.so  (Argus runtime)
    println!("cargo:rustc-link-lib=argus");

    // NvBufSurface (buffer mapping helper)
    println!("cargo:rustc-link-lib=nvbufsurface");

    // Tegra libs live here on aarch64 L4T
    println!("cargo:rustc-link-search=/usr/lib/aarch64-linux-gnu/tegra");

    println!("cargo:rerun-if-changed=src/argus_wrapper.cpp");
    println!("cargo:rerun-if-changed=src/argus_wrapper.h");
}
