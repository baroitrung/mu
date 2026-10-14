//! Status subsystem ported from `internal/status/` — /proc + mountinfo
//! parsing, health score, snapshot JSON output, and (later) the TUI dashboard.

// Staged port: items consumed only by the deferred TUI phase (Dashboard,
// model::human_*, proc::read_disk/read_network wrappers) are intentionally
// dead for now — same convention as `#[allow(dead_code)] mod ...` in main.rs.
// Remove once the TUI phase wires them up.
#![allow(dead_code)]

pub mod health;
pub mod model;
pub mod proc;

pub use health::health_score_available;
pub use model::{
    BatteryInfo, CPUSample, DiskIORate, DiskIOSample, DiskStat, HardwareInfo, MemStats, NetRate,
    NetStat, ProcessInfo, Snapshot,
};
// Re-exported for the TUI phase (view.go ports) — not yet consumed.
#[allow(unused_imports)]
pub use health::health_score;
#[allow(unused_imports)]
pub use model::{human_bytes, human_kb};
// Glob keeps the re-export correct while proc.rs settles its final API
// (`read_disk_full`/`read_network_full` carry Go's (values, joinedErr) pairs).
pub use proc::*;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Go's `readDiskFn` — `(values, joinedErr)` pair so partial values survive
/// errors, matching `proc::read_disk_full`.
pub type DiskReader = Box<dyn FnMut() -> (Vec<DiskStat>, Option<Error>)>;

/// DiskIOReader returns (map of device to DiskIOSample, scan error).
pub type DiskIOReader = Box<dyn FnMut() -> (BTreeMap<String, DiskIOSample>, Option<Error>)>;

/// Go's `readNetworkFn` — `(values, joinedErr)` pair, matching
/// `proc::read_network_full`.
pub type NetReader = Box<dyn FnMut() -> (BTreeMap<String, NetStat>, Option<Error>)>;

/// Injectable metric readers — the port of Go's package-var hooks
/// (`readCPUFn`, `readMemoryFn`, `readDiskFn`, `readNetworkFn`) that
/// `status_test.go` swaps out for fakes, plus Phase 5 extended collectors.
pub struct Readers {
    pub cpu: Box<dyn FnMut() -> Result<CPUSample>>,
    pub cpu_cores: Box<dyn FnMut() -> Result<Vec<CPUSample>>>,
    pub memory: Box<dyn FnMut() -> Result<MemStats>>,
    pub disk: DiskReader,
    pub disk_io: DiskIOReader,
    pub network: NetReader,
    pub hardware: Box<dyn FnMut() -> HardwareInfo>,
    pub battery: Box<dyn FnMut() -> Result<Option<BatteryInfo>>>,
    pub processes: Box<dyn FnMut() -> Result<Vec<ProcessInfo>>>,
}

impl Default for Readers {
    fn default() -> Self {
        Readers {
            cpu: Box::new(|| Ok(CPUSample::default())),
            cpu_cores: Box::new(|| Ok(Vec::new())),
            memory: Box::new(|| Ok(MemStats::default())),
            disk: Box::new(|| (Vec::new(), None)),
            disk_io: Box::new(|| (BTreeMap::new(), None)),
            network: Box::new(|| (BTreeMap::new(), None)),
            hardware: Box::new(HardwareInfo::default),
            battery: Box::new(|| Ok(None)),
            processes: Box::new(|| Ok(Vec::new())),
        }
    }
}
impl Readers {
    /// Readers bound to the real /proc + mountinfo parsers.
    pub fn real() -> Self {
        Readers {
            cpu: Box::new(proc::read_cpu),
            cpu_cores: Box::new(proc::read_cpu_cores),
            memory: Box::new(proc::read_memory),
            disk: Box::new(proc::read_disk_full),
            disk_io: Box::new(proc::read_disk_io_full),
            network: Box::new(proc::read_network_full),
            hardware: Box::new(proc::read_hardware),
            battery: Box::new(proc::read_battery),
            processes: Box::new(proc::read_top_processes),
        }
    }
}

/// Sleep for `interval` — the port of Go's waitContext minus cancellation.
/// Ctrl-C kills the CLI process outright; Go's ctx-cancel path only produced
/// a scan_error, which cannot occur without a context to cancel.
fn wait(interval: Duration) {
    if interval > Duration::ZERO {
        std::thread::sleep(interval);
    }
}

