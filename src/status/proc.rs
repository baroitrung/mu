//! /proc and mountinfo parsers ported from `internal/status/proc.go`.
//!
//! Go's `ReadDisk`/`ReadNetwork` return partial results together with a joined
//! error (`result, errors.Join(errs...)`). Rust has no dual return, so each
//! has a `*_full` variant returning `(values, Option<Error>)` for callers that
//! need the Go semantics; the `Result`-returning wrappers keep the original
//! signatures and yield `Err` (dropping the partial results) when the join is
//! non-empty.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::{Error, Result};

use super::model::{
    BatteryInfo, CPUSample, DiskIORate, DiskIOSample, DiskStat, HardwareInfo, MemStats, NetRate,
    NetStat, ProcessInfo,
};

/// Go `os.Open` surfaces `*PathError` — `open /proc/stat: no such file or
/// directory`. Rust's io::Error Display is `No such file or directory
/// (os error 2)`: strip the os-error suffix and lowercase the first char to
/// reproduce Go's errno text (Go's table is lowercase strerror).
fn open_err(path: &str, e: std::io::Error) -> Error {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text);
    let mut chars = text.chars();
    let lowered = match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    Error::Msg(format!("open {path}: {lowered}"))
}

/// ReadCPU reads the first "cpu" line from /proc/stat.
pub fn read_cpu() -> Result<CPUSample> {
    let f = File::open("/proc/stat").map_err(|e| open_err("/proc/stat", e))?;
    read_cpu_from(BufReader::new(f))
}

fn read_cpu_from(reader: impl BufRead) -> Result<CPUSample> {
    for line in reader.lines() {
        // Go never checks scanner.Err() here: a scan error ends the loop and
        // falls through to "no cpu line found".
        let Ok(line) = line else { break };
        if !line.starts_with("cpu ") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // fields[0] = "cpu", fields[1..] = user nice system idle iowait irq
        // softirq steal guest guest_nice
        if fields.len() < 9 {
            // Go's %q on a string ≈ Rust's {:?}: both produce a double-quoted,
            // escaped literal (identical for the ASCII content expected here).
            return Err(Error::Msg(format!(
                "unexpected /proc/stat format: {line:?}"
            )));
        }
        let mut values = [0u64; 8];
        for (i, v) in values.iter_mut().enumerate() {
            *v = fields[i + 1]
                .parse::<u64>()
                .map_err(|e| Error::Msg(format!("parse /proc/stat field {}: {e}", i + 1)))?;
        }
        return Ok(CPUSample {
            user: values[0],
            nice: values[1],
            system: values[2],
            idle: values[3],
            iowait: values[4],
            irq: values[5],
            softirq: values[6],
            steal: values[7],
        });
    }
    Err(Error::Msg("no cpu line found in /proc/stat".to_string()))
}

/// ReadCPUCores parses per-core jiffies (`cpu0`, `cpu1`, ...) from /proc/stat.
pub fn read_cpu_cores() -> Result<Vec<CPUSample>> {
    let f = File::open("/proc/stat").map_err(|e| open_err("/proc/stat", e))?;
    read_cpu_cores_from(BufReader::new(f))
}

pub fn read_cpu_cores_from(reader: impl BufRead) -> Result<Vec<CPUSample>> {
    let mut cores = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !line.starts_with("cpu") {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 4 || !bytes[3].is_ascii_digit() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 9 {
            return Err(Error::Msg(format!(
                "unexpected /proc/stat format: {line:?}"
            )));
        }
        let mut values = [0u64; 8];
        for (i, v) in values.iter_mut().enumerate() {
            *v = fields[i + 1]
                .parse::<u64>()
                .map_err(|e| Error::Msg(format!("parse /proc/stat field {}: {e}", i + 1)))?;
        }
        cores.push(CPUSample {
            user: values[0],
            nice: values[1],
            system: values[2],
            idle: values[3],
            iowait: values[4],
            irq: values[5],
            softirq: values[6],
            steal: values[7],
        });
    }
    Ok(cores)
}

/// CPUCoresPercent computes per-core usage % between two samples.
pub fn cpu_cores_percent(prev: &[CPUSample], curr: &[CPUSample]) -> Vec<f64> {
    let n = prev.len().min(curr.len());
    let mut res = Vec::with_capacity(n);
    for i in 0..n {
        res.push(cpu_percent(prev[i], curr[i]));
    }
    res
}

/// CPUPercent computes CPU usage % between two samples.
/// Returns 0 if delta_total == 0 or if prev is zero-value (first sample).
pub fn cpu_percent(prev: CPUSample, curr: CPUSample) -> f64 {
    // Go sums with uint64 wrapping arithmetic.
    let total = |s: CPUSample| {
        s.user
            .wrapping_add(s.nice)
            .wrapping_add(s.system)
            .wrapping_add(s.idle)
            .wrapping_add(s.iowait)
            .wrapping_add(s.irq)
            .wrapping_add(s.softirq)
            .wrapping_add(s.steal)
    };
    let prev_total = total(prev);
    let curr_total = total(curr);

    // Zero prevTotal means this is the first sample; no delta available.
    if prev_total == 0 {
        return 0.0;
    }
    if curr_total <= prev_total {
        return 0.0;
    }
    let total_delta = (curr_total - prev_total) as f64;

    let prev_idle = prev.idle.wrapping_add(prev.iowait);
    let curr_idle = curr.idle.wrapping_add(curr.iowait);
    if curr_idle < prev_idle {
        return 0.0;
    }
    let idle_delta = (curr_idle - prev_idle) as f64;

    let percent = (1.0 - idle_delta / total_delta) * 100.0;
    if percent < 0.0 {
        return 0.0;
    }
    if percent > 100.0 {
        return 100.0;
    }
    percent
}

/// ReadMemory parses /proc/meminfo.
pub fn read_memory() -> Result<MemStats> {
    let f = File::open("/proc/meminfo").map_err(|e| open_err("/proc/meminfo", e))?;
    read_memory_from(BufReader::new(f))
}

