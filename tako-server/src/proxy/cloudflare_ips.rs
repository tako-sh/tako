use ipnet::IpNet;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CLOUDFLARE_IPS_URL: &str = "https://api.cloudflare.com/client/v4/ips";
const CLOUDFLARE_IP_CACHE_VERSION: u8 = 1;

const STATIC_CLOUDFLARE_CIDRS: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

#[derive(Debug, Clone)]
pub(crate) struct CloudflareIpRanges {
    cidrs: Arc<RwLock<Vec<IpNet>>>,
}

impl CloudflareIpRanges {
    pub(crate) fn contains(&self, ip: &IpAddr) -> bool {
        self.cidrs.read().iter().any(|cidr| cidr.contains(ip))
    }

    pub(crate) fn count(&self) -> usize {
        self.cidrs.read().len()
    }

    pub(crate) fn load_cache_file(&self, path: &Path) -> Result<(), String> {
        let cache = CloudflareIpCache::read_from_path(path)?;
        self.replace_cidrs(cache.to_ipnets()?)?;
        Ok(())
    }

    pub(crate) fn from_static() -> Self {
        Self::from_cidr_strings(STATIC_CLOUDFLARE_CIDRS)
            .expect("static Cloudflare CIDRs should parse")
    }

    #[cfg(test)]
    pub(crate) fn from_test_cidrs(cidrs: &[&str]) -> Self {
        Self::from_cidr_strings(cidrs).expect("test Cloudflare CIDRs should parse")
    }

    pub(crate) async fn refresh_from_api(&self) -> Result<CloudflareIpCache, String> {
        let cache = CloudflareIpCache::fetch().await?;
        self.replace_cidrs(cache.to_ipnets()?)?;
        Ok(cache)
    }

    fn replace_cidrs(&self, cidrs: Vec<IpNet>) -> Result<(), String> {
        if cidrs.is_empty() {
            return Err("Cloudflare IP ranges cannot be empty".to_string());
        }
        *self.cidrs.write() = cidrs;
        Ok(())
    }

    fn from_cidr_strings(cidrs: &[&str]) -> Result<Self, String> {
        Self::from_owned_cidr_strings(cidrs.iter().map(|cidr| cidr.to_string()).collect())
    }

    fn from_owned_cidr_strings(cidrs: Vec<String>) -> Result<Self, String> {
        let cidrs = parse_cidr_strings(cidrs)?;
        if cidrs.is_empty() {
            return Err("Cloudflare IP ranges cannot be empty".to_string());
        }
        Ok(Self {
            cidrs: Arc::new(RwLock::new(cidrs)),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct CloudflareIpCache {
    version: u8,
    fetched_at_unix_secs: u64,
    #[serde(default)]
    etag: Option<String>,
    ipv4_cidrs: Vec<String>,
    ipv6_cidrs: Vec<String>,
}

impl CloudflareIpCache {
    pub(crate) fn write_to_path(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("Invalid Cloudflare IP cache path '{}'", path.display()))?;
        let tmp_path = path.with_file_name(format!(".{filename}.tmp.{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp_path, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn read_from_path(path: &Path) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let cache: Self = serde_json::from_str(&contents).map_err(|e| e.to_string())?;
        cache.validate()?;
        Ok(cache)
    }

    async fn fetch() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?;
        let response = client
            .get(CLOUDFLARE_IPS_URL)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let body = response.text().await.map_err(|e| e.to_string())?;
        let parsed: CloudflareIpsResponse =
            serde_json::from_str(&body).map_err(|e| e.to_string())?;
        if !parsed.success {
            return Err("Cloudflare IPs API returned success=false".to_string());
        }

        let cache = Self {
            version: CLOUDFLARE_IP_CACHE_VERSION,
            fetched_at_unix_secs: now_unix_secs(),
            etag: parsed.result.etag,
            ipv4_cidrs: parsed.result.ipv4_cidrs,
            ipv6_cidrs: parsed.result.ipv6_cidrs,
        };
        cache.validate()?;
        Ok(cache)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != CLOUDFLARE_IP_CACHE_VERSION {
            return Err(format!(
                "Unsupported Cloudflare IP cache version {}",
                self.version
            ));
        }
        let cidrs = self.cidr_strings();
        if cidrs.is_empty() {
            return Err("Cloudflare IP cache contains no CIDRs".to_string());
        }
        parse_cidr_strings(cidrs)?;
        Ok(())
    }

    fn to_ipnets(&self) -> Result<Vec<IpNet>, String> {
        parse_cidr_strings(self.cidr_strings())
    }

    fn cidr_strings(&self) -> Vec<String> {
        let mut cidrs = Vec::with_capacity(self.ipv4_cidrs.len() + self.ipv6_cidrs.len());
        cidrs.extend(self.ipv4_cidrs.iter().cloned());
        cidrs.extend(self.ipv6_cidrs.iter().cloned());
        cidrs
    }
}

fn parse_cidr_strings(cidrs: Vec<String>) -> Result<Vec<IpNet>, String> {
    cidrs
        .into_iter()
        .map(|cidr| {
            cidr.parse::<IpNet>()
                .map_err(|e| format!("Invalid Cloudflare CIDR '{cidr}': {e}"))
        })
        .collect::<Result<Vec<_>, _>>()
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

impl Default for CloudflareIpRanges {
    fn default() -> Self {
        Self::from_static()
    }
}

#[derive(Deserialize)]
struct CloudflareIpsResponse {
    success: bool,
    result: CloudflareIpsResult,
}

#[derive(Deserialize)]
struct CloudflareIpsResult {
    #[serde(default)]
    ipv4_cidrs: Vec<String>,
    #[serde(default)]
    ipv6_cidrs: Vec<String>,
    #[serde(default)]
    etag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_ranges_include_cloudflare_ipv4_and_ipv6() {
        let ranges = CloudflareIpRanges::from_static();

        assert!(ranges.contains(&"173.245.48.1".parse().unwrap()));
        assert!(ranges.contains(&"2400:cb00::1".parse().unwrap()));
        assert!(!ranges.contains(&"203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn cache_file_replaces_static_ranges() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cloudflare-ips.json");
        let cache = CloudflareIpCache {
            version: CLOUDFLARE_IP_CACHE_VERSION,
            fetched_at_unix_secs: 123,
            etag: Some("test-etag".to_string()),
            ipv4_cidrs: vec!["198.51.100.0/24".to_string()],
            ipv6_cidrs: vec!["2001:db8::/32".to_string()],
        };
        cache.write_to_path(&cache_path).unwrap();

        let ranges = CloudflareIpRanges::from_static();
        ranges.load_cache_file(&cache_path).unwrap();

        assert!(ranges.contains(&"198.51.100.10".parse().unwrap()));
        assert!(ranges.contains(&"2001:db8::1".parse().unwrap()));
        assert!(!ranges.contains(&"173.245.48.1".parse().unwrap()));
    }

    #[test]
    fn invalid_cache_file_is_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cloudflare-ips.json");
        std::fs::write(
            &cache_path,
            r#"{"version":1,"fetched_at_unix_secs":123,"ipv4_cidrs":["not-a-cidr"],"ipv6_cidrs":[]}"#,
        )
        .unwrap();

        let ranges = CloudflareIpRanges::from_static();
        let error = ranges.load_cache_file(&cache_path).unwrap_err();

        assert!(error.contains("Invalid Cloudflare CIDR"), "{error}");
        assert!(ranges.contains(&"173.245.48.1".parse().unwrap()));
    }
}