/// CollectSnapshot gathers one scriptable status sample and reports every
/// unavailable metric in `scan_errors`.
pub fn collect_snapshot(interval: Duration) -> Snapshot {
    collect_snapshot_with(interval, &mut Readers::real())
}

/// CollectSnapshot with injectable readers — the testable core of
/// `CollectSnapshot` (Go tests swap the `read*Fn` package vars instead).
pub fn collect_snapshot_with(interval: Duration, readers: &mut Readers) -> Snapshot {
    let mut snap = Snapshot::default();

    // Sample initial rates
    let cpu1 = (readers.cpu)();
    let cores1 = (readers.cpu_cores)();
    let (disk_io1, _) = (readers.disk_io)();
    let net1 = (readers.network)();

    wait(interval);

    let mut cpu_available = false;
    match cpu1 {
        Err(err) => snap.scan_errors.push(format!("cpu: {err}")),
        Ok(s1) => match (readers.cpu)() {
            Err(second_err) => snap.scan_errors.push(format!("cpu: {second_err}")),
            Ok(s2) => {
                snap.cpu_percent = cpu_percent(s1, s2);
                cpu_available = true;
            }
        },
    }

    if let (Ok(c1), Ok(c2)) = (cores1, (readers.cpu_cores)()) {
        snap.cpu_cores =
            (!c1.is_empty() && !c2.is_empty()).then(|| proc::cpu_cores_percent(&c1, &c2));
    }

    let seconds = interval.as_secs_f64();
    let seconds = if seconds <= 0.0 { 1.0 } else { seconds };

    let (disk_io2, _) = (readers.disk_io)();
    if !disk_io1.is_empty() || !disk_io2.is_empty() {
        let rates = proc::disk_io_rates(&disk_io1, &disk_io2, seconds);
        snap.disk_io = Some(rates);
    }

    let mut mem_available = false;
    match (readers.memory)() {
        Err(mem_err) => snap.scan_errors.push(format!("memory: {mem_err}")),
        Ok(mem) => {
            snap.memory = mem;
            mem_available = true;
        }
    }

    let (disks, disk_err) = (readers.disk)();
    // Go: `snap.Disks = disks` — nil slice (zero entries) serializes null.
    snap.disks = if disks.is_empty() { None } else { Some(disks) };
    if let Some(disk_err) = disk_err {
        snap.scan_errors.push(format!("disk: {disk_err}"));
    }

    match net1 {
        (_, Some(net_err)) => snap.scan_errors.push(format!("network: {net_err}")),
        (n1, None) => match (readers.network)() {
            (_, Some(second_err)) => {
                snap.scan_errors.push(format!("network: {second_err}"));
            }
            (n2, None) => {
                // Go: NetworkRates returns make(map) → `{}` even when
                // empty; Some(_) distinguishes success from the nil/null
                // error paths above.
                snap.network = Some(network_rates(&n1, &n2, seconds));
            }
        },
    }

    let hw = (readers.hardware)();
    if hw.model.is_some() || hw.cpu_model.is_some() || hw.cpu_cores.is_some() {
        snap.hardware = Some(hw);
    }

    if let Ok(Some(bat)) = (readers.battery)() {
        snap.battery = Some(bat);
    }

    if let Ok(procs) = (readers.processes)() {
        snap.processes = (!procs.is_empty()).then_some(procs);
    }

    snap.health = health_score_available(
        snap.cpu_percent,
        cpu_available,
        snap.memory,
        mem_available,
        snap.disks.as_deref().unwrap_or(&[]),
    );
    snap
}

/// Snapshot JSON helper: one-shot collection with default 1s interval.
pub fn collect_snapshot_json() -> Snapshot {
    collect_snapshot(Duration::from_secs(1))
}

