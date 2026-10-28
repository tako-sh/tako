use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let mut assets = Vec::new();
    for (directory, kind) in [
        ("website/docs", "docs"),
        ("sdk/javascript/skills", "skills"),
    ] {
        let directory = root.join(directory).canonicalize().unwrap();
        println!("cargo:rerun-if-changed={}", directory.display());
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let source = if kind == "skills" {
                path.join("SKILL.md")
            } else {
                path.clone()
            };
            if !source.is_file()
                || source
                    .extension()
                    .is_none_or(|extension| extension != "md" && extension != "mdx")
            {
                continue;
            }
            let name = path.file_stem().unwrap().to_str().unwrap();
            assets.push(format!(
                "({:?}, include_str!({:?}))",
                format!("{kind}/{name}"),
                source
            ));
        }
    }
    assets.sort();
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("agent_assets.rs");
    fs::write(
        output,
        format!(
            "pub const ASSETS: &[(&str, &str)] = &[{}];",
            assets.join(",\n")
        ),
    )
    .unwrap();
}
