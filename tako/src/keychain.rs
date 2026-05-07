use std::path::Path;

#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
#[cfg(target_os = "macos")]
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;

pub fn unavailable_message() -> &'static str {
    "iCloud Keychain requires the signed Tako app. Reinstall Tako and try again."
}

#[cfg(target_os = "macos")]
pub fn save_key(
    id: &str,
    key: &crate::crypto::EncryptionKey,
    usage_path: Option<&Path>,
) -> Result<(), String> {
    use security_framework::passwords::set_generic_password_options;

    validate_key_id(id)?;

    let options = key_query_options(id);
    set_generic_password_options(key.to_base64().as_bytes(), options)
        .map_err(|e| keychain_error("save key to iCloud Keychain", e))?;
    update_key_metadata(id, usage_path)
        .map_err(|e| keychain_error("update iCloud Keychain key metadata", e))
}

#[cfg(not(target_os = "macos"))]
pub fn save_key(
    _id: &str,
    _key: &crate::crypto::EncryptionKey,
    _usage_path: Option<&Path>,
) -> Result<(), String> {
    Err(unavailable_message().to_string())
}

#[cfg(target_os = "macos")]
pub fn mark_key_used(id: &str, usage_path: &Path) -> Result<(), String> {
    validate_key_id(id)?;
    update_key_metadata(id, Some(usage_path))
        .map_err(|e| keychain_error("update iCloud Keychain key metadata", e))
}

#[cfg(not(target_os = "macos"))]
pub fn mark_key_used(_id: &str, _usage_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn load_key(id: &str) -> Result<Option<crate::crypto::EncryptionKey>, String> {
    use security_framework::passwords::generic_password;

    validate_key_id(id)?;

    match generic_password(key_query_options(id)) {
        Ok(bytes) => {
            let encoded = String::from_utf8(bytes)
                .map_err(|e| format!("Invalid iCloud Keychain key encoding: {e}"))?;
            crate::crypto::EncryptionKey::from_base64(encoded.trim())
                .map(Some)
                .map_err(|e| e.to_string())
        }
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(e) if e.code() == ERR_SEC_MISSING_ENTITLEMENT => Ok(None),
        Err(e) => Err(keychain_error("read key from iCloud Keychain", e)),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn load_key(_id: &str) -> Result<Option<crate::crypto::EncryptionKey>, String> {
    Ok(None)
}

#[cfg(target_os = "macos")]
pub fn delete_key(id: &str) -> Result<(), String> {
    use security_framework::passwords::delete_generic_password_options;

    validate_key_id(id)?;

    match delete_generic_password_options(key_query_options(id)) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
        Err(e) if e.code() == ERR_SEC_MISSING_ENTITLEMENT => Ok(()),
        Err(e) => Err(keychain_error("delete key from iCloud Keychain", e)),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn delete_key(_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn require_export_authentication() -> Result<(), String> {
    use security_framework::passwords::{
        AccessControlOptions, PasswordOptions, generic_password, set_generic_password_options,
    };

    let mut options =
        PasswordOptions::new_generic_password("Tako Secrets Export", "secrets-key-export");
    options.set_access_control_options(AccessControlOptions::USER_PRESENCE);
    options.set_label("Tako secrets key export");

    match generic_password(options) {
        Ok(_) => Ok(()),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => {
            let mut create_options =
                PasswordOptions::new_generic_password("Tako Secrets Export", "secrets-key-export");
            create_options.set_access_control_options(AccessControlOptions::USER_PRESENCE);
            create_options.set_label("Tako secrets key export");
            set_generic_password_options(b"ok", create_options)
                .map_err(|e| keychain_error("prepare export authentication", e))?;
            require_export_authentication()
        }
        Err(e) => Err(keychain_error("authenticate key export", e)),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn require_export_authentication() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn key_query_options(id: &str) -> security_framework::passwords::PasswordOptions {
    use security_framework::passwords::PasswordOptions;

    let mut options = PasswordOptions::new_generic_password("Tako Secrets", id);
    options.use_protected_keychain();
    options.set_access_synchronized(Some(true));
    options
}

#[cfg(target_os = "macos")]
fn update_key_metadata(
    id: &str,
    usage_path: Option<&Path>,
) -> security_framework::base::Result<()> {
    use security_framework::item::{
        CloudSync, ItemClass, ItemSearchOptions, ItemUpdateOptions, update_item,
    };

    let mut search = ItemSearchOptions::new();
    search
        .class(ItemClass::generic_password())
        .service("Tako Secrets")
        .account(id)
        .cloud_sync(CloudSync::MatchSyncYes)
        .ignore_legacy_keychains();

    let mut update = ItemUpdateOptions::new();
    update.set_label(crate::crypto::keychain_label_for_key_id(id));
    if let Some(comment) = keychain_comment_for_usage_path(usage_path) {
        update.set_comment(comment);
    }

    update_item(&search, &update)
}

fn keychain_comment_for_usage_path(usage_path: Option<&Path>) -> Option<String> {
    let path = usage_path?;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Some(format!("Last used at {}", path.display()))
}

#[cfg(target_os = "macos")]
fn validate_key_id(id: &str) -> Result<(), String> {
    if id.len() != 16 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid key id '{id}'. Expected 16 hex characters."
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_error(action: &str, error: security_framework::base::Error) -> String {
    if error.code() == ERR_SEC_MISSING_ENTITLEMENT {
        return unavailable_message().to_string();
    }
    format!("Failed to {action}: {error}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn unavailable_message_mentions_signed_app() {
        assert_eq!(
            super::unavailable_message(),
            "iCloud Keychain requires the signed Tako app. Reinstall Tako and try again."
        );
    }

    #[test]
    fn keychain_comment_for_usage_path_formats_last_used_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = std::fs::canonicalize(temp.path()).unwrap();
        let expected = format!("Last used at {}", path.display());

        assert_eq!(
            super::keychain_comment_for_usage_path(Some(temp.path())).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn keychain_comment_for_usage_path_skips_missing_path() {
        assert_eq!(super::keychain_comment_for_usage_path(None), None);
    }
}