/// Dashboard ports Go's `Model` tick-time state machine for the later TUI.
/// `tick` mirrors the `tickMsg` branch of `Model.Update`; Bubbletea's
/// view/key handling and tick scheduling are deferred to the ratatui phase.
pub struct Dashboard {
    /// Previous CPU sample. Go compares `prevCPU != (CPUSample{})`, so the
    /// zero-value compare is kept verbatim: an all-zero read counts as
    /// "no previous sample" and a failed read resets it, exactly like Go.
    pub prev_cpu: CPUSample,
    pub prev_cpu_cores: Vec<CPUSample>,
    pub cpu: f64,
    pub cpu_cores: Vec<f64>,
    pub cpu_ready: bool,
    pub mem: MemStats,
    pub disks: Vec<DiskStat>,
    pub prev_disk_io: Option<BTreeMap<String, DiskIOSample>>,
    pub disk_io_rates: BTreeMap<String, DiskIORate>,
    /// `None` mirrors Go's nil map — `Some(empty)` still counts as present.
    pub prev_nets: Option<BTreeMap<String, NetStat>>,
    pub net_rates: BTreeMap<String, NetRate>,
    pub hardware: HardwareInfo,
    pub battery: Option<BatteryInfo>,
    pub processes: Vec<ProcessInfo>,
    pub health: i64,
    /// Timestamp of the previous tick (Go `prevTime`); the network rate
    /// interval is `prev_time.elapsed()` clamped to >= 1s like Go.
    pub prev_time: Instant,
    pub scan_errors: Vec<String>,
}

impl Dashboard {
    /// Fresh dashboard — Go `NewModel` minus the view-only `width` field.
    pub fn new() -> Self {
        Self::default()
    }

    /// One dashboard tick — the `tickMsg` branch of Go's `Model.Update`,
    /// minus the `tea.Tick` reschedule the caller owns.
    pub fn tick(&mut self, readers: &mut Readers) {
        self.scan_errors.clear();
        self.cpu_ready = false;
        let mut cpu_available = false;
        let mut mem_available = false;

        // 1. Read CPU; compute percent from prev.
        match (readers.cpu)() {
            Ok(curr) => {
                if self.prev_cpu != CPUSample::default() {
                    self.cpu = cpu_percent(self.prev_cpu, curr);
                    cpu_available = true;
                    self.cpu_ready = true;
                }
                self.prev_cpu = curr;
            }
            Err(err) => {
                self.cpu = 0.0;
                self.prev_cpu = CPUSample::default();
                self.scan_errors.push(format!("cpu: {err}"));
            }
        }

        // 1b. Per-core CPU
        if let Ok(curr_cores) = (readers.cpu_cores)() {
            if !self.prev_cpu_cores.is_empty() && !curr_cores.is_empty() {
                self.cpu_cores = proc::cpu_cores_percent(&self.prev_cpu_cores, &curr_cores);
            }
            self.prev_cpu_cores = curr_cores;
        }

        // 2. Read memory.
        match (readers.memory)() {
            Err(mem_err) => self.scan_errors.push(format!("memory: {mem_err}")),
            Ok(mem) => {
                self.mem = mem;
                mem_available = true;
            }
        }

        // 3. Read disk — partial values survive errors.
        let (disks, disk_err) = (readers.disk)();
        self.disks = disks;
        if let Some(disk_err) = disk_err {
            self.scan_errors.push(format!("disk: {disk_err}"));
        }

        // 3b. Read disk I/O
        let elapsed = self.prev_time.elapsed().as_secs_f64();
        let elapsed = if elapsed <= 0.0 { 1.0 } else { elapsed };
        let (curr_disk_io, _) = (readers.disk_io)();
        if let Some(prev_io) = &self.prev_disk_io {
            self.disk_io_rates = proc::disk_io_rates(prev_io, &curr_disk_io, elapsed);
        }
        self.prev_disk_io = Some(curr_disk_io);

        // 4. Read network; compute rates from the previous sample.
        match (readers.network)() {
            (curr_nets, None) => {
                if let Some(prev_nets) = &self.prev_nets {
                    self.net_rates = network_rates(prev_nets, &curr_nets, elapsed);
                }
                self.prev_nets = Some(curr_nets);
            }
            (_, Some(err)) => self.scan_errors.push(format!("network: {err}")),
        }
        self.prev_time = Instant::now();

        // 4b. Hardware, battery, processes
        self.hardware = (readers.hardware)();
        self.battery = (readers.battery)().unwrap_or(None);
        self.processes = (readers.processes)().unwrap_or_default();

        // 5. Compute health.
        self.health = health_score_available(
            self.cpu,
            cpu_available,
            self.mem,
            mem_available,
            &self.disks,
        );
    }
}

