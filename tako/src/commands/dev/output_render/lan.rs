use super::shared::{RESET, ansi_rgb, muted, split_route_pattern};

/// Render a URL as a terminal-friendly QR code using Unicode block characters.
/// Each row of the QR code uses upper/lower half-block characters to pack two
/// rows of modules into one terminal line, giving a compact square appearance.
fn format_qr_code(url: &str) -> Vec<String> {
    use qrcode::QrCode;

    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(_) => return vec![format!("(QR code generation failed for {url})")],
    };

    let matrix = code.to_colors();
    let width = code.width();
    let height = matrix.len() / width;

    // Two module rows per terminal line via upper/lower half-blocks.
    let mut lines = Vec::new();

    let mut y = 0;
    while y < height {
        let mut line = String::new();
        for x in 0..width {
            let top = matrix[y * width + x];
            let bottom = if y + 1 < height {
                matrix[(y + 1) * width + x]
            } else {
                qrcode::Color::Light
            };
            match (top, bottom) {
                (qrcode::Color::Dark, qrcode::Color::Dark) => line.push('█'),
                (qrcode::Color::Dark, qrcode::Color::Light) => line.push('▀'),
                (qrcode::Color::Light, qrcode::Color::Dark) => line.push('▄'),
                (qrcode::Color::Light, qrcode::Color::Light) => line.push(' '),
            }
        }
        lines.push(line);
        y += 2;
    }

    lines
}

/// Convert a `.test` / `.tako.test` route to its `.local` LAN equivalent.
pub(in crate::commands::dev) fn to_local_route(route: &str) -> Option<String> {
    let (host, path) = split_route_pattern(route);
    let (wildcard, host) = if let Some(rest) = host.strip_prefix("*.") {
        ("*.", rest)
    } else {
        ("", host)
    };
    let base = host
        .strip_suffix(".tako.test")
        .or_else(|| host.strip_suffix(".test"))?;
    Some(match path {
        Some(path) => format!("{wildcard}{base}.local{path}"),
        None => format!("{wildcard}{base}.local"),
    })
}

/// Render a LAN mode block: routes + QR code as a single visual unit.
pub(in crate::commands::dev) fn format_lan_block(hosts: &[String], ca_url: &str) -> Vec<String> {
    let url_color = ansi_rgb(240, 175, 95);
    let warn_color = ansi_rgb(234, 211, 156);
    let mut out = Vec::new();
    out.push(String::new());

    // Wildcard routes cannot be advertised via mDNS (Bonjour/Avahi) — each
    // concrete subdomain needs its own record — so they are excluded from
    // the LAN route list (which would otherwise mislead the user into
    // trying an unreachable URL). Only concrete hostnames are listed.
    let concrete_hosts: Vec<String> = hosts
        .iter()
        .filter(|h| !split_route_pattern(h).0.starts_with("*."))
        .filter_map(|h| to_local_route(h))
        .collect();
    let wildcard_host = hosts
        .iter()
        .map(|h| split_route_pattern(h).0)
        .find(|h| h.starts_with("*.") && to_local_route(h).is_some());

    if concrete_hosts.is_empty() {
        out.push(format!(
            "  {}",
            muted("No routes are reachable on your local network")
        ));
    } else {
        out.push(format!(
            "  {}",
            "Your app is now available on your local network at these routes"
        ));
        out.push(String::new());
        for local in &concrete_hosts {
            out.push(format!("  {url_color}https://{local}{RESET}"));
        }
    }

    // If there are any wildcard routes, explain why they were excluded and
    // suggest a concrete example derived from one of them. `!` is flush-left
    // so the body text column lines up with the URL text column above.
    if let Some(wildcard_host) = wildcard_host {
        let example = wildcard_host.replacen('*', "tenant", 1);
        out.push(String::new());
        out.push(format!(
            "{warn_color}! Wildcard routes can't be advertised to devices via mDNS{RESET}"
        ));
        out.push(format!(
            "  {warn_color}Use non-wildcard routes (e.g. {example}) to reach it from your phone{RESET}"
        ));
    }

    out.push(String::new());
    for line in format_qr_code(ca_url) {
        out.push(format!("  {line}"));
    }
    out.push(format!(
        "  {}",
        "Scan to install the CA certificate on your device"
    ));
    out.push(format!(
        "  {}",
        muted(
            "If the page doesn't load, your Wi-Fi may use client isolation and LAN mode won't work"
        )
    ));
    out.push(String::new());
    out
}