fn read_memory_from(reader: impl BufRead) -> Result<MemStats> {
    let mut m = MemStats::default();
    for line in reader.lines() {
        // Go checks scanner.Err() after the loop and returns it, discarding
        // the partial stats — propagating here has the same effect.
        let line = line?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        // Go parses fields[1] for every line but only reports the parse error
        // for the keys it switches on; parsing inside each arm is equivalent.
        match fields[0] {
            "MemTotal:" => m.total_kb = parse_kb_field(fields[1], "MemTotal")?,
            "MemAvailable:" => m.available_kb = parse_kb_field(fields[1], "MemAvailable")?,
            "SwapTotal:" => m.swap_total_kb = parse_kb_field(fields[1], "SwapTotal")?,
            "SwapFree:" => m.swap_free_kb = parse_kb_field(fields[1], "SwapFree")?,
            _ => {}
        }
    }
    if m.total_kb == 0 || m.available_kb > m.total_kb {
        return Err(Error::Msg(
            "required memory metrics unavailable or invalid".to_string(),
        ));
    }
    Ok(m)
}

/// Mirrors `fmt.Errorf("parse %s: %w", name, parseErr)` for meminfo values.
fn parse_kb_field(value: &str, name: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|e| Error::Msg(format!("parse {name}: {e}")))
}

/// Filesystems skipped by ReadDisk — mirrors Go's `skipFSTypes` map.
const SKIP_FS_TYPES: &[&str] = &[
    "tmpfs",
    "proc",
    "sysfs",
    "devtmpfs",
    "cgroup",
    "cgroup2",
    "debugfs",
    "squashfs",
    "devpts",
    "hugetlbfs",
    "mqueue",
    "pstore",
    "securityfs",
    "fusectl",
    "binfmt_misc",
    "efivarfs",
    "tracefs",
    "configfs",
    "autofs",
    "rpc_pipefs",
    "nsfs",
    "fuse.portal",
    "bpf",
    "fuse.gvfsd-fuse",
];

/// mountEntry mirrors Go's `mountEntry`.
struct MountEntry {
    mount: String,
    fstype: String,
}

/// The fields of `syscall.Statfs_t` the Go code reads, plus the dedup key.
struct StatfsInfo {
    blocks: u64,
    bsize: i64,
    bavail: u64,
    /// Same-filesystem dedup key (see [`read_disk_full`]).
    dev: u64,
}

/// ReadDisk parses mountinfo and calls statfs on each real filesystem,
/// deduplicating by filesystem identity.
///
/// Thin wrapper over [`read_disk_full`]: returns `Ok` only when no scan
/// errors occurred; on error the partial entries are dropped, so callers that
/// need Go's `disks, err := ReadDisk()` semantics should use
/// `read_disk_full` instead.
#[allow(dead_code)] // kept for the stub's public API; callers use read_disk_full
pub fn read_disk() -> Result<Vec<DiskStat>> {
    let (disks, err) = read_disk_full();
    match err {
        Some(e) => Err(e),
        None => Ok(disks),
    }
}

/// Go-faithful dual return of `ReadDisk`: `(entries, joined scan errors)`,
/// equivalent to Go's `return result, errors.Join(scanErrors...)`.
pub fn read_disk_full() -> (Vec<DiskStat>, Option<Error>) {
    let f = match File::open("/proc/self/mountinfo") {
        Ok(f) => f,
        Err(e) => return (Vec::new(), Some(open_err("/proc/self/mountinfo", e))),
    };
    let mut statfs = |mount: &str| -> std::result::Result<StatfsInfo, String> {
        let stat = nix::sys::statfs::statfs(mount).map_err(|e| e.to_string())?;
        // Go dedups on statfs Fsid's int32 pair. nix exposes f_fsid only as an
        // opaque libc::fsid_t (its __val field is private), so use the mount
        // dir's st_dev instead — the same same-filesystem identity for dedup.
        let dev = std::fs::metadata(mount).map_err(|e| e.to_string())?.dev();
        Ok(StatfsInfo {
            blocks: stat.blocks(),
            // block_size() is i64 on glibc and u64 on musl — try_into is a
            // no-op on the former (hence the allow) and a checked
            // conversion on the latter.
            #[allow(clippy::useless_conversion)]
            bsize: stat.block_size().try_into().unwrap_or_default(),
            bavail: stat.blocks_available(),
            dev,
        })
    };
    read_disk_from(BufReader::new(f), &mut statfs)
}

/// readDiskFrom — the statfs call is injectable for tests, exactly like Go's
/// `readDiskFrom(reader, statfs)`.
fn read_disk_from<F>(reader: impl BufRead, statfs_fn: &mut F) -> (Vec<DiskStat>, Option<Error>)
where
    F: FnMut(&str) -> std::result::Result<StatfsInfo, String>,
{
    let mut entries = match parse_mount_info(reader) {
        Ok(entries) => entries,
        Err(e) => return (Vec::new(), Some(e)),
    };
    // Go's sort.SliceStable: "/" first, then by mount path length ascending.
    entries.sort_by(|a, b| {
        if a.mount == "/" {
            return Ordering::Less;
        }
        if b.mount == "/" {
            return Ordering::Greater;
        }
        a.mount.len().cmp(&b.mount.len())
    });

    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut scan_errors: Vec<String> = Vec::new();
    for entry in &entries {
        if SKIP_FS_TYPES.contains(&entry.fstype.as_str()) {
            continue;
        }
        let stat = match statfs_fn(&entry.mount) {
            Ok(stat) => stat,
            Err(e) => {
                scan_errors.push(format!("statfs {}: {e}", entry.mount));
                continue;
            }
        };
        // Go checks `seen` before the zero-size guards and only marks the key
        // once an entry qualifies — keep that order.
        if seen.contains(&stat.dev) {
            continue;
        }
        if stat.blocks == 0 || stat.bsize <= 0 {
            continue;
        }
        seen.insert(stat.dev);
        result.push(DiskStat {
            mount: entry.mount.clone(),
            total_bytes: stat.blocks.wrapping_mul(stat.bsize as u64),
            free_bytes: stat.bavail.wrapping_mul(stat.bsize as u64),
        });
    }
    (result, join_errors(scan_errors))
}

