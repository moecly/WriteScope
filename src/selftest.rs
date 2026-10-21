use crate::cli::{GlobalOpts, Pattern, SyncMode, Workload};
use crate::device;
use crate::report::{self, Report};
use crate::run;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;

const WINDOW: Duration = Duration::from_millis(100);
const MIB: u64 = 1024 * 1024;

struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

pub fn run(opts: &GlobalOpts) -> Result<()> {
    let target = device::resolve(&opts.path)?;
    if !opts.json {
        println!("writescope self-test");
        println!(
            "{:<12}{} ({} on {})",
            "target",
            target.path.display(),
            target.filesystem,
            target.mount_source
        );
        println!(
            "{:<12}{} device(s), baseline/settle {}ms",
            "devices",
            target.devices.len(),
            WINDOW.as_millis()
        );
        println!();
    }

    let cases = cases();
    let mut checks = Vec::with_capacity(cases.len() + 1);
    for case in cases {
        checks.push(run_case(opts, case));
    }
    checks.push(keep_case(opts));
    checks.push(leftover_case(&target.path));

    let passed = checks.iter().filter(|check| check.ok).count();
    let failed = checks.len() - passed;

    if opts.json {
        let checks: Vec<_> = checks
            .iter()
            .map(|check| {
                json!({
                    "name": check.name,
                    "ok": check.ok,
                    "detail": check.detail,
                })
            })
            .collect();
        let report = json!({
            "schema_version": report::SCHEMA_VERSION,
            "tool": report::TOOL,
            "version": env!("CARGO_PKG_VERSION"),
            "target": report::TargetReport::from(&target),
            "baseline_ms": WINDOW.as_millis() as u64,
            "settle_ms": WINDOW.as_millis() as u64,
            "checks": checks,
            "passed": passed,
            "failed": failed,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for check in &checks {
            let status = if check.ok { "PASS" } else { "FAIL" };
            println!("{:<5}{:<28}{}", status, check.name, check.detail);
        }
        println!();
        println!("{:<12}{} passed, {} failed", "result", passed, failed);
    }

    if failed > 0 {
        bail!("self-test failed: {failed} check(s)");
    }
    Ok(())
}

struct Case {
    name: &'static str,
    workload: Workload,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "seq 64MiB",
            workload: Workload::Seq {
                size: 64 * MIB,
                block_size: MIB,
                sync: SyncMode::File,
                direct: false,
                preallocate: false,
                pattern: Pattern::Random,
                threads: 1,
            },
        },
        Case {
            name: "seq direct+preallocate",
            workload: Workload::Seq {
                size: 8 * MIB,
                block_size: MIB,
                sync: SyncMode::Write,
                direct: true,
                preallocate: true,
                pattern: Pattern::Random,
                threads: 1,
            },
        },
        Case {
            name: "rand 32MiB threads=4",
            workload: Workload::Rand {
                file_size: 32 * MIB,
                block_size: 4096,
                sync: SyncMode::File,
                prefill: false,
                direct: false,
                pattern: Pattern::Random,
                threads: 4,
                seed: 42,
            },
        },
        Case {
            name: "rand prefill+direct",
            workload: Workload::Rand {
                file_size: 8 * MIB,
                block_size: 4096,
                sync: SyncMode::File,
                prefill: true,
                direct: true,
                pattern: Pattern::Random,
                threads: 1,
                seed: 42,
            },
        },
        Case {
            name: "files 100x4KiB threads=4",
            workload: Workload::Files {
                count: 100,
                file_size: 4096,
                sync: SyncMode::File,
                pattern: Pattern::Random,
                threads: 4,
            },
        },
        Case {
            name: "files sync=fs",
            workload: Workload::Files {
                count: 20,
                file_size: 4096,
                sync: SyncMode::Fs,
                pattern: Pattern::Zero,
                threads: 1,
            },
        },
    ]
}

