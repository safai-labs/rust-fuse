#[cfg(not(target_os = "macos"))]
const LIBFUSE_NAME: &str = "fuse";

#[cfg(target_os = "macos")]
const LIBFUSE_NAME: &str = "osxfuse";

fn main() {
    let libfuse_dir = std::env::var_os("CARGO_MANIFEST_DIR")
                            .unwrap();
    let libfuse_dir = std::path::PathBuf::from(libfuse_dir)
                                .join(&"../../libfuse/build/lib");
   
    println!(
        "cargo:rerun-if-changed={}/libfuse3",
        libfuse_dir.to_string_lossy()
    );
    println!(
        "cargo:rustc-link-search=native=-L{}",
        libfuse_dir.to_string_lossy()
    );
    println!("cargo:rustc-link-lib=fuse3");
    println!("cargo:rustc-link-lib={}", LIBFUSE_NAME);
}
