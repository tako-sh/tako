#[cfg(target_os = "macos")]
fn main() {
    for path in ["/opt/homebrew/lib", "/usr/local/lib"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