impl Default for Dashboard {
    fn default() -> Self {
        Dashboard {
            prev_cpu: CPUSample::default(),
            prev_cpu_cores: Vec::new(),
            cpu: 0.0,
            cpu_cores: Vec::new(),
            cpu_ready: false,
            mem: MemStats::default(),
            disks: Vec::new(),
            prev_disk_io: None,
            disk_io_rates: BTreeMap::new(),
            prev_nets: None,
            net_rates: BTreeMap::new(),
            hardware: HardwareInfo::default(),
            battery: None,
            processes: Vec::new(),
            health: 0,
            prev_time: Instant::now(),
            scan_errors: Vec::new(),
        }
    }
}

/// Run launches the live status dashboard (Go `Run`).
/// Outputs JSON if `--json` is set or stdout is not a TTY; on a TTY without
/// `--json` it runs the alt-screen dashboard (`tea.NewProgram(m).Run()` in
/// Go). A terminal error returns 1 — the code cobra produces after printing
/// the program's error.
/// Returns the process exit code.
pub fn run(json: bool) -> i32 {
    if json || !std::io::stdout().is_terminal() {
        let snap = collect_snapshot(Duration::from_secs(1));
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        // Go's json.Encoder.Encode emits compact JSON (HTML-escaped) + "\n".
        let written = serde_json::to_string(&snap)
            .map(|text| model::escape_html_json(&text))
            .map_err(|e| e.to_string())
            .and_then(|text| {
                out.write_all(text.as_bytes())
                    .and_then(|()| out.write_all(b"\n"))
                    .and_then(|()| out.flush())
                    .map_err(|e| e.to_string())
            });
        return match written {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("{err}");
                1
            }
        };
    }
    match crate::tui::run_status_dashboard() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// RunWatch continuously streams status snapshots as NDJSON lines until cancelled
