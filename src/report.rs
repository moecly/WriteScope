use crate::cli::Workload;
use crate::device::Target;
use anyhow::Result;
use serde::Serialize;
use std::io::Write;

pub const SCHEMA_VERSION: u32 = 1;
pub const TOOL: &str = "writescope";

#[derive(Debug, Serialize)]
pub struct TargetReport {
    pub path: String,
    pub filesystem: String,
    pub mount_point: String,
    pub mount_source: String,
    pub mount_options: String,
    pub super_options: String,
    pub devices: Vec<DeviceReport>,
}

#[derive(Debug, Serialize)]
pub struct DeviceReport {
    pub name: String,
    pub major: u32,
    pub minor: u32,
    pub logical_block_size: u32,
    pub rotational: bool,
}

impl From<&Target> for TargetReport {
    fn from(target: &Target) -> Self {
        Self {
            path: target.path.display().to_string(),
            filesystem: target.filesystem.clone(),
            mount_point: target.mount_point.display().to_string(),
            mount_source: target.mount_source.clone(),
            mount_options: target.mount_options.clone(),
            super_options: target.super_options.clone(),
            devices: target
                .devices
                .iter()
                .map(|device| DeviceReport {
                    name: device.name.clone(),
                    major: device.major,
                    minor: device.minor,
                    logical_block_size: device.logical_block_size,
                    rotational: device.rotational,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkloadReport {
    Seq {
        size: u64,
        block_size: u64,
        sync: String,
        direct: bool,
        preallocate: bool,
        pattern: String,
        threads: usize,
    },
    Rand {
        file_size: u64,
        block_size: u64,
        sync: String,
        prefill: bool,
        direct: bool,
        pattern: String,
        threads: usize,
        seed: u64,
    },
    Files {
        count: u64,
        file_size: u64,
        sync: String,
        pattern: String,
        threads: usize,
    },
}

impl From<&Workload> for WorkloadReport {
    fn from(workload: &Workload) -> Self {
        match workload {
            Workload::Seq {
                size,
                block_size,
                sync,
                direct,
                preallocate,
                pattern,
                threads,
            } => WorkloadReport::Seq {
                size: *size,
                block_size: *block_size,
                sync: sync.as_str().to_string(),
                direct: *direct,
                preallocate: *preallocate,
                pattern: pattern.as_str().to_string(),
                threads: *threads,
            },
            Workload::Rand {
                file_size,
                block_size,
                sync,
                prefill,
                direct,
                pattern,
                threads,
                seed,
            } => WorkloadReport::Rand {
                file_size: *file_size,
                block_size: *block_size,
                sync: sync.as_str().to_string(),
                prefill: *prefill,
                direct: *direct,
                pattern: pattern.as_str().to_string(),
                threads: *threads,
                seed: *seed,
            },
            Workload::Files {
                count,
                file_size,
                sync,
                pattern,
                threads,
            } => WorkloadReport::Files {
                count: *count,
                file_size: *file_size,
                sync: sync.as_str().to_string(),
                pattern: pattern.as_str().to_string(),
                threads: *threads,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TimelineBucket {
    pub offset_ms: u64,
    pub physical_bytes: u64,
    pub block_writes: u64,
}

#[derive(Debug, Serialize)]
pub struct ResultReport {
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub physical_active_bytes: u64,
    pub physical_settle_bytes: u64,
    pub waf: f64,
    pub active_ms: u64,
    pub settle_ms: u64,
    pub logical_mib_per_sec: f64,
    pub block_writes: u64,
    pub flushes: Option<u64>,
    pub discarded_bytes: Option<u64>,
    pub ambient_bytes_per_sec: f64,
    pub ambient_ratio: f64,
    pub timeline: Vec<TimelineBucket>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool: &'static str,
    pub version: &'static str,
    pub started_unix_ms: u64,
    pub target: TargetReport,
    pub workload: WorkloadReport,
    pub result: ResultReport,
    pub run_dir: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DeviceCounters {
    pub name: String,
    pub major: u32,
    pub minor: u32,
    pub sectors_written: u64,
    pub block_writes: u64,
    pub flushes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct InfoReport {
    pub schema_version: u32,
    pub tool: &'static str,
    pub version: &'static str,
    pub target: TargetReport,
    pub baseline_ms: u64,
    pub ambient_bytes_per_sec: f64,
    pub counters: Vec<DeviceCounters>,
    pub warnings: Vec<String>,
}

pub fn compute_waf(physical_bytes: u64, logical_bytes: u64) -> f64 {
    if logical_bytes == 0 {
        return 0.0;
    }
    physical_bytes as f64 / logical_bytes as f64
}

pub fn compute_ambient_ratio(
    ambient_bytes_per_sec: f64,
    active_ms: u64,
    settle_ms: u64,
    physical_bytes: u64,
) -> f64 {
    if physical_bytes == 0 {
        return 0.0;
    }
    let window_secs = (active_ms + settle_ms) as f64 / 1000.0;
    ambient_bytes_per_sec * window_secs / physical_bytes as f64
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_seconds(ms: u64) -> String {
    format!("{:.3} s", ms as f64 / 1000.0)
}

fn device_line(target: &TargetReport) -> String {
    target
        .devices
        .iter()
        .map(|device| {
            format!(
                "{} ({}:{}, {} B logical, {})",
                device.name,
                device.major,
                device.minor,
                device.logical_block_size,
                if device.rotational {
                    "rotational"
                } else {
                    "non-rotational"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn workload_line(workload: &WorkloadReport) -> String {
    match workload {
        WorkloadReport::Seq {
            size,
            block_size,
            sync,
            direct,
            preallocate,
            pattern,
            threads,
        } => {
            let mut line = format!(
                "seq: {}, block {}, sync={sync}, pattern={pattern}, threads={threads}",
                format_bytes(*size),
                format_bytes(*block_size)
            );
            if *preallocate {
                line.push_str(", preallocate");
            }
            if *direct {
                line.push_str(", direct");
            }
            line
        }
        WorkloadReport::Rand {
            file_size,
            block_size,
            sync,
            prefill,
            direct,
            pattern,
            threads,
            seed,
        } => {
            let mut line = format!(
                "rand: {}, block {}, sync={sync}, pattern={pattern}, threads={threads}, seed={seed}",
                format_bytes(*file_size),
                format_bytes(*block_size)
            );
            if *prefill {
                line.push_str(", prefill");
            }
            if *direct {
                line.push_str(", direct");
            }
            line
        }
        WorkloadReport::Files {
            count,
            file_size,
            sync,
            pattern,
            threads,
        } => format!(
            "files: {count} x {}, sync={sync}, pattern={pattern}, threads={threads}",
            format_bytes(*file_size)
        ),
    }
}

pub fn emit_measurement(report: &Report, json: bool) -> Result<()> {
    if json {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        serde_json::to_writer_pretty(&mut lock, report)?;
        writeln!(lock)?;
        return Ok(());
    }

    println!(
        "{:<12}{} ({} on {}, {})",
        "target",
        report.target.path,
        report.target.filesystem,
        report.target.mount_source,
        report.target.mount_point
    );
    println!("{:<12}{}", "devices", device_line(&report.target));
    println!("{:<12}{}", "workload", workload_line(&report.workload));
    println!(
        "{:<12}{}",
        "logical",
        format_bytes(report.result.logical_bytes)
    );
    println!(
        "{:<12}{} (active {}, settle {})",
        "physical",
        format_bytes(report.result.physical_bytes),
        format_bytes(report.result.physical_active_bytes),
        format_bytes(report.result.physical_settle_bytes)
    );
    println!("{:<12}{:.3}x", "WAF", report.result.waf);
    println!(
        "{:<12}{}",
        "active",
        format_seconds(report.result.active_ms)
    );
    println!(
        "{:<12}{}",
        "settle",
        format_seconds(report.result.settle_ms)
    );
    println!(
        "{:<12}{:.1} MiB/s logical",
        "throughput", report.result.logical_mib_per_sec
    );
    let mut io_parts = vec![format!("{} writes", report.result.block_writes)];
    if let Some(flushes) = report.result.flushes {
        io_parts.push(format!("{flushes} flushes"));
    }
    if let Some(discarded) = report.result.discarded_bytes {
        io_parts.push(format!("{} discarded", format_bytes(discarded)));
    }
    println!("{:<12}{}", "block I/O", io_parts.join(", "));
    println!(
        "{:<12}{}/s ({:.2}% of physical)",
        "ambient",
        format_bytes(report.result.ambient_bytes_per_sec.round() as u64),
        report.result.ambient_ratio * 100.0
    );
    if let Some(run_dir) = &report.run_dir {
        println!("{:<12}kept at {run_dir}", "run dir");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

pub fn emit_info(report: &InfoReport, json: bool) -> Result<()> {
    if json {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        serde_json::to_writer_pretty(&mut lock, report)?;
        writeln!(lock)?;
        return Ok(());
    }

    println!("{:<12}{}", "target", report.target.path);
    println!("{:<12}{}", "filesystem", report.target.filesystem);
    println!(
        "{:<12}{} on {} ({}; {})",
        "mount",
        report.target.mount_source,
        report.target.mount_point,
        report.target.mount_options,
        report.target.super_options
    );
    println!("{:<12}{}", "devices", device_line(&report.target));
    println!(
        "{:<12}{}/s over {}",
        "ambient",
        format_bytes(report.ambient_bytes_per_sec.round() as u64),
        format_seconds(report.baseline_ms)
    );
    for counters in &report.counters {
        let mut line = format!(
            "{}: {} sectors written ({}), {} block writes",
            counters.name,
            counters.sectors_written,
            format_bytes(counters.sectors_written * 512),
            counters.block_writes
        );
        if let Some(flushes) = counters.flushes {
            line.push_str(&format!(", {flushes} flushes"));
        }
        println!("{:<12}{line}", "counters");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_boundaries() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn computes_waf() {
        assert_eq!(compute_waf(2048, 1024), 2.0);
        assert_eq!(compute_waf(0, 1024), 0.0);
        assert_eq!(compute_waf(1024, 0), 0.0);
    }

    #[test]
    fn computes_ambient_ratio() {
        assert_eq!(compute_ambient_ratio(0.0, 1000, 1000, 1024), 0.0);
        assert_eq!(compute_ambient_ratio(512.0, 1000, 0, 1024), 0.5);
        assert_eq!(compute_ambient_ratio(100.0, 0, 0, 0), 0.0);
    }

    #[test]
    fn formats_seconds() {
        assert_eq!(format_seconds(1234), "1.234 s");
        assert_eq!(format_seconds(0), "0.000 s");
    }
}
