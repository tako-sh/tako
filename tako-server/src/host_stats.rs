//! What the machine running the server has and uses, reported by `server_info`.

use std::path::Path;
use tako_core::{CpuStats, DiskStats, HostStats, MemoryStats};

/// Reads uptime, CPU, memory, and the data directory's filesystem. A part the
/// server can't read, such as `/proc` outside Linux, is left out.
pub(crate) fn read(data_dir: &Path) -> HostStats {
    HostStats {
        uptime_secs: read_proc("/proc/uptime").as_deref().and_then(parse_uptime),
        cpu: online_cpus().map(|cores| CpuStats {
            cores,
            load_average: read_proc("/proc/loadavg")
                .as_deref()
                .and_then(parse_loadavg),
        }),
        memory: read_proc("/proc/meminfo")
            .as_deref()
            .and_then(parse_meminfo),
        disk: disk_stats(data_dir),
    }
}

fn read_proc(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// CPUs online on the machine, not only those this process may use, so the
/// count matches the machine-wide load average.
fn online_cpus() -> Option<u32> {
    // SAFETY: sysconf has no preconditions and only reads system configuration.
    let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    u32::try_from(online)
        .ok()
        .filter(|&count| count > 0)
        .or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .and_then(|count| u32::try_from(count.get()).ok())
        })
}

fn disk_stats(path: &Path) -> Option<DiskStats> {
    let stat = rustix::fs::statvfs(path).ok()?;
    let block = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Some(DiskStats {
        path: path.display().to_string(),
        total_bytes: stat.f_blocks.saturating_mul(block),
        used_bytes: stat
            .f_blocks
            .saturating_sub(stat.f_bfree)
            .saturating_mul(block),
        available_bytes: stat.f_bavail.saturating_mul(block),
    })
}

/// `/proc/uptime`: seconds since boot, then idle seconds.
fn parse_uptime(content: &str) -> Option<u64> {
    let secs: f64 = content.split_whitespace().next()?.parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then_some(secs as u64)
}

/// `/proc/loadavg`: the 1, 5, and 15 minute averages lead the line.
fn parse_loadavg(content: &str) -> Option<[f64; 3]> {
    let mut fields = content.split_whitespace();
    let mut next = || -> Option<f64> {
        fields
            .next()?
            .parse()
            .ok()
            .filter(|value: &f64| value.is_finite())
    };
    Some([next()?, next()?, next()?])
}

/// `/proc/meminfo`: `Name:   <value> kB` lines.
fn parse_meminfo(content: &str) -> Option<MemoryStats> {
    let field = |name: &str| {
        content.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix(':')?;
            let mut parts = value.split_whitespace();
            let amount: u64 = parts.next()?.parse().ok()?;
            match parts.next() {
                Some("kB") => Some(amount.saturating_mul(1024)),
                None => Some(amount),
                Some(_) => None,
            }
        })
    };
    Some(MemoryStats {
        total_bytes: field("MemTotal")?,
        available_bytes: field("MemAvailable")?,
        swap_total_bytes: field("SwapTotal").unwrap_or(0),
        swap_free_bytes: field("SwapFree").unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_takes_whole_seconds_since_boot() {
        assert_eq!(parse_uptime("350735.47 234388.90\n"), Some(350_735));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_uptime("-1 0"), None);
        assert_eq!(parse_uptime("NaN 0"), None);
    }

    #[test]
    fn loadavg_takes_the_three_averages() {
        assert_eq!(
            parse_loadavg("0.52 1.58 12.00 1/1013 12345\n"),
            Some([0.52, 1.58, 12.0])
        );
        assert_eq!(parse_loadavg("0.52 1.58"), None);
        assert_eq!(parse_loadavg("0.52 inf 1.0"), None);
    }

    #[test]
    fn meminfo_reads_total_available_and_swap_in_bytes() {
        let content = "\
MemTotal:        8040576 kB
MemFree:          612820 kB
MemAvailable:    5361820 kB
Buffers:          263812 kB
SwapCached:            0 kB
SwapTotal:       2097148 kB
SwapFree:        2097148 kB
";
        assert_eq!(
            parse_meminfo(content),
            Some(MemoryStats {
                total_bytes: 8_040_576 * 1024,
                available_bytes: 5_361_820 * 1024,
                swap_total_bytes: 2_097_148 * 1024,
                swap_free_bytes: 2_097_148 * 1024,
            })
        );
    }

    #[test]
    fn meminfo_without_swap_or_available_memory() {
        let no_swap = "MemTotal: 1024 kB\nMemAvailable: 512 kB\n";
        let stats = parse_meminfo(no_swap).unwrap();
        assert_eq!((stats.swap_total_bytes, stats.swap_free_bytes), (0, 0));

        // MemAvailable is the only honest free figure; without it, report none.
        assert_eq!(parse_meminfo("MemTotal: 1024 kB\nMemFree: 512 kB\n"), None);
    }

    #[test]
    fn disk_stats_describe_the_filesystem_holding_the_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let disk = disk_stats(temp.path()).unwrap();
        assert_eq!(disk.path, temp.path().display().to_string());
        assert!(disk.total_bytes > 0);
        assert!(disk.used_bytes + disk.available_bytes <= disk.total_bytes);
        assert!(disk_stats(&temp.path().join("missing")).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_reports_every_part_on_linux() {
        let temp = tempfile::TempDir::new().unwrap();
        let stats = read(temp.path());
        assert!(stats.uptime_secs.is_some());
        let cpu = stats.cpu.unwrap();
        assert!(cpu.cores > 0);
        assert!(cpu.load_average.is_some());
        let memory = stats.memory.unwrap();
        assert!(memory.available_bytes <= memory.total_bytes);
        assert!(stats.disk.is_some());
    }
}
