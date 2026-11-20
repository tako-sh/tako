use std::fs;
use std::path::Path;

use super::TakoToml;
use crate::config::{ConfigError, Result};

impl TakoToml {
    /// Record a network in the selected config, preserving comments and formatting.
    pub fn record_network(path: &Path, env: &str, id: &str) -> Result<()> {
        crate::config::validate_network_id(id)?;
        // Serialize read/modify/write across commands recording different environments.
        let directory = path.parent().unwrap_or(Path::new(".")).join(".tako");
        fs::create_dir_all(&directory)
            .map_err(|error| ConfigError::FileWrite(directory.clone(), error))?;
        let lock_path = directory.join("config.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|error| ConfigError::FileWrite(lock_path.clone(), error))?;
        lock.lock()
            .map_err(|error| ConfigError::FileWrite(lock_path, error))?;
        let text = fs::read_to_string(path)
            .map_err(|error| ConfigError::FileRead(path.to_path_buf(), error))?;
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| ConfigError::Validation(format!("{}: {error}", path.display())))?;
        let table = doc
            .get_mut("envs")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|envs| envs.get_mut(env))
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or_else(|| {
                ConfigError::Validation(format!(
                    "Environment '{env}' is not configured in {}",
                    path.display()
                ))
            })?;
        let mut value = toml_edit::Value::from(id);
        if let Some(previous) = table.get("network").and_then(toml_edit::Item::as_value) {
            *value.decor_mut() = previous.decor().clone();
        }
        table.insert("network", toml_edit::Item::Value(value));
        let text = doc.to_string();
        Self::parse(&text)?;
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, text)
            .and_then(|()| fs::rename(&temporary, path))
            .map_err(|error| ConfigError::FileWrite(path.to_path_buf(), error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_preserves_comments_and_other_configs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.toml");
        let original = "#:schema https://tako.sh/schemas/tako-toml.json\n# app\nname = 'shop'\n\n[envs.production] # deployment\nroute = 'shop.example.com' # public\n";
        fs::write(&path, original).unwrap();
        fs::write(dir.path().join("tako.toml"), original).unwrap();
        TakoToml::record_network(&path, "production", &"a".repeat(32)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(original), "{text}");
        assert_eq!(
            TakoToml::load_from_file(&path).unwrap().envs["production"]
                .network
                .as_deref(),
            Some("a".repeat(32).as_str())
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("tako.toml")).unwrap(),
            original
        );
        assert!(!dir.path().join(".tako/project.json").exists());
    }

    #[test]
    fn moving_network_preserves_its_comment_and_inline_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tako.toml");
        for original in [
            "[envs.production]\nroute = 'shop.example.com'\nnetwork = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' # fleet\n",
            "envs = { production = { route = 'shop.example.com', network = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' } } # fleet\n",
        ] {
            fs::write(&path, original).unwrap();
            TakoToml::record_network(&path, "production", &"b".repeat(32)).unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                original.replace(
                    "'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
                    "\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\""
                )
            );
        }
    }

    #[test]
    fn concurrent_records_keep_every_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tako.toml");
        let envs: Vec<String> = (0..16).map(|n| format!("env{n}")).collect();
        let text: String = envs
            .iter()
            .map(|env| format!("[envs.{env}]\nroute = '{env}.example.com'\n"))
            .collect();
        fs::write(&path, text).unwrap();
        std::thread::scope(|scope| {
            for env in &envs {
                let path = &path;
                scope.spawn(move || TakoToml::record_network(path, env, &"a".repeat(32)).unwrap());
            }
        });
        let config = TakoToml::load_from_file(&path).unwrap();
        for env in envs {
            assert!(config.envs[&env].network.is_some(), "{env} was dropped");
        }
    }

    #[test]
    fn invalid_edit_leaves_config_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tako.toml");
        let text = "[envs.development]\nroute = 'shop.test'\n";
        fs::write(&path, text).unwrap();
        assert!(TakoToml::record_network(&path, "development", &"a".repeat(32)).is_err());
        assert!(TakoToml::record_network(&path, "production", &"a".repeat(32)).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
}
