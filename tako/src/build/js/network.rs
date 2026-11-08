use super::*;

#[derive(serde::Deserialize)]
pub(super) struct GeneratedNetworkTypes {
    pub declarations: String,
    pub contracts: String,
}

pub(super) fn generate(
    project: &Path,
    app_root: &str,
    declarations: &Path,
    command: &impl Fn() -> Command,
) -> std::io::Result<Option<GeneratedNetworkTypes>> {
    let root = js_app_root_dir(project, app_root);
    let declaration_dir = declarations.parent().unwrap_or(project);
    if !root.join("workflows").is_dir()
        && !root.join("channels").is_dir()
        && !root.join("network-handlers").is_dir()
        && !declaration_dir.join("tako.imports.ts").exists()
        && !declaration_dir.join("tako.exports.d.ts").exists()
    {
        return Ok(None);
    }
    let Some(app) = TakoToml::load_from_dir(project)
        .ok()
        .and_then(|config| config.name)
    else {
        return Ok(None);
    };
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("sdk/javascript/bin/gen-network-types.ts");
    let mut generator = command();
    if source.is_file() {
        generator.arg(source);
    } else {
        generator.arg("run").arg("tako-sh-gen-network-types");
    }
    let output = generator
        .arg(&root)
        .arg(declaration_dir)
        .arg(app)
        .current_dir(project)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "network type generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map(Some)
        .map_err(std::io::Error::other)
}
