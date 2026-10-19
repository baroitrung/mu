//! Shared status data types — the JSON schema contract ported from
//! `internal/status/model.go` and `internal/status/proc.go`.
//!
//! Field names match Go's `encoding/json` output exactly: `Snapshot` uses its
//! JSON tags; nested structs keep Go's default capitalized field names.

use std::collections::BTreeMap;

use serde::Serialize;

/// Snapshot is the data structure for JSON output mode.
///
/// `scan_errors` maps to `json:"scan_errors,omitempty"` — omitted when empty.
/// `disks`/`network` are `Option` to mirror Go nil-vs-non-nil: a nil slice/map
/// serializes as `null` (never `[]`/`{}` — Go's readers only produce non-nil
/// when populated, and `network` stays nil until rates are computed), while a
/// `Some` empty map serializes as `{}` exactly like Go's `make(map)`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Snapshot {
    #[serde(serialize_with = "serialize_go_f64")]
    pub cpu_percent: f64,
    pub memory: MemStats,
    pub disks: Option<Vec<DiskStat>>,
    pub network: Option<BTreeMap<String, NetRate>>,
    pub health: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_io: Option<BTreeMap<String, DiskIORate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<HardwareInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<BatteryInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processes: Option<Vec<ProcessInfo>>,
    #[serde(rename = "scan_errors", skip_serializing_if = "Vec::is_empty")]
    pub scan_errors: Vec<String>,
}

/// Serialize an f64 the way Go's `encoding/json` does: integral finite values
/// emit without a fraction (`0`, `100`, not `0.0`), everything else uses the
/// shortest round-trip form ryu already produces (identical digits to Go's
/// `strconv.AppendFloat(f, 'f', -1, 64)` in the [1e-6, 1e21) window where Go
/// chooses 'f'; cpu_percent is clamped to [0,100] so the 'e' window is
/// unreachable).
pub fn serialize_go_f64<S>(v: &f64, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e18 {
        // -0.0 → "0" here vs Go's "-0", but cpu_percent never yields -0.0
        // (negative results are clamped to the literal 0).
        serializer.serialize_i64(*v as i64)
    } else {
        serializer.serialize_f64(*v)
    }
}

/// Apply Go `json.Encoder`'s HTML escaping to a serialized JSON document.
/// `&`, `<`, `>`, U+2028, and U+2029 never appear in JSON syntax outside
/// string literals, so a whole-document replace is exact — the replacements
/// themselves contain none of the escaped characters.
pub fn escape_html_json(text: &str) -> String {
    text.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// MemStats holds memory info. JSON keys keep Go's exported field names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MemStats {
    #[serde(rename = "TotalKB")]
    pub total_kb: u64,
    #[serde(rename = "AvailableKB")]
    pub available_kb: u64,
    #[serde(rename = "SwapTotalKB")]
    pub swap_total_kb: u64,
    #[serde(rename = "SwapFreeKB")]
    pub swap_free_kb: u64,
}

/// DiskStat holds per-mount disk info.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct DiskStat {
    #[serde(rename = "Mount")]
    pub mount: String,
    #[serde(rename = "TotalBytes")]
    pub total_bytes: u64,
    #[serde(rename = "FreeBytes")]
    pub free_bytes: u64,
}

/// NetStat holds cumulative network bytes for one interface.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NetStat {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// NetRate holds per-second byte rates for one interface.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NetRate {
    #[serde(rename = "RxBytesPerSec")]
    pub rx_bytes_per_sec: u64,
    #[serde(rename = "TxBytesPerSec")]
    pub tx_bytes_per_sec: u64,
}

/// CPUSample holds raw /proc/stat ticks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CPUSample {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

/// DiskIORate holds per-second I/O throughput rates for one device.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiskIORate {
    #[serde(rename = "read_bytes_per_sec")]
    pub read_bytes_per_sec: u64,
    #[serde(rename = "write_bytes_per_sec")]
    pub write_bytes_per_sec: u64,
    #[serde(rename = "read_iops", skip_serializing_if = "Option::is_none")]
    pub read_iops: Option<u64>,
    #[serde(rename = "write_iops", skip_serializing_if = "Option::is_none")]
    pub write_iops: Option<u64>,
}

/// DiskIOSample holds raw /proc/diskstats counters for one device.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiskIOSample {
    pub reads: u64,
    pub sectors_read: u64,
    pub writes: u64,
    pub sectors_written: u64,
}

/// HardwareInfo holds hardware and CPU specifications.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct HardwareInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<usize>,
}

/// BatteryInfo holds battery charge, status, and health metrics.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct BatteryInfo {
    pub percentage: u8,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_percent: Option<f64>,
}

/// ProcessInfo holds process resource consumption metrics.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
}

