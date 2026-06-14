use super::*;

#[test]
fn format_lan_block_lists_concrete_routes_and_preserves_paths() {
    // Wildcard routes are excluded from the LAN route list because mDNS
    // can't advertise them — leaving them in would mislead the user into
    // trying a URL their phone can't resolve.
    let lines = format_lan_block(
        &[
            "bun-example.test".to_string(),
            "bun-example.test/bun".to_string(),
            "*.bun-example.test/api/*".to_string(),
        ],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(!plain.contains("LAN mode enabled"));
    assert!(plain.contains("Your app is now available on your local network at these routes"));
    assert!(plain.contains("https://bun-example.local"));
    assert!(plain.contains("https://bun-example.local/bun"));
    assert!(
        !plain.contains("https://*.bun-example.local"),
        "wildcard route should be excluded from the LAN list, got:\n{plain}"
    );
}

#[test]
fn format_lan_block_keeps_primary_copy_unmuted() {
    let lines = format_lan_block(&["demo.test".to_string()], "http://192.168.1.2/ca.pem");
    let available = lines
        .iter()
        .find(|line| line.contains("Your app is now available on your local network"))
        .expect("available copy missing");
    let scan = lines
        .iter()
        .find(|line| line.contains("Scan to install the CA certificate"))
        .expect("scan copy missing");

    assert!(
        !available.contains("\x1b[2m"),
        "available copy should not be muted: {available:?}"
    );
    assert!(
        !scan.contains("\x1b[2m"),
        "QR scan copy should not be muted: {scan:?}"
    );
}

#[test]
fn format_lan_block_excludes_external_routes() {
    let lines = format_lan_block(
        &["app.test".to_string(), "tunnel.example.com".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(plain.contains("https://app.local"));
    assert!(!plain.contains("tunnel.example.com.local"));
}

#[test]
fn format_lan_block_has_no_reachable_routes_for_external_only_routes() {
    let lines = format_lan_block(
        &["tunnel.example.com".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(plain.contains("No routes are reachable on your local network"));
    assert!(!plain.contains("tunnel.example.com.local"));
}

#[test]
fn format_lan_block_shows_unreachable_message_when_only_wildcards() {
    // When every configured route is a wildcard, there is nothing mDNS can
    // advertise, so the header flips to "no routes are reachable" and the
    // warning explains what to do about it.
    let lines = format_lan_block(&["*.app.test".to_string()], "http://192.168.1.2/ca.pem");
    let plain = strip_ansi(&lines.join("\n"));

    assert!(
        plain.contains("No routes are reachable on your local network"),
        "expected unreachable header, got:\n{plain}"
    );
    assert!(
        !plain.contains("Your app is now available on your local network"),
        "available header should be absent when only wildcards exist, got:\n{plain}"
    );
    assert!(plain.contains("Wildcard routes can't be advertised"));
    assert!(plain.contains("e.g. tenant.app.test"));
}

#[test]
fn format_lan_block_warns_below_route_list_with_derived_example() {
    // Warning sits after the full route list (separated by a blank line)
    // with text "! ..." flush-left so the body column matches the URL
    // column above. The remedy example is derived from the app's own
    // wildcard route so the user can copy-paste it into tako.toml.
    let lines = format_lan_block(
        &[
            "demo.test".to_string(),
            "*.demo.test".to_string(),
            "other.test".to_string(),
        ],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(
        plain.contains("Wildcard routes can't be advertised to devices via mDNS"),
        "expected wildcard warning, got:\n{plain}"
    );
    assert!(
        plain.contains("Use non-wildcard routes"),
        "expected 'Use non-wildcard routes' wording, got:\n{plain}"
    );
    assert!(
        plain.contains("e.g. tenant.demo.test"),
        "expected derived example, got:\n{plain}"
    );

    // The warning must sit after every URL in the route list, not inline
    // between URLs — proving it is emitted once at the bottom of the block.
    let last_url_idx = plain
        .rfind("https://other.local")
        .expect("other URL missing");
    let warning_idx = plain
        .find("Wildcard routes can't be advertised")
        .expect("warning missing");
    assert!(
        warning_idx > last_url_idx,
        "expected warning after the full route list, got:\n{plain}"
    );

    // The warning title line is flush-left (`!` at col 0) so "Wildcard"
    // lines up at col 2, matching the "https://" text column of the
    // routes. Find the warning line and check its prefix.
    let warning_line = plain
        .lines()
        .find(|l| l.contains("Wildcard routes can't be advertised"))
        .expect("warning line missing");
    assert!(
        warning_line.starts_with("! Wildcard"),
        "expected warning line to start with '! Wildcard' (no leading indent), got: {warning_line:?}"
    );
}

#[test]
fn format_lan_block_warning_uses_first_wildcard_for_example() {
    // When multiple wildcards exist, derive the example from the first one
    // and emit only a single warning block.
    let lines = format_lan_block(
        &["*.one.test".to_string(), "*.two.test".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(plain.contains("e.g. tenant.one.test"));
    assert!(!plain.contains("e.g. tenant.two.test"));
    assert_eq!(
        plain.matches("Wildcard routes can't be advertised").count(),
        1,
        "expected exactly one warning block"
    );
}

#[test]
fn format_lan_block_hints_at_client_isolation_below_qr() {
    // The muted hint under the QR code explains the most common silent
    // failure mode (AP client isolation) so users on guest/coffee-shop
    // Wi-Fi know where to look instead of assuming Tako is broken.
    let lines = format_lan_block(&["demo.test".to_string()], "http://192.168.1.2/ca.pem");
    let plain = strip_ansi(&lines.join("\n"));
    assert!(
        plain.contains("your Wi-Fi may use client isolation"),
        "expected client-isolation hint under QR, got:\n{plain}"
    );
}

#[test]
fn format_lan_block_omits_wildcard_warning_when_none_present() {
    let lines = format_lan_block(
        &["demo.test".to_string(), "demo.test/api".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));
    assert!(!plain.contains("Wildcard routes can't be advertised"));
    assert!(!plain.contains("Use non-wildcard routes"));
}
