#[cfg(not(target_os = "macos"))]
const LIBFUSE_NAME: &str = "fuse";

#[cfg(target_os = "macos")]
const LIBFUSE_NAME: &str = "osxfuse";

fn main() {
    /*
    pkg_config::Config::new()
        .atleast_version("2.6.0")
        .probe(LIBFUSE_NAME)
        .map_err(|e| eprintln!("{}", e))
        .unwrap();
    */
    println!("cargo:rustc-link-search=native=-L../../../../libfuse/build/lib");
    println!("cargo:rustc-link-lib=fuse");
}