fn run_case(opts: &GlobalOpts, case: Case) -> Check {
    let expected = match case.workload.logical_bytes() {
        Ok(bytes) => bytes,
        Err(error) => return failed(case.name, error),
    };
    let run_opts = GlobalOpts {
        path: opts.path.clone(),
        json: false,
        keep: false,
        baseline: opts.baseline.min(WINDOW),
        settle: opts.settle.min(WINDOW),
        sample_interval: opts.sample_interval,
    };
    match run::run_measurement(run_opts, case.workload) {
        Ok(report) => match verify(expected, &report) {
            Ok(()) => Check {
                name: case.name,
                ok: true,
                detail: format!(
                    "WAF {:.3}x, physical {}, {}",
                    report.result.waf,
                    report::format_bytes(report.result.physical_bytes),
                    buckets(report.result.timeline.len())
                ),
            },
            Err(error) => failed(case.name, error),
        },
        Err(error) => failed(case.name, error),
    }
}

fn verify(expected: u64, report: &Report) -> Result<()> {
    let result = &report.result;
    if result.logical_bytes != expected {
        bail!(
            "logical_bytes is {}, expected {expected}",
            result.logical_bytes
        );
    }
    if result.physical_active_bytes + result.physical_settle_bytes != result.physical_bytes {
        bail!(
            "active ({}) + settle ({}) != physical ({})",
            result.physical_active_bytes,
            result.physical_settle_bytes,
            result.physical_bytes
        );
    }
    if result.physical_bytes == 0 {
        bail!("physical_bytes is zero");
    }
    if result.block_writes == 0 {
        bail!("block_writes is zero");
    }
    if !result.waf.is_finite() || result.waf <= 0.0 {
        bail!("waf is not a positive finite number: {}", result.waf);
    }
    let timeline: u64 = result
        .timeline
        .iter()
        .try_fold(0u64, |sum, bucket| sum.checked_add(bucket.physical_bytes))
        .context("timeline byte count overflows u64")?;
    if timeline != result.physical_bytes {
        bail!(
            "timeline sums to {timeline}, physical_bytes is {}",
            result.physical_bytes
        );
    }
    if report.run_dir.is_some() {
        bail!("run directory was not removed without --keep");
    }
    Ok(())
}

fn keep_case(opts: &GlobalOpts) -> Check {
    let name = "cleanup with --keep";
    let workload = Workload::Seq {
        size: 4 * MIB,
        block_size: MIB,
        sync: SyncMode::File,
        direct: false,
        preallocate: false,
        pattern: Pattern::Zero,
        threads: 1,
    };
    let run_opts = GlobalOpts {
        path: opts.path.clone(),
        json: false,
        keep: true,
        baseline: opts.baseline.min(WINDOW),
        settle: opts.settle.min(WINDOW),
        sample_interval: opts.sample_interval,
    };
    let report = match run::run_measurement(run_opts, workload) {
        Ok(report) => report,
        Err(error) => return failed(name, error),
    };
    let Some(run_dir) = report.run_dir.clone() else {
        return Check {
            name,
            ok: false,
            detail: "run_dir was not reported".to_string(),
        };
    };
    let data_file = Path::new(&run_dir).join("data.bin");
    let retained = data_file.is_file();
    if !retained {
        return Check {
            name,
            ok: false,
            detail: format!("{} is missing", data_file.display()),
        };
    }
    if let Err(error) = fs::remove_dir_all(&run_dir) {
        return failed(
            name,
            anyhow::Error::new(error).context(format!("cannot remove {run_dir}")),
        );
    }
    Check {
        name,
        ok: true,
        detail: "run_dir and data.bin retained, then removed".to_string(),
    }
}

fn leftover_case(target: &Path) -> Check {
    let name = "no leftover run dirs";
    let prefix = format!(".writescope-{}-", std::process::id());
    let mut leftovers = Vec::new();
    let entries = match fs::read_dir(target) {
        Ok(entries) => entries,
        Err(error) => return failed(name, error.into()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return failed(name, error.into()),
        };
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(&prefix) {
            leftovers.push(file_name.into_owned());
        }
    }
    if leftovers.is_empty() {
        Check {
            name,
            ok: true,
            detail: "all run directories removed".to_string(),
        }
    } else {
        Check {
            name,
            ok: false,
            detail: format!("left over: {}", leftovers.join(", ")),
        }
    }
}

fn buckets(count: usize) -> String {
    if count == 1 {
        "1 bucket".to_string()
    } else {
        format!("{count} buckets")
    }
}

fn failed(name: &'static str, error: anyhow::Error) -> Check {
    Check {
        name,
        ok: false,
        detail: format!("{error:#}"),
    }
}