/// humanKB mirrors the Go helper: 0 → "0 B", else kb*1024 via humanBytes.
pub fn human_kb(kb: u64) -> String {
    if kb == 0 {
        return "0 B".to_string();
    }
    human_bytes(kb * 1024)
}

/// humanBytes mirrors utils humanBytes: "%d B" under 1 KiB, else "%.1f %cB".
pub fn human_bytes(b: u64) -> String {
    const UNIT: u64 = 1024;
    if b < UNIT {
        return format!("{b} B");
    }
    let mut div = UNIT;
    let mut exp = 0usize;
    let mut n = b / UNIT;
    while n >= UNIT {
        div *= UNIT;
        exp += 1;
        n /= UNIT;
    }
    format!("{:.1} {}B", b as f64 / div as f64, b"KMGTPE"[exp] as char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_json_matches_go_schema() {
        let mut snap = Snapshot {
            cpu_percent: 2.5,
            health: 58,
            ..Snapshot::default()
        };
        snap.memory = MemStats {
            total_kb: 32096344,
            available_kb: 6123136,
            swap_total_kb: 24436772,
            swap_free_kb: 20961572,
        };
        snap.disks = Some(vec![DiskStat {
            mount: "/".to_string(),
            total_bytes: 499680116736,
            free_bytes: 235636928512,
        }]);
        let mut network = BTreeMap::new();
        network.insert(
            "enp0s31f6".to_string(),
            NetRate {
                rx_bytes_per_sec: 21672,
                tx_bytes_per_sec: 7967,
            },
        );
        snap.network = Some(network);

        let text = serde_json::to_string(&snap).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["cpu_percent", "disks", "health", "memory", "network"],
            "field set must match Go output (scan_errors omitted)"
        );
        // serde derives serialize in declaration order — assert on the raw
        // text because serde_json::Value re-sorts keys alphabetically.
        let pos = |k: &str| text.find(k).unwrap();
        assert!(
            pos("cpu_percent") < pos("memory")
                && pos("memory") < pos("disks")
                && pos("disks") < pos("network")
                && pos("network") < pos("health"),
            "field order must match Go struct order: {text}"
        );
        let mem = &v["memory"];
        assert_eq!(
            mem["TotalKB"].as_u64().unwrap(),
            32096344,
            "Go default capitalization"
        );
        assert_eq!(v["disks"][0]["Mount"], "/");
        assert_eq!(v["network"]["enp0s31f6"]["RxBytesPerSec"], 21672);
    }

    #[test]
    fn nil_fields_serialize_null_like_go() {
        // Go: unset Disks/Network are nil → `"disks":null,"network":null`.
        let snap = Snapshot::default();
        let text = serde_json::to_string(&snap).unwrap();
        assert!(text.contains("\"disks\":null"), "{text}");
        assert!(text.contains("\"network\":null"), "{text}");
        // Some(empty map) → `{}` like Go's make(map); non-empty vec → array.
        let snap = Snapshot {
            disks: Some(vec![]),
            network: Some(BTreeMap::new()),
            ..Snapshot::default()
        };
        let text = serde_json::to_string(&snap).unwrap();
        assert!(text.contains("\"disks\":[]"), "{text}");
        assert!(text.contains("\"network\":{}"), "{text}");
    }

    #[test]
    fn integral_cpu_percent_serializes_like_go() {
        // Go emits 0/100 for integral floats; ryu would emit 0.0/100.0.
        let snap = Snapshot::default();
        let text = serde_json::to_string(&snap).unwrap();
        assert!(text.contains("\"cpu_percent\":0"), "{text}");
        let snap = Snapshot {
            cpu_percent: 100.0,
            ..Snapshot::default()
        };
        let text = serde_json::to_string(&snap).unwrap();
        assert!(text.contains("\"cpu_percent\":100"), "{text}");
        let snap = Snapshot {
            cpu_percent: 42.857142857142854,
            ..Snapshot::default()
        };
        let text = serde_json::to_string(&snap).unwrap();
        assert!(
            text.contains("\"cpu_percent\":42.857142857142854"),
            "{text}"
        );
    }

    #[test]
    fn escape_html_json_matches_go_encoder() {
        assert_eq!(
            escape_html_json("{\"e\":[\"a&b <c>\"]}"),
            "{\"e\":[\"a\\u0026b \\u003cc\\u003e\"]}"
        );
        assert_eq!(escape_html_json("\"x\u{2028}y\""), "\"x\\u2028y\"");
    }

    #[test]
    fn scan_errors_emitted_when_present() {
        let snap = Snapshot {
            scan_errors: vec!["cpu: cpu unavailable".to_string()],
            ..Snapshot::default()
        };
        let v: serde_json::Value = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["scan_errors"][0], "cpu: cpu unavailable");
    }

    #[test]
    fn human_formatting() {
        assert_eq!(human_kb(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_kb(2048), "2.0 MB");
    }
}
