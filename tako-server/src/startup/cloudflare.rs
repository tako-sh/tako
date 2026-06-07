use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::proxy;

const CLOUDFLARE_IP_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn should_skip_cloudflare_ip_refresh() -> bool {
    std::env::var_os("TAKO_TEST_SKIP_CLOUDFLARE_IP_REFRESH").is_some()
}

pub(super) fn spawn_cloudflare_ip_refresh(
    rt: &tokio::runtime::Runtime,
    cloudflare_ips: proxy::CloudflareIpRanges,
    cache_path: PathBuf,
    routes: Arc<parking_lot::RwLock<crate::routing::RouteTable>>,
) {
    rt.spawn(async move {
        loop {
            tokio::time::sleep(CLOUDFLARE_IP_REFRESH_INTERVAL).await;
            if !routes.read().needs_cloudflare_ip_ranges() {
                continue;
            }
            refresh_cloudflare_ip_ranges_once(&cloudflare_ips, &cache_path).await;
        }
    });
}

async fn refresh_cloudflare_ip_ranges_once(
    cloudflare_ips: &proxy::CloudflareIpRanges,
    cache_path: &Path,
) {
    match cloudflare_ips.refresh_from_api().await {
        Ok(cache) => {
            if let Err(error) = cache.write_to_path(cache_path) {
                tracing::warn!(
                    path = %cache_path.display(),
                    "Failed to write Cloudflare IP range cache: {error}"
                );
            }
            tracing::info!(
                cidrs = cloudflare_ips.count(),
                "Refreshed Cloudflare IP ranges"
            );
        }
        Err(error) => {
            tracing::warn!("Cloudflare IP range refresh skipped: {error}");
        }
    }
}
