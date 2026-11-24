//! Local CA setup for development HTTPS.

use std::path::{Path, PathBuf};

use sha2::Digest;

use crate::dev::{LocalCA, LocalCAStore};
use crate::output;

// ── CA setup ─────────────────────────────────────────────────────────────────

fn sudo_action_line() -> &'static str {
    "Enable HTTPS"
}

fn trust_install_required(has_usable_ca: bool, ca_trusted: bool) -> bool {
    if has_usable_ca {
        !ca_trusted
    } else {
        // If the current CA cannot be loaded, `tako dev` will regenerate it.
        // That new root always needs a trust-store install.
        true
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn pending_sudo_action() -> Result<Option<&'static str>, Box<dyn std::error::Error>> {
    let store = LocalCAStore::new()?;
    let has_usable_ca = store.load_ca().is_ok();
    let install_trust = trust_install_required(has_usable_ca, store.is_ca_trusted());
    Ok(install_trust.then_some(sudo_action_line()))
}

/// Setup the local CA for development.
///
/// 1. Load existing or generate new Root CA
/// 2. Install trust in the system store if needed (requires sudo)
pub async fn setup_local_ca() -> Result<LocalCA, Box<dyn std::error::Error>> {
    let store = LocalCAStore::new()?;
    let existing_ca = store.load_ca().ok();
    let install_trust = trust_install_required(existing_ca.is_some(), store.is_ca_trusted());

    if install_trust && !output::is_interactive() && !output::is_root() {
        return Err(
            "local CA is not trusted; run `tako dev` interactively once to install it, or use `tako dev --tunnel` for a public preview without sudo".into(),
        );
    }

    let ca = match existing_ca {
        Some(ca) => {
            let _t = output::timed("Load existing Tako CA from store");
            ca
        }
        None => {
            let ca = {
                let _t = output::timed("Generate new Tako CA");
                LocalCA::generate().map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?
            };
            {
                let _t = output::timed("Save Tako CA to secure storage");
                store
                    .save_ca(&ca)
                    .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
            }
            ca
        }
    };

    if install_trust {
        let _t = output::timed("Install Tako CA in system trust store");
        store
            .install_ca_trust()
            .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
    }

    Ok(ca)
}

// ── CA fingerprint ───────────────────────────────────────────────────────────

const DEV_TLS_CA_FINGERPRINT_FILENAME: &str = "ca_fingerprint";

pub(crate) fn ca_fingerprint(ca: &LocalCA) -> String {
    hex::encode(sha2::Sha256::digest(ca.ca_cert_pem().as_bytes()))
}

pub(crate) fn ca_fingerprint_path_for_home(home: &Path) -> PathBuf {
    home.join("certs").join(DEV_TLS_CA_FINGERPRINT_FILENAME)
}

/// Record the CA the dev server should sign with. Returns `true` when it
/// differs from the recorded one, meaning a running daemon holds a stale CA.
///
/// The daemon signs leaf certificates on demand for any SNI, so adding an app
/// never requires a restart; only a CA change does.
pub(crate) fn record_dev_ca_fingerprint_for_home(
    ca: &LocalCA,
    home: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let certs_dir = home.join("certs");
    let fp_path = ca_fingerprint_path_for_home(home);
    let fingerprint = ca_fingerprint(ca);
    if std::fs::read_to_string(&fp_path).is_ok_and(|stored| stored.trim() == fingerprint) {
        return Ok(false);
    }
    std::fs::create_dir_all(&certs_dir)?;
    std::fs::write(&fp_path, fingerprint)?;
    Ok(true)
}

pub(crate) fn record_dev_ca_fingerprint(ca: &LocalCA) -> Result<bool, Box<dyn std::error::Error>> {
    let data_dir = crate::paths::tako_data_dir()?;
    record_dev_ca_fingerprint_for_home(ca, &data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_install_not_required_for_usable_trusted_ca() {
        assert!(!trust_install_required(true, true));
    }

    #[test]
    fn trust_install_required_for_usable_but_untrusted_ca() {
        assert!(trust_install_required(true, false));
    }

    #[test]
    fn trust_install_required_when_ca_will_be_regenerated() {
        assert!(trust_install_required(false, false));
        assert!(trust_install_required(false, true));
    }

    #[test]
    fn sudo_action_line_mentions_https() {
        let line = sudo_action_line();
        assert!(line.contains("HTTPS"));
    }
}