/// parseMountInfo — cuts each mountinfo line on " - "; left field 4 is the
/// mount point (octal-escaped), right field 0 is the fstype. Lines that fail
/// the cut or have too few fields are skipped.
fn parse_mount_info(reader: impl BufRead) -> Result<Vec<MountEntry>> {
    let mut entries = Vec::new();
    for line in reader.lines() {
        // Go returns `entries, scanner.Err()`; the caller discards the partial
        // entries on error, which propagating here reproduces.
        let line = line?;
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        let right_fields: Vec<&str> = right.split_whitespace().collect();
        if left_fields.len() < 5 || right_fields.is_empty() {
            continue;
        }
        entries.push(MountEntry {
            mount: unescape_mount_path(left_fields[4]),
            fstype: right_fields[0].to_string(),
        });
    }
    Ok(entries)
}

/// unescapeMountPath — Go's strings.NewReplacer performs a single
/// left-to-right pass; a non-match consumes only the backslash so an escape
/// starting inside the lookahead still applies (e.g. `\\040` → `\ `), and
/// replacement output is never re-scanned (e.g. `\134040` → `\040`).
fn unescape_mount_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let seq: String = chars.clone().take(3).collect();
            let replacement = match seq.as_str() {
                "040" => Some(' '),
                "011" => Some('\t'),
                "012" => Some('\n'),
                "134" => Some('\\'),
                _ => None,
            };
            match replacement {
                Some(r) => {
                    out.push(r);
                    for _ in 0..3 {
                        chars.next();
                    }
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Joins collected error messages like Go's `errors.Join(errs...)`: "\n"
/// separated, nil (None) when empty.
fn join_errors(errs: Vec<String>) -> Option<Error> {
    if errs.is_empty() {
        None
    } else {
        Some(Error::Msg(errs.join("\n")))
    }
}

/// ReadNetwork parses /proc/net/dev. Skips "lo".
/// Same dual-return caveat as [`read_disk`]; see [`read_network_full`].
#[allow(dead_code)] // kept for the stub's public API; callers use read_network_full
pub fn read_network() -> Result<BTreeMap<String, NetStat>> {
    let (nets, err) = read_network_full();
    match err {
        Some(e) => Err(e),
        None => Ok(nets),
    }
}

/// Go-faithful dual return of `ReadNetwork`: `(map, joined parse errors)`.
pub fn read_network_full() -> (BTreeMap<String, NetStat>, Option<Error>) {
    let f = match File::open("/proc/net/dev") {
        Ok(f) => f,
        Err(e) => return (BTreeMap::new(), Some(open_err("/proc/net/dev", e))),
    };
    read_network_from(BufReader::new(f))
}

fn read_network_from(reader: impl BufRead) -> (BTreeMap<String, NetStat>, Option<Error>) {
    let mut result = BTreeMap::new();
    let mut parse_errors: Vec<String> = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        // Go: a scan error ends the loop and scanner.Err() joins parseErrors.
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                parse_errors.push(e.to_string());
                break;
            }
        };
        // Skip the two header lines.
        if idx < 2 {
            continue;
        }
        // Format: "  eth0: N N N N N N N N N N N N N N N N"
        let Some(colon) = line.find(':') else {
            continue;
        };
        let iface = line[..colon].trim();
        if iface == "lo" {
            continue;
        }
        let rest = line[colon + 1..].trim();
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 10 {
            parse_errors.push(format!("unexpected /proc/net/dev format for {iface}"));
            continue;
        }
        match (fields[0].parse::<u64>(), fields[8].parse::<u64>()) {
            (Ok(rx_bytes), Ok(tx_bytes)) => {
                result.insert(iface.to_string(), NetStat { rx_bytes, tx_bytes });
            }
            (rx, tx) => {
                // errors.Join(rxErr, txErr) — join whichever failed, in order.
                let mut joined = Vec::new();
                if let Err(e) = rx {
                    joined.push(e.to_string());
                }
                if let Err(e) = tx {
                    joined.push(e.to_string());
                }
                parse_errors.push(format!(
                    "parse network counters for {iface}: {}",
                    joined.join("\n")
                ));
            }
        }
    }
    (result, join_errors(parse_errors))
}

/// NetworkRates computes per-second rates from two NetStat snapshots.
/// Returns 0 for interfaces with no previous reading.
pub fn network_rates(
    prev: &BTreeMap<String, NetStat>,
    curr: &BTreeMap<String, NetStat>,
    elapsed_sec: f64,
) -> BTreeMap<String, NetRate> {
    let elapsed_sec = if elapsed_sec <= 0.0 { 1.0 } else { elapsed_sec };
    let mut rates = BTreeMap::new();
    for (iface, c) in curr {
        let Some(p) = prev.get(iface) else {
            rates.insert(iface.clone(), NetRate::default());
            continue;
        };
        // Counter resets read as 0 — Go's guarded `c >= p` subtraction.
        let rx_delta = c.rx_bytes.saturating_sub(p.rx_bytes);
        let tx_delta = c.tx_bytes.saturating_sub(p.tx_bytes);
        rates.insert(
            iface.clone(),
            NetRate {
                // Go: uint64(float64(delta)/elapsedSec) — truncating cast.
                rx_bytes_per_sec: (rx_delta as f64 / elapsed_sec) as u64,
                tx_bytes_per_sec: (tx_delta as f64 / elapsed_sec) as u64,
            },
        );
    }
    rates
}

/// ReadDiskIO parses /proc/diskstats for disk I/O metrics.
#[allow(dead_code)]
pub fn read_disk_io() -> Result<BTreeMap<String, DiskIOSample>> {
    let (samples, err) = read_disk_io_full();
    if let Some(err) = err {
        return Err(err);
    }
    Ok(samples)
}

/// ReadDiskIOFull returns (samples, joined scan errors).
pub fn read_disk_io_full() -> (BTreeMap<String, DiskIOSample>, Option<Error>) {
    match File::open("/proc/diskstats") {
        Ok(f) => read_disk_io_from(BufReader::new(f)),
        Err(e) => (BTreeMap::new(), Some(open_err("/proc/diskstats", e))),
    }
}

