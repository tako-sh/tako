//! Portable application schemas and native source generation.
mod golang;
mod model;
mod native;
mod resolve;
mod rust;
mod types;
mod typescript;

pub use model::{Manifest, Primitive};
pub use native::{generate_native, native_root};
pub use resolve::{Resolved, resolve};
use std::{fs, io, path::Path};

pub fn write_generated(path: &Path, content: &str) -> io::Result<bool> {
    if fs::read_to_string(path).is_ok_and(|old| old == content) {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| model::invalid("invalid generated path"))?;
    fs::create_dir_all(parent)?;
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(content.as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(true)
}

pub fn typescript(manifests: &[Manifest]) -> io::Result<String> {
    typescript::generate(manifests)
}
pub fn golang(manifests: &[Manifest], package: &str) -> io::Result<String> {
    golang::generate(manifests, package)
}
pub fn rust(manifests: &[Manifest]) -> io::Result<String> {
    rust::generate(manifests)
}

#[cfg(test)]
mod tests;
