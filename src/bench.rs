use crate::cli::{BenchPlan, GlobalOpts, Workload};
use crate::device::{self, Target};
use crate::report::{self, Report};
use crate::run;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

const AMBIENT_WARN_RATIO: f64 = 0.05;

struct BenchResult {
    kind: &'static str,
    label: String,
    logical_bytes: u64,
    reports: Vec<Report>,
}

pub fn run(opts: &GlobalOpts, plan: BenchPlan) -> Result<()> {
    let target = device::resolve(&opts.path)?;
    let baseline = opts.baseline.max(Duration::from_secs(1));
    let settle = if target.filesystem == "btrfs" {
        opts.settle.max(Duration::from_secs(5))
    } else {
        opts.settle.max(Duration::from_secs(2))
    };

    if !opts.json {
        println!("writescope benchmark");
        println!(
            "{:<12}{} ({} on {})",
            "target",
            target.path.display(),
            target.filesystem,
            target.mount_source
        );
        println!(
            "{:<12}{} device(s), baseline {}, settle {}, repeats {}, sync=file",
            "profile",
            target.devices.len(),
            humantime::format_duration(baseline),
            humantime::format_duration(settle),
            plan.repeats
        );
        println!();
    }

    sync_target(&target.path)?;

    let workloads = [
        ("seq", plan.seq),
        ("rand", plan.rand),
        ("files", plan.files),
    ];
    let mut results = Vec::with_capacity(workloads.len());

    for (kind, workload) in workloads {
        let label = label(&workload);
        let mut reports = Vec::with_capacity(plan.repeats as usize);
        for index in 0..plan.repeats {
            if !opts.json {
                eprintln!("run {}/{}  {label}", index + 1, plan.repeats);
            }
            let run_opts = GlobalOpts {
                path: opts.path.clone(),
                json: false,
                keep: false,
                baseline,
                settle,
                sample_interval: opts.sample_interval,
            };
            reports.push(run::run_measurement(run_opts, workload.clone())?);
            sync_target(&target.path)?;
        }
        results.push(BenchResult {
            kind,
            label,
            logical_bytes: workload.logical_bytes()?,
            reports,
        });
    }

    if opts.json {
        emit_json(&target, baseline, settle, plan.repeats, &results)
    } else {
        emit_table(&results);
        Ok(())
    }
}

fn label(workload: &Workload) -> String {
    match workload {
        Workload::Seq {
            size, block_size, ..
        } => format!(
            "seq {} / {}",
            report::format_bytes(*size),
            report::format_bytes(*block_size)
        ),
        Workload::Rand {
            file_size,
            block_size,
            prefill,
            ..
        } => format!(
            "rand {} / {}{}",
            report::format_bytes(*file_size),
            report::format_bytes(*block_size),
            if *prefill { " prefill" } else { "" }
        ),
        Workload::Files {
            count, file_size, ..
        } => format!("files {count} x {}", report::format_bytes(*file_size)),
    }
}

fn emit_table(results: &[BenchResult]) {
    println!(
        "{:<34}{:>10}{:>12}{:>14}  runs",
        "workload", "logical", "physical", "median WAF"
    );
    for result in results {
        let wafs = waf_runs(result);
        let physical = median_u64(&physical_runs(result));
        println!(
            "{:<34}{:>10}{:>12}{:>11.3}x  {}",
            result.label,
            report::format_bytes(result.logical_bytes),
            report::format_bytes(physical),
            median_f64(&wafs),
            wafs.iter()
                .map(|waf| format!("{waf:.3}x"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let mut warned = false;
    for result in results {
        let ambient = ambient_max(result);
        if ambient > AMBIENT_WARN_RATIO {
            println!(
                "warning: {} ambient I/O reached {:.1}% of physical writes",
                result.kind,
                ambient * 100.0
            );
            warned = true;
        }
        let warnings: BTreeSet<_> = result
            .reports
            .iter()
            .flat_map(|report| report.warnings.iter())
            .collect();
        for warning in warnings {
            if warning.starts_with("ambient") {
                continue;
            }
            println!("warning: {}: {warning}", result.kind);
            warned = true;
        }
    }
    if warned {
        println!();
    }
    println!("note        WAF is workload-dependent; compare the same workload and configuration.");
}

fn emit_json(
    target: &Target,
    baseline: Duration,
    settle: Duration,
    repeats: u32,
    results: &[BenchResult],
) -> Result<()> {
    let mut entries = Vec::with_capacity(results.len());
    for result in results {
        let wafs = waf_runs(result);
        let warnings: BTreeSet<_> = result
            .reports
            .iter()
            .flat_map(|report| report.warnings.iter().cloned())
            .collect();
        entries.push(json!({
            "kind": result.kind,
            "label": result.label,
            "logical_bytes": result.logical_bytes,
            "physical_bytes_median": median_u64(&physical_runs(result)),
            "waf_runs": wafs,
            "waf_median": median_f64(&wafs),
            "ambient_ratio_max": ambient_max(result),
            "warnings": warnings,
            "runs": result.reports,
        }));
    }

    let output = json!({
        "schema_version": report::SCHEMA_VERSION,
        "tool": report::TOOL,
        "version": env!("CARGO_PKG_VERSION"),
        "target": report::TargetReport::from(target),
        "baseline_ms": baseline.as_millis() as u64,
        "settle_ms": settle.as_millis() as u64,
        "repeats": repeats,
        "results": entries,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn waf_runs(result: &BenchResult) -> Vec<f64> {
    result
        .reports
        .iter()
        .map(|report| report.result.waf)
        .collect()
}

fn physical_runs(result: &BenchResult) -> Vec<u64> {
    result
        .reports
        .iter()
        .map(|report| report.result.physical_bytes)
        .collect()
}

fn ambient_max(result: &BenchResult) -> f64 {
    result
        .reports
        .iter()
        .map(|report| report.result.ambient_ratio)
        .fold(0.0, f64::max)
}

fn median_f64(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn median_u64(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        sorted[middle - 1] + (sorted[middle] - sorted[middle - 1]) / 2
    } else {
        sorted[middle]
    }
}

fn sync_target(path: &Path) -> Result<()> {
    let dir = File::open(path)
        .with_context(|| format!("failed to open {} for syncfs", path.display()))?;
    let result = unsafe { libc::syncfs(dir.as_raw_fd()) };
    if result == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("syncfs failed for {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_and_even_runs() {
        assert_eq!(median_f64(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median_f64(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median_u64(&[3, 1, 2]), 2);
        assert_eq!(median_u64(&[4, 1, 3, 2]), 2);
    }
}