pub fn read_disk_io_from(reader: impl BufRead) -> (BTreeMap<String, DiskIOSample>, Option<Error>) {
    let mut samples = BTreeMap::new();
    let mut scan_errors = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 {
            continue;
        }
        let dev_name = fields[2];
        if dev_name.starts_with("loop")
            || dev_name.starts_with("ram")
            || dev_name.starts_with("zram")
            || dev_name.starts_with("dm-")
            || Path::new("/sys/class/block")
                .join(dev_name)
                .join("partition")
                .exists()
        {
            continue;
        }
        let reads = match fields[3].parse::<u64>() {
            Ok(v) => v,
            Err(e) => {
                scan_errors.push(format!("parse /proc/diskstats {dev_name} reads: {e}"));
                continue;
            }
        };
        let sectors_read = match fields[5].parse::<u64>() {
            Ok(v) => v,
            Err(e) => {
                scan_errors.push(format!(
                    "parse /proc/diskstats {dev_name} sectors_read: {e}"
                ));
                continue;
            }
        };
        let writes = match fields[7].parse::<u64>() {
            Ok(v) => v,
            Err(e) => {
                scan_errors.push(format!("parse /proc/diskstats {dev_name} writes: {e}"));
                continue;
            }
        };
        let sectors_written = match fields[9].parse::<u64>() {
            Ok(v) => v,
            Err(e) => {
                scan_errors.push(format!(
                    "parse /proc/diskstats {dev_name} sectors_written: {e}"
                ));
                continue;
            }
        };
        samples.insert(
            dev_name.to_string(),
            DiskIOSample {
                reads,
                sectors_read,
                writes,
                sectors_written,
            },
        );
    }
    (samples, join_errors(scan_errors))
}

/// DiskIORates computes per-second byte and IO throughput from two snapshots.
pub fn disk_io_rates(
    prev: &BTreeMap<String, DiskIOSample>,
    curr: &BTreeMap<String, DiskIOSample>,
    elapsed_sec: f64,
) -> BTreeMap<String, DiskIORate> {
    let elapsed = if elapsed_sec <= 0.0 { 1.0 } else { elapsed_sec };
    let mut rates = BTreeMap::new();
    for (dev, c) in curr {
        if let Some(p) = prev.get(dev) {
            let read_sectors = c.sectors_read.saturating_sub(p.sectors_read);
            let write_sectors = c.sectors_written.saturating_sub(p.sectors_written);
            let read_ops = c.reads.saturating_sub(p.reads);
            let write_ops = c.writes.saturating_sub(p.writes);
            let read_bytes = read_sectors * 512;
            let write_bytes = write_sectors * 512;
            rates.insert(
                dev.clone(),
                DiskIORate {
                    read_bytes_per_sec: (read_bytes as f64 / elapsed).round() as u64,
                    write_bytes_per_sec: (write_bytes as f64 / elapsed).round() as u64,
                    read_iops: Some((read_ops as f64 / elapsed).round() as u64),
                    write_iops: Some((write_ops as f64 / elapsed).round() as u64),
                },
            );
        }
    }
    rates
}

/// ReadHardware gathers system and CPU specs from /proc/cpuinfo and /sys/class/dmi.
pub fn read_hardware() -> HardwareInfo {
    let cpuinfo = File::open("/proc/cpuinfo").ok();
    let dmi_prod = std::fs::read_to_string("/sys/class/dmi/id/product_name").ok();
    let dmi_vendor = std::fs::read_to_string("/sys/class/dmi/id/sys_vendor").ok();
    read_hardware_from(
        cpuinfo.map(BufReader::new),
        dmi_prod.as_deref(),
        dmi_vendor.as_deref(),
    )
}

pub fn read_hardware_from(
    cpuinfo_reader: Option<impl BufRead>,
    dmi_product: Option<&str>,
    dmi_vendor: Option<&str>,
) -> HardwareInfo {
    let mut cpu_model = None;
    let mut cpu_cores = 0;
    if let Some(reader) = cpuinfo_reader {
        for line in reader.lines().map_while(std::result::Result::ok) {
            if line.starts_with("model name") && cpu_model.is_none() {
                if let Some((_, val)) = line.split_once(':') {
                    cpu_model = Some(val.trim().to_string());
                }
            } else if line.starts_with("processor") {
                cpu_cores += 1;
            }
        }
    }
    let model = match (dmi_vendor.map(str::trim), dmi_product.map(str::trim)) {
        (Some(v), Some(p)) if !v.is_empty() && !p.is_empty() && !p.starts_with(v) => {
            Some(format!("{v} {p}"))
        }
        (_, Some(p)) if !p.is_empty() => Some(p.to_string()),
        (Some(v), _) if !v.is_empty() => Some(v.to_string()),
        _ => None,
    };
    HardwareInfo {
        model,
        cpu_model,
        cpu_cores: if cpu_cores > 0 { Some(cpu_cores) } else { None },
    }
}

/// ReadBattery parses battery charge, health, and status from /sys/class/power_supply/BAT*.
pub fn read_battery() -> Result<Option<BatteryInfo>> {
    read_battery_from(Path::new("/sys/class/power_supply"))
}

pub fn read_battery_from(ps_dir: &Path) -> Result<Option<BatteryInfo>> {
    if !ps_dir.exists() {
        return Ok(None);
    }
    let entries = match std::fs::read_dir(ps_dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("BAT") || name_str.starts_with("bat") {
            let dir = entry.path();
            let cap_str = std::fs::read_to_string(dir.join("capacity")).unwrap_or_default();
            let percentage = cap_str.trim().parse::<u8>().unwrap_or(0);
            let status_str = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
            let status = status_str.trim().to_string();

            let health_percent = {
                let full = std::fs::read_to_string(dir.join("energy_full"))
                    .or_else(|_| std::fs::read_to_string(dir.join("charge_full")))
                    .ok()
                    .and_then(|s| s.trim().parse::<f64>().ok());
                let design = std::fs::read_to_string(dir.join("energy_full_design"))
                    .or_else(|_| std::fs::read_to_string(dir.join("charge_full_design")))
                    .ok()
                    .and_then(|s| s.trim().parse::<f64>().ok());
                match (full, design) {
                    (Some(f), Some(d)) if d > 0.0 => Some(((f / d) * 100.0).clamp(0.0, 100.0)),
                    _ => None,
                }
            };

            return Ok(Some(BatteryInfo {
                percentage,
                status: if status.is_empty() {
                    "Unknown".to_string()
                } else {
                    status
                },
                health_percent,
            }));
        }
    }
    Ok(None)
}

