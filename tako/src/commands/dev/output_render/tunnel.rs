use super::shared::{RESET, ansi_rgb};

pub(in crate::commands::dev) fn format_tunnel_block(url: &str) -> Vec<String> {
    let url_color = ansi_rgb(240, 175, 95);
    vec![
        String::new(),
        "  Your app is now available on the public internet at this URL".to_string(),
        String::new(),
        format!("  {url_color}{url}{RESET}"),
        String::new(),
    ]
}