/// or downstream pipe closes.
pub fn run_watch(interval: Duration) -> i32 {
    let mut readers = Readers::real();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut prev_cpu = (readers.cpu)().unwrap_or_default();
    let mut prev_cores = (readers.cpu_cores)().unwrap_or_default();
    let (mut prev_disk_io, _) = (readers.disk_io)();
    let (mut prev_net, _) = (readers.network)();
    let mut prev_time = Instant::now();

    loop {
        wait(interval);

        let mut snap = Snapshot::default();
        let elapsed = prev_time.elapsed().as_secs_f64();
        let elapsed = if elapsed <= 0.0 { 1.0 } else { elapsed };
        prev_time = Instant::now();

        let mut cpu_available = false;
        match (readers.cpu)() {
            Ok(curr_cpu) => {
                if prev_cpu != CPUSample::default() {
                    snap.cpu_percent = cpu_percent(prev_cpu, curr_cpu);
                    cpu_available = true;
                }
                prev_cpu = curr_cpu;
            }
            Err(e) => {
                snap.scan_errors.push(format!("cpu: {e}"));
                prev_cpu = CPUSample::default();
            }
        }

        if let Ok(curr_cores) = (readers.cpu_cores)() {
            if !prev_cores.is_empty() && !curr_cores.is_empty() {
                snap.cpu_cores = Some(proc::cpu_cores_percent(&prev_cores, &curr_cores));
            }
            prev_cores = curr_cores;
        }

        let (curr_disk_io, _) = (readers.disk_io)();
        if !prev_disk_io.is_empty() || !curr_disk_io.is_empty() {
            snap.disk_io = Some(proc::disk_io_rates(&prev_disk_io, &curr_disk_io, elapsed));
        }
        prev_disk_io = curr_disk_io;

        match (readers.network)() {
            (curr_net, None) => {
                snap.network = Some(network_rates(&prev_net, &curr_net, elapsed));
                prev_net = curr_net;
            }
            (_, Some(e)) => {
                snap.scan_errors.push(format!("network: {e}"));
            }
        }

        let mut mem_available = false;
        match (readers.memory)() {
            Ok(mem) => {
                snap.memory = mem;
                mem_available = true;
            }
            Err(e) => snap.scan_errors.push(format!("memory: {e}")),
        }

        let (disks, disk_err) = (readers.disk)();
        snap.disks = if disks.is_empty() { None } else { Some(disks) };
        if let Some(e) = disk_err {
            snap.scan_errors.push(format!("disk: {e}"));
        }

        let hw = (readers.hardware)();
        if hw.model.is_some() || hw.cpu_model.is_some() || hw.cpu_cores.is_some() {
            snap.hardware = Some(hw);
        }
        if let Ok(Some(bat)) = (readers.battery)() {
            snap.battery = Some(bat);
        }
        if let Ok(procs) = (readers.processes)() {
            snap.processes = (!procs.is_empty()).then_some(procs);
        }

        snap.health = health_score_available(
            snap.cpu_percent,
            cpu_available,
            snap.memory,
            mem_available,
            snap.disks.as_deref().unwrap_or(&[]),
        );

        let body = match serde_json::to_string(&snap) {
            Ok(b) => model::escape_html_json(&b),
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };

        if let Err(e) = out
            .write_all(body.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush())
        {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                return 0;
            }
            eprintln!("{e}");
            return 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::msg;
    use std::collections::VecDeque;

    fn failing_readers() -> Readers {
        Readers {
            cpu: Box::new(|| msg("cpu unavailable")),
            memory: Box::new(|| msg("memory unavailable")),
            disk: Box::new(|| (Vec::new(), Some(Error::Msg("disk unavailable".to_string())))),
            network: Box::new(|| {
                (
                    BTreeMap::new(),
                    Some(Error::Msg("network unavailable".to_string())),
                )
            }),
            ..Readers::default()
        }
    }

    #[test]
    fn collect_snapshot_surfaces_unavailable_metrics() {
        let mut readers = failing_readers();
        let snapshot = collect_snapshot_with(Duration::ZERO, &mut readers);
        let joined = snapshot.scan_errors.join(" ");
        for metric in [
            "cpu unavailable",
            "memory unavailable",
            "disk unavailable",
            "network unavailable",
        ] {
            assert!(
                joined.contains(metric),
                "missing {metric:?} from {:?}",
                snapshot.scan_errors
            );
        }
        assert_eq!(
            snapshot.health, 0,
            "missing metrics produced health {}",
            snapshot.health
        );
    }

    #[test]
    fn collect_snapshot_keeps_partial_disk_values_on_error() {
        // Go: `snap.Disks = disks` runs even when diskErr != nil.
        let mut readers = Readers {
            disk: Box::new(|| {
                (
                    vec![DiskStat {
                        mount: "/".to_string(),
                        total_bytes: 100,
                        free_bytes: 50,
                    }],
                    Some(Error::Msg("statfs /x: permission denied".to_string())),
                )
            }),
            ..failing_readers()
        };
        let snap = collect_snapshot_with(Duration::ZERO, &mut readers);
        assert_eq!(
            snap.disks.as_deref().unwrap_or(&[]).len(),
            1,
            "partial disk list must survive the error"
        );
        assert!(
            snap.scan_errors
                .iter()
                .any(|e| e == "disk: statfs /x: permission denied"),
            "missing disk error in {:?}",
            snap.scan_errors
        );
    }

    #[test]
    fn dashboard_cpu_recovery_requires_two_fresh_samples() {
        let mut samples: VecDeque<Result<CPUSample>> = [
            Ok(CPUSample {
                user: 10,
                idle: 90,
                ..CPUSample::default()
            }),
            Ok(CPUSample {
                user: 20,
                idle: 180,
                ..CPUSample::default()
            }),
            Err(Error::Msg("read failed".to_string())),
            Ok(CPUSample {
                user: 30,
                idle: 270,
                ..CPUSample::default()
            }),
            Ok(CPUSample {
                user: 40,
                idle: 360,
                ..CPUSample::default()
            }),
        ]
        .into_iter()
        .collect();

        let mut readers = Readers {
            cpu: Box::new(move || samples.pop_front().expect("cpu sample exhausted")),
            memory: Box::new(|| {
                Ok(MemStats {
                    total_kb: 100,
                    available_kb: 50,
                    ..MemStats::default()
                })
            }),
            disk: Box::new(|| {
                (
                    vec![DiskStat {
                        mount: "/".to_string(),
                        total_bytes: 100,
                        free_bytes: 50,
                    }],
                    None,
                )
            }),
            network: Box::new(|| (BTreeMap::new(), None)),
            ..Readers::default()
        };

        let mut dashboard = Dashboard::new();
        let want_ready = [false, true, false, false, true];
        for (i, want) in want_ready.iter().enumerate() {
            dashboard.tick(&mut readers);
            assert_eq!(
                dashboard.cpu_ready,
                *want,
                "sample {} cpu_ready={} want={}",
                i + 1,
                dashboard.cpu_ready,
                want
            );
            if i == 2 {
                assert_eq!(
                    dashboard.prev_cpu,
                    CPUSample::default(),
                    "failed CPU read retained stale previous sample"
                );
            }
        }
    }

    #[test]
    fn snapshot_json_shape_matches_go_schema() {
        let mut readers = Readers {
            cpu: Box::new(|| {
                Ok(CPUSample {
                    user: 10,
                    idle: 90,
                    ..CPUSample::default()
                })
            }),
            memory: Box::new(|| {
                Ok(MemStats {
                    total_kb: 100,
                    available_kb: 50,
                    ..MemStats::default()
                })
            }),
            disk: Box::new(|| {
                (
                    vec![DiskStat {
                        mount: "/".to_string(),
                        total_bytes: 100,
                        free_bytes: 50,
                    }],
                    None,
                )
            }),
            network: Box::new(|| {
                let mut nets = BTreeMap::new();
                nets.insert(
                    "eth0".to_string(),
                    NetStat {
                        rx_bytes: 1000,
                        tx_bytes: 500,
                    },
                );
                (nets, None)
            }),
            ..Readers::default()
        };
        let snap = collect_snapshot_with(Duration::ZERO, &mut readers);
        assert!(snap.scan_errors.is_empty());
        let v: serde_json::Value = serde_json::to_value(&snap).unwrap();
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["cpu_percent", "disks", "health", "memory", "network"]
        );
    }

    #[test]
    fn snapshot_json_includes_scan_errors_when_present() {
        let mut readers = failing_readers();
        let snap = collect_snapshot_with(Duration::ZERO, &mut readers);
        let v: serde_json::Value = serde_json::to_value(&snap).unwrap();
        let errors = v["scan_errors"]
            .as_array()
            .expect("scan_errors must be emitted when metrics fail");
        assert_eq!(errors.len(), 4);
    }

    #[test]
    fn snapshot_json_includes_extended_fields_when_present() {
        let mut readers = Readers {
            cpu_cores: Box::new(|| {
                Ok(vec![CPUSample {
                    user: 10,
                    idle: 90,
                    ..CPUSample::default()
                }])
            }),
            disk_io: Box::new(|| {
                let mut map = BTreeMap::new();
                map.insert(
                    "sda".to_string(),
                    DiskIOSample {
                        reads: 10,
                        sectors_read: 100,
                        writes: 5,
                        sectors_written: 50,
                    },
                );
                (map, None)
            }),
            hardware: Box::new(|| HardwareInfo {
                model: Some("TestBox".to_string()),
                cpu_model: Some("FastCPU".to_string()),
                cpu_cores: Some(4),
            }),
            battery: Box::new(|| {
                Ok(Some(BatteryInfo {
                    percentage: 95,
                    status: "Charging".to_string(),
                    health_percent: Some(99.0),
                }))
            }),
            processes: Box::new(|| {
                Ok(vec![ProcessInfo {
                    pid: 1,
                    name: "systemd".to_string(),
                    cpu_percent: 0.5,
                    memory_bytes: 1024,
                }])
            }),
            ..Readers::default()
        };
        let snap = collect_snapshot_with(Duration::ZERO, &mut readers);
        let v: serde_json::Value = serde_json::to_value(&snap).unwrap();
        assert!(v.get("cpu_cores").is_some(), "missing cpu_cores");
        assert!(v.get("disk_io").is_some(), "missing disk_io");
        assert!(v.get("hardware").is_some(), "missing hardware");
        assert!(v.get("battery").is_some(), "missing battery");
        assert!(v.get("processes").is_some(), "missing processes");
    }
}