/// ReadTopProcesses reads top resource-consuming processes from /proc/[pid]/stat.
pub fn read_top_processes() -> Result<Vec<ProcessInfo>> {
    read_top_processes_from(Path::new("/proc"), 5)
}

pub fn read_top_processes_from(proc_dir: &Path, limit: usize) -> Result<Vec<ProcessInfo>> {
    if !proc_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(proc_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let uptime_secs = std::fs::read_to_string(proc_dir.join("uptime"))
        .or_else(|_| std::fs::read_to_string("/proc/uptime"))
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    let clk_tck = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .map(|v| v as f64)
        .unwrap_or(100.0);
    let page_size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .map(|v| v as u64)
        .unwrap_or(4096);

    let mut procs = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        let stat_path = entry.path().join("stat");
        let Ok(stat_content) = std::fs::read_to_string(stat_path) else {
            continue;
        };
        let Some(open_paren) = stat_content.find('(') else {
            continue;
        };
        let Some(close_paren) = stat_content.rfind(')') else {
            continue;
        };
        let proc_name = &stat_content[open_paren + 1..close_paren];
        let remainder = &stat_content[close_paren + 1..];
        let fields: Vec<&str> = remainder.split_whitespace().collect();
        if fields.len() > 21 {
            let utime = fields[11].parse::<u64>().unwrap_or(0);
            let stime = fields[12].parse::<u64>().unwrap_or(0);
            let starttime = fields[19].parse::<u64>().unwrap_or(0);
            let rss_pages = fields[21].parse::<u64>().unwrap_or(0);
            let memory_bytes = rss_pages * page_size;

            let cpu_percent = if uptime_secs > 0.0 {
                let start_secs = starttime as f64 / clk_tck;
                let proc_age_secs = uptime_secs - start_secs;
                if proc_age_secs > 0.05 {
                    let cpu_secs = (utime + stime) as f64 / clk_tck;
                    let pct = (cpu_secs / proc_age_secs) * 100.0;
                    (pct * 10.0).round() / 10.0
                } else {
                    0.0
                }
            } else {
                0.0
            };

            procs.push(ProcessInfo {
                pid,
                name: proc_name.to_string(),
                cpu_percent,
                memory_bytes,
            });
        }
    }
    procs.sort_by(|a, b| {
        b.memory_bytes
            .cmp(&a.memory_bytes)
            .then_with(|| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.pid.cmp(&b.pid))
    });
    procs.truncate(limit);
    Ok(procs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- CPU (ports of the TestReadCPU_*/TestCPUPercent* Go tests) --

    #[test]
    fn read_cpu_parses_fixture() {
        let sample = read_cpu_from(include_str!("testdata/proc_stat.txt").as_bytes()).unwrap();
        assert_eq!(
            sample,
            CPUSample {
                user: 1234567,
                nice: 12345,
                system: 345678,
                idle: 89012345,
                iowait: 12345,
                irq: 1234,
                softirq: 12345,
                steal: 0,
            }
        );
    }

    #[test]
    fn read_cpu_error_messages_match_go() {
        // Per-CPU lines start with "cpu0"/"cpu1" — not the "cpu " prefix.
        let err = read_cpu_from(b"cpu0 1 2 3 4 5 6 7 8\n".as_slice()).unwrap_err();
        assert_eq!(err.to_string(), "no cpu line found in /proc/stat");

        let err = read_cpu_from(b"cpu  1 2\n".as_slice()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unexpected /proc/stat format: \"cpu  1 2\""
        );

        let err = read_cpu_from(b"cpu  1 x 3 4 5 6 7 8\n".as_slice()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse /proc/stat field 2: invalid digit found in string"
        );
    }

    // Go: TestReadCPU_ParsesFixture — despite the name it only exercises
    // CPUPercent.
    #[test]
    fn cpu_percent_computes_expected_usage() {
        let s1 = CPUSample {
            user: 1000,
            nice: 0,
            system: 100,
            idle: 9000,
            iowait: 50,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        let s2 = CPUSample {
            user: 1100,
            nice: 0,
            system: 150,
            idle: 9200,
            iowait: 50,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        // total_delta = 10500 - 10150 = 350; idle_delta = 9250 - 9050 = 200;
        // usage = (1 - 200/350) * 100 ≈ 42.857
        let pct = cpu_percent(s1, s2);
        assert!(pct > 42.0 && pct < 44.0, "expected ~42.9%, got {pct:.2}%");
    }

    // Go: TestCPUPercentCounterResetClampsToZero.
    #[test]
    fn cpu_percent_counter_reset_clamps_to_zero() {
        let prev = CPUSample {
            user: 100,
            idle: 900,
            ..CPUSample::default()
        };
        let curr = CPUSample {
            user: 50,
            idle: 400,
            ..CPUSample::default()
        };
        assert_eq!(cpu_percent(prev, curr), 0.0);
    }

    // Go: TestReadCPU_ZeroPrevGivesZero.
    #[test]
    fn cpu_percent_zero_prev_gives_zero() {
        let prev = CPUSample::default();
        let curr = CPUSample {
            user: 100,
            idle: 900,
            ..CPUSample::default()
        };
        assert_eq!(cpu_percent(prev, curr), 0.0);
    }

    // -- Disk (ports of the TestReadDisk* Go tests) --

    fn statfs_ok(blocks: u64, bsize: i64, bavail: u64, dev: u64) -> StatfsInfo {
        StatfsInfo {
            blocks,
            bsize,
            bavail,
            dev,
        }
    }

    // Go: TestReadDiskParsesMountInfoAndExcludesEFIVariables.
    #[test]
    fn read_disk_parses_mount_info_and_excludes_efi_variables() {
        let fixture = "36 25 8:1 / / rw,relatime - ext4 /dev/root rw\n\
                       37 25 0:31 / /sys/firmware/efi/efivars rw - efivarfs efivarfs rw\n";
        let mut efi_called = false;
        let (disks, err) = read_disk_from(fixture.as_bytes(), &mut |mount: &str| {
            if mount.contains("efivars") {
                efi_called = true;
            }
            Ok(statfs_ok(100, 4096, 40, 1))
        });
        assert!(err.is_none(), "{err:?}");
        assert!(
            !efi_called && disks.len() == 1 && disks[0].mount == "/",
            "pseudo filesystem leaked into disk stats: disks={disks:?} efi_called={efi_called}"
        );
        assert_eq!(disks[0].total_bytes, 100 * 4096);
        assert_eq!(disks[0].free_bytes, 40 * 4096);
    }

    // Go: TestReadDiskSurfacesStatfsErrors.
    #[test]
    fn read_disk_surfaces_statfs_errors() {
        let fixture = "36 25 8:1 / / rw - ext4 /dev/root rw\n";
        let (disks, err) = read_disk_from(fixture.as_bytes(), &mut |_: &str| {
            Err("permission denied".to_string())
        });
        assert!(disks.is_empty());
        let err = err.expect("statfs error must be reported");
        assert!(err.to_string().contains("permission denied"), "err={err}");
        assert_eq!(err.to_string(), "statfs /: permission denied");
    }

    #[test]
    fn read_disk_sorts_root_first_then_by_mount_path_length() {
        // Out of order on purpose: Go's stable sort puts "/" first, then
        // sorts by mount path length ascending.
        let fixture = "36 25 8:1 / /data/longer rw - ext4 /dev/d1 rw\n\
                       37 25 8:2 / /boot rw - ext4 /dev/d2 rw\n\
                       38 25 8:3 / / rw - ext4 /dev/d3 rw\n";
        let (disks, err) = read_disk_from(fixture.as_bytes(), &mut |mount: &str| {
            let dev = match mount {
                "/" => 3,
                "/boot" => 2,
                _ => 1,
            };
            Ok(statfs_ok(100, 4096, 40, dev))
        });
        assert!(err.is_none(), "{err:?}");
        let mounts: Vec<&str> = disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/boot", "/data/longer"]);
    }

    #[test]
    fn read_disk_dedups_by_device_after_zero_blocks_check() {
        // Mirrors Go's ordering: `seen` is checked before the zero-size guards
        // and marked only for qualifying entries — so "/" (blocks=0) does not
        // claim dev 1, letting "/data" (same dev) through, while "/boot/dup"
        // is deduped against "/boot" (dev 2).
        let fixture = "36 25 8:1 / / rw - ext4 /dev/root rw\n\
                       37 25 8:2 / /boot rw - ext4 /dev/boot rw\n\
                       38 25 8:3 / /data rw - ext4 /dev/root rw\n\
                       39 25 8:4 / /boot/dup rw - ext4 /dev/boot rw\n";
        let (disks, err) = read_disk_from(fixture.as_bytes(), &mut |mount: &str| {
            Ok(match mount {
                "/" => statfs_ok(0, 4096, 0, 1),
                "/data" => statfs_ok(200, 4096, 50, 1),
                _ => statfs_ok(100, 4096, 40, 2),
            })
        });
        assert!(err.is_none(), "{err:?}");
        let mounts: Vec<&str> = disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/boot", "/data"]);
    }

    #[test]
    fn parse_mount_info_skips_malformed_lines() {
        let fixture = "garbage line without separator\n\
                       1 2 - ext4 r\n\
                       36 25 8:1 / /mnt rw - ext4 /dev/d rw\n";
        let entries = parse_mount_info(fixture.as_bytes()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount, "/mnt");
        assert_eq!(entries[0].fstype, "ext4");
    }

    #[test]
    fn unescape_mount_path_decodes_octal_escapes() {
        assert_eq!(unescape_mount_path("/foo\\040bar"), "/foo bar");
        assert_eq!(unescape_mount_path("\\011x\\012y\\134z"), "\tx\ny\\z");
        // Replacement output is not re-scanned: \134 → '\' then "040" stays
        // literal (matches Go's single-pass NewReplacer).
        assert_eq!(unescape_mount_path("\\134040"), "\\040");
        // A non-matching backslash consumes only itself, so an escape starting
        // inside the lookahead still applies: `\\040` → `\` + ` `.
        assert_eq!(unescape_mount_path("\\\\040"), "\\ ");
        // Lone backslash and short tails pass through unchanged.
        assert_eq!(unescape_mount_path("a\\"), "a\\");
        assert_eq!(unescape_mount_path("a\\04"), "a\\04");
    }

    // -- Memory (ports of TestReadMemory_ParsesFile plus error cases) --

    #[test]
    fn read_memory_parses_fixture() {
        let m = read_memory_from(include_str!("testdata/proc_meminfo.txt").as_bytes()).unwrap();
        assert_eq!(
            m,
            MemStats {
                total_kb: 16384000,
                available_kb: 8192000,
                swap_total_kb: 4194304,
                swap_free_kb: 4194304,
            }
        );
    }

    // Go: TestReadMemory_ParsesFile verifies the real /proc/meminfo parses.
    #[test]
    fn read_memory_works_on_live_system() {
        let m = read_memory().expect("ReadMemory");
        assert!(m.total_kb > 0, "MemTotal must be > 0");
    }

    #[test]
    fn read_memory_reports_parse_and_validation_errors() {
        let err = read_memory_from(b"MemTotal: nope kB\n".as_slice()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse MemTotal: invalid digit found in string"
        );

        let err =
            read_memory_from(b"MemAvailable: x kB\nMemTotal: 100 kB\n".as_slice()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse MemAvailable: invalid digit found in string"
        );

        // Available > Total triggers Go's invalid-metrics error.
        let err =
            read_memory_from(b"MemTotal: 100 kB\nMemAvailable: 200 kB\n".as_slice()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "required memory metrics unavailable or invalid"
        );

        // Missing MemTotal fails the same check.
        let err = read_memory_from(b"MemFree: 100 kB\n".as_slice()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "required memory metrics unavailable or invalid"
        );
    }

    // -- Network (ports of TestReadNetwork_*/TestNetworkRates_* plus edge cases) --

    #[test]
    fn read_network_parses_fixture_and_skips_loopback() {
        let (nets, err) = read_network_from(include_str!("testdata/proc_net_dev.txt").as_bytes());
        assert!(err.is_none(), "{err:?}");
        assert_eq!(nets.len(), 2);
        assert!(!nets.contains_key("lo"));
        assert_eq!(
            nets["eth0"],
            NetStat {
                rx_bytes: 1234567890,
                tx_bytes: 987654321,
            }
        );
        assert_eq!(
            nets["wlan0"],
            NetStat {
                rx_bytes: 98765432,
                tx_bytes: 87654321,
            }
        );
    }

    // Go: TestReadNetwork_SkipsLoopback verifies the real /proc/net/dev.
    #[test]
    fn read_network_skips_loopback_on_live_system() {
        let (nets, err) = read_network_full();
        assert!(err.is_none(), "{err:?}");
        assert!(
            !nets.contains_key("lo"),
            "loopback interface 'lo' must be skipped"
        );
    }

    #[test]
    fn read_network_collects_bad_lines() {
        let fixture = "header one\n\
                       header two\n\
                       \x20 eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n\
                       \x20 bad: 1 2 3\n\
                       \x20 eth1: xx 0 0 0 0 0 0 0 yy 0 0 0 0 0 0 0\n\
                       no colon on this line\n";
        let (nets, err) = read_network_from(fixture.as_bytes());
        assert_eq!(nets.len(), 1);
        assert_eq!(
            nets["eth0"],
            NetStat {
                rx_bytes: 100,
                tx_bytes: 200,
            }
        );
        let err = err.expect("parse errors must be reported").to_string();
        // errors.Join output: one message per bad line, "\n" separated.
        assert!(
            err.contains("unexpected /proc/net/dev format for bad"),
            "{err}"
        );
        assert!(
            err.contains("parse network counters for eth1: invalid digit found in string\ninvalid digit found in string"),
            "{err}"
        );
    }

    // Go: TestNetworkRates_ComputesDelta.
    #[test]
    fn network_rates_computes_delta() {
        let prev = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 1000,
                tx_bytes: 500,
            },
        )]);
        let curr = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 2000,
                tx_bytes: 1500,
            },
        )]);
        let rates = network_rates(&prev, &curr, 1.0);
        let r = rates["eth0"];
        assert_eq!(r.rx_bytes_per_sec, 1000);
        assert_eq!(r.tx_bytes_per_sec, 1000);
    }

    // Go: TestNetworkRates_NoPrevGivesZero.
    #[test]
    fn network_rates_no_prev_gives_zero() {
        let prev = BTreeMap::new();
        let curr = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 2000,
                tx_bytes: 1500,
            },
        )]);
        let rates = network_rates(&prev, &curr, 1.0);
        let r = rates["eth0"];
        assert_eq!(r.rx_bytes_per_sec, 0);
        assert_eq!(r.tx_bytes_per_sec, 0);
    }

    #[test]
    fn network_rates_counter_reset_gives_zero() {
        let prev = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 2000,
                tx_bytes: 1500,
            },
        )]);
        let curr = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 1000,
                tx_bytes: 500,
            },
        )]);
        let rates = network_rates(&prev, &curr, 1.0);
        let r = rates["eth0"];
        assert_eq!(r.rx_bytes_per_sec, 0);
        assert_eq!(r.tx_bytes_per_sec, 0);
    }

    #[test]
    fn network_rates_zero_or_negative_elapsed_uses_one_second() {
        let prev = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 1000,
                tx_bytes: 500,
            },
        )]);
        let curr = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 2000,
                tx_bytes: 1500,
            },
        )]);
        for elapsed in [0.0, -2.5] {
            let rates = network_rates(&prev, &curr, elapsed);
            let r = rates["eth0"];
            assert_eq!(r.rx_bytes_per_sec, 1000, "elapsed={elapsed}");
            assert_eq!(r.tx_bytes_per_sec, 1000, "elapsed={elapsed}");
        }
    }

    #[test]
    fn network_rates_elapsed_divides_and_truncates() {
        // Go: uint64(float64(delta)/elapsedSec) truncates toward zero.
        let prev = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 0,
                tx_bytes: 0,
            },
        )]);
        let curr = BTreeMap::from([(
            "eth0".to_string(),
            NetStat {
                rx_bytes: 1001,
                tx_bytes: 999,
            },
        )]);
        let rates = network_rates(&prev, &curr, 2.0);
        let r = rates["eth0"];
        assert_eq!(r.rx_bytes_per_sec, 500); // 500.5 truncated
        assert_eq!(r.tx_bytes_per_sec, 499); // 499.5 truncated
    }

    // -- Phase 5 Extended Metrics Tests --

    #[test]
    fn read_cpu_cores_parses_fixture() {
        let cores = read_cpu_cores_from(include_str!("testdata/proc_stat.txt").as_bytes()).unwrap();
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[0].user, 617283);
        assert_eq!(cores[1].user, 617284);
    }

    #[test]
    fn cpu_cores_percent_computes_per_core() {
        let s1 = vec![
            CPUSample {
                user: 100,
                idle: 900,
                ..CPUSample::default()
            },
            CPUSample {
                user: 200,
                idle: 800,
                ..CPUSample::default()
            },
        ];
        let s2 = vec![
            CPUSample {
                user: 200,
                idle: 900,
                ..CPUSample::default()
            },
            CPUSample {
                user: 400,
                idle: 800,
                ..CPUSample::default()
            },
        ];
        let pcts = cpu_cores_percent(&s1, &s2);
        assert_eq!(pcts.len(), 2);
        assert_eq!(pcts[0], 100.0);
        assert_eq!(pcts[1], 100.0);
    }

    #[test]
    fn read_disk_io_parses_sample_and_filters_virtual() {
        let input = b"\
   7       0 loop0 100 0 1000 50 200 0 2000 60 0 10 20
   8       0 sda 500 10 10000 120 300 20 8000 140 0 20 50
 259       0 nvme0n1 1200 50 40000 200 800 30 24000 300 0 30 90
";
        let (samples, err) = read_disk_io_from(input.as_slice());
        assert!(err.is_none());
        assert_eq!(samples.len(), 2);
        assert!(!samples.contains_key("loop0"));
        assert!(samples.contains_key("sda"));
        assert!(samples.contains_key("nvme0n1"));

        let sda = &samples["sda"];
        assert_eq!(sda.reads, 500);
        assert_eq!(sda.sectors_read, 10000);
        assert_eq!(sda.writes, 300);
        assert_eq!(sda.sectors_written, 8000);
    }

    #[test]
    fn disk_io_rates_computes_throughput() {
        let mut prev = BTreeMap::new();
        prev.insert(
            "sda".to_string(),
            DiskIOSample {
                reads: 100,
                sectors_read: 2000,
                writes: 50,
                sectors_written: 1000,
            },
        );
        let mut curr = BTreeMap::new();
        curr.insert(
            "sda".to_string(),
            DiskIOSample {
                reads: 120,            // +20 ops
                sectors_read: 4000,    // +2000 sectors = 1,024,000 bytes
                writes: 60,            // +10 ops
                sectors_written: 2000, // +1000 sectors = 512,000 bytes
            },
        );
        let rates = disk_io_rates(&prev, &curr, 1.0);
        let sda = &rates["sda"];
        assert_eq!(sda.read_bytes_per_sec, 2000 * 512);
        assert_eq!(sda.write_bytes_per_sec, 1000 * 512);
        assert_eq!(sda.read_iops, Some(20));
        assert_eq!(sda.write_iops, Some(10));
    }

    #[test]
    fn read_hardware_parses_cpuinfo_and_dmi() {
        let cpuinfo = b"\
processor	: 0
model name	: Test CPU 123
cpu cores	: 2

processor	: 1
model name	: Test CPU 123
cpu cores	: 2
";
        let hw = read_hardware_from(
            Some(cpuinfo.as_slice()),
            Some("Motherboard X"),
            Some("Acme Corp"),
        );
        assert_eq!(hw.cpu_model.as_deref(), Some("Test CPU 123"));
        assert_eq!(hw.cpu_cores, Some(2));
        assert_eq!(hw.model.as_deref(), Some("Acme Corp Motherboard X"));
    }

    #[test]
    fn read_battery_graceful_on_missing_dir() {
        let non_existent = Path::new("/tmp/mu_test_non_existent_power_supply_123");
        let res = read_battery_from(non_existent).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn read_top_processes_parses_stat_and_sorts() {
        let temp_dir = std::env::temp_dir().join(format!("mu_proc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let p1 = temp_dir.join("101");
        let p2 = temp_dir.join("102");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&p2).unwrap();

        // PID 101: comm with space "proc one", utime=10, stime=20 -> total=30, rss=500, starttime=1000
        let stat1 = "101 (proc one) S 1 101 101 0 -1 4194304 100 0 0 0 10 20 0 0 20 0 1 0 1000 10000 500 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        // PID 102: comm "proc_two", utime=100, stime=50 -> total=150, rss=200, starttime=1000
        let stat2 = "102 (proc_two) R 1 102 102 0 -1 4194304 100 0 0 0 100 50 0 0 20 0 1 0 1000 10000 200 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";

        std::fs::write(p1.join("stat"), stat1).unwrap();
        std::fs::write(p2.join("stat"), stat2).unwrap();
        std::fs::write(temp_dir.join("uptime"), "100.0 200.0\n").unwrap();

        let procs = read_top_processes_from(&temp_dir, 5).unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);

        assert_eq!(procs.len(), 2);
        // PID 101 has rss=500, which has higher memory than PID 102 rss=200
        assert_eq!(procs[0].pid, 101);
        assert_eq!(procs[0].name, "proc one");
        assert!(procs[0].memory_bytes > 0);
        assert!(procs[0].cpu_percent > 0.0);
        assert_eq!(procs[1].pid, 102);
        assert_eq!(procs[1].name, "proc_two");
        assert!(procs[1].memory_bytes > 0);
        assert!(procs[1].cpu_percent > 0.0);
    }

    #[test]
    fn read_battery_parses_capacity_status_and_health() {
        let temp_dir = std::env::temp_dir().join(format!("mu_bat_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let bat0 = temp_dir.join("BAT0");
        std::fs::create_dir_all(&bat0).unwrap();

        std::fs::write(bat0.join("capacity"), "85\n").unwrap();
        std::fs::write(bat0.join("status"), "Discharging\n").unwrap();
        std::fs::write(bat0.join("energy_full"), "4500000\n").unwrap();
        std::fs::write(bat0.join("energy_full_design"), "5000000\n").unwrap();

        let bat = read_battery_from(&temp_dir)
            .unwrap()
            .expect("battery found");
        assert_eq!(bat.percentage, 85);
        assert_eq!(bat.status, "Discharging");
        assert_eq!(bat.health_percent, Some(90.0));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn read_battery_charge_fallback() {
        let temp_dir = std::env::temp_dir().join(format!("mu_bat_fb_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let bat1 = temp_dir.join("BAT1");
        std::fs::create_dir_all(&bat1).unwrap();

        std::fs::write(bat1.join("capacity"), "95\n").unwrap();
        std::fs::write(bat1.join("status"), "Charging\n").unwrap();
        std::fs::write(bat1.join("charge_full"), "3800000\n").unwrap();
        std::fs::write(bat1.join("charge_full_design"), "4000000\n").unwrap();

        let bat = read_battery_from(&temp_dir)
            .unwrap()
            .expect("battery found");
        assert_eq!(bat.percentage, 95);
        assert_eq!(bat.status, "Charging");
        assert_eq!(bat.health_percent, Some(95.0));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
