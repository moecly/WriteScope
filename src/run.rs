use crate::cli::{self, GlobalOpts, SyncMode, Workload};
use crate::device::{self, DeviceInfo, Target};
use crate::diskstats::{self, Counters};
use crate::report;
use crate::selftest;
use crate::workload;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SPACE_HEADROOM_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_ST_BLKSIZE: u64 = 4096;

pub fn dispatch(cli: cli::Cli) -> Result<()> {
    let cli::Validated { opts, action } = cli::validate(cli)?;
    match action {
        cli::Action::Measure(workload) => measure(opts, workload),
        cli::Action::Info => info(&opts),
        cli::Action::Selftest => selftest::run(&opts),
    }
}

pub fn measure(opts: GlobalOpts, workload: Workload) -> Result<()> {
    let json = opts.json;
    let report = run_measurement(opts, workload)?;
    report::emit_measurement(&report, json)
}

pub(crate) fn run_measurement(opts: GlobalOpts, workload: Workload) -> Result<report::Report> {
    let started_unix_ms = unix_ms();
    let target = device::resolve(&opts.path)?;
    check_space(&target.path, workload.logical_bytes()?)?;
    let run_dir = create_run_dir(&target.path)?;
    let mut guard = RunDirGuard::new(run_dir.clone(), opts.keep);

    let mut report = execute(&opts, &workload, &target, &run_dir, started_unix_ms)
        .map_err(|error| error.context(format!("run failed in {}", run_dir.display())))?;

    finalize_run_dir(&run_dir, opts.keep, &mut report);
    guard.disarm();
    Ok(report)
}

pub fn info(opts: &GlobalOpts) -> Result<()> {
    let target = device::resolve(&opts.path)?;
    let names: Vec<String> = target
        .devices
        .iter()
        .map(|device| device.name.clone())
        .collect();

    let start = Instant::now();
    let before = diskstats::read(&target.devices)?;
    thread::sleep(opts.baseline);
    let after = diskstats::read(&target.devices)?;
    let elapsed = start.elapsed();
    let delta = diskstats::delta(&before, &after, &names)?;
    let ambient_bytes_per_sec = if opts.baseline.is_zero() || elapsed < Duration::from_millis(1) {
        0.0
    } else {
        delta.physical_bytes as f64 / elapsed.as_secs_f64()
    };

    let counters = target
        .devices
        .iter()
        .zip(after)
        .map(|(device, counters)| report::DeviceCounters {
            name: device.name.clone(),
            major: device.major,
            minor: device.minor,
            sectors_written: counters.sectors_written,
            block_writes: counters.block_writes,
            flushes: counters.flushes,
        })
        .collect();

    let info = report::InfoReport {
        schema_version: report::SCHEMA_VERSION,
        tool: report::TOOL,
        version: env!("CARGO_PKG_VERSION"),
        target: report::TargetReport::from(&target),
        baseline_ms: elapsed.as_millis() as u64,
        ambient_bytes_per_sec,
        counters,
        warnings: target.warnings.clone(),
    };
    report::emit_info(&info, opts.json)
}

fn execute(
    opts: &GlobalOpts,
    workload: &Workload,
    target: &Target,
    run_dir: &Path,
    started_unix_ms: u64,
) -> Result<report::Report> {
    let prepared = prepare(workload, run_dir)?;
    let names: Vec<String> = target
        .devices
        .iter()
        .map(|device| device.name.clone())
        .collect();
    let logical_expected = workload.logical_bytes()?;

    let baseline_start_counters = diskstats::read(&target.devices)?;
    let baseline_start = Instant::now();
    thread::sleep(opts.baseline);
    let baseline_end_counters = diskstats::read(&target.devices)?;
    let baseline_elapsed = baseline_start.elapsed();
    let baseline_delta =
        diskstats::delta(&baseline_start_counters, &baseline_end_counters, &names)?;
    let ambient_bytes_per_sec =
        if opts.baseline.is_zero() || baseline_elapsed < Duration::from_millis(1) {
            0.0
        } else {
            baseline_delta.physical_bytes as f64 / baseline_elapsed.as_secs_f64()
        };

    let start_counters = diskstats::read(&target.devices)?;
    let active_start = Instant::now();
    let mut sampler = Some(Sampler::spawn(
        target.devices.clone(),
        opts.sample_interval,
        active_start,
    )?);
    let progress = Progress::start(
        !opts.json && std::io::stderr().is_terminal(),
        logical_expected,
    );
    let abort = AtomicBool::new(false);
    let write_result = workload::execute(
        workload,
        prepared.data_file.as_ref(),
        run_dir,
        prepared.align,
        progress.done(),
        &abort,
    );
    progress.finish();
    let logical_bytes = match write_result {
        Ok(bytes) => bytes,
        Err(error) => {
            if let Some(sampler) = sampler.take() {
                let _ = sampler.finish();
            }
            return Err(error.context(format!("workload failed in {}", run_dir.display())));
        }
    };
    if logical_bytes != logical_expected {
        bail!("internal error: wrote {logical_bytes} bytes, expected {logical_expected}");
    }

    final_sync(workload.sync(), &prepared)?;

    let post_sync_counters = diskstats::read(&target.devices)?;
    let active_ms = active_start.elapsed().as_millis() as u64;
    let post_sync_time = Instant::now();
    thread::sleep(opts.settle);
    let samples = match sampler.take() {
        Some(sampler) => sampler.finish()?,
        None => Vec::new(),
    };
    let end_counters = diskstats::read(&target.devices)?;
    let settle_ms = post_sync_time.elapsed().as_millis() as u64;

    let active = diskstats::delta(&start_counters, &post_sync_counters, &names)?;
    let settle = diskstats::delta(&post_sync_counters, &end_counters, &names)?;
    let total = diskstats::delta(&start_counters, &end_counters, &names)?;
    let physical_bytes = active
        .physical_bytes
        .checked_add(settle.physical_bytes)
        .context("physical byte counter overflow")?;
    let timeline = build_timeline(
        &start_counters,
        &samples,
        &end_counters,
        &names,
        active_ms + settle_ms,
    )?;

    let active_secs = active_ms as f64 / 1000.0;
    let logical_mib_per_sec = if active_secs < 1e-9 {
        0.0
    } else {
        logical_bytes as f64 / active_secs / (1024.0 * 1024.0)
    };
    let waf = report::compute_waf(physical_bytes, logical_bytes);
    let ambient_ratio =
        report::compute_ambient_ratio(ambient_bytes_per_sec, active_ms, settle_ms, physical_bytes);

    let mut warnings = target.warnings.clone();
    if waf < 1.0 {
        warnings.push(
            "physical bytes < logical bytes; compression or deduplication can cause WAF < 1"
                .to_string(),
        );
    }
    if ambient_ratio > 0.05 {
        warnings.push(format!(
            "ambient I/O is {:.1}% of measured physical traffic",
            ambient_ratio * 100.0
        ));
    }
    if physical_bytes == 0 {
        warnings.push("no physical write traffic was measured".to_string());
    }

    Ok(report::Report {
        schema_version: report::SCHEMA_VERSION,
        tool: report::TOOL,
        version: env!("CARGO_PKG_VERSION"),
        started_unix_ms,
        target: report::TargetReport::from(target),
        workload: report::WorkloadReport::from(workload),
        result: report::ResultReport {
            logical_bytes,
            physical_bytes,
            physical_active_bytes: active.physical_bytes,
            physical_settle_bytes: settle.physical_bytes,
            waf,
            active_ms,
            settle_ms,
            logical_mib_per_sec,
            block_writes: total.block_writes,
            flushes: total.flushes,
            discarded_bytes: total.discarded_bytes,
            ambient_bytes_per_sec,
            ambient_ratio,
            timeline,
        },
        run_dir: None,
        warnings,
    })
}

struct Prepared {
    data_file: Option<Arc<File>>,
    dir: File,
    align: usize,
}

fn prepare(workload: &Workload, run_dir: &Path) -> Result<Prepared> {
    let dir = File::open(run_dir)
        .with_context(|| format!("cannot open run directory {}", run_dir.display()))?;
    match workload {
        Workload::Seq {
            size,
            block_size,
            direct,
            preallocate,
            ..
        } => {
            let file = open_data_file(run_dir, *direct)?;
            let blksize = st_blksize(&file);
            if *direct {
                check_direct(*block_size, *size, blksize)?;
            }
            if *preallocate {
                fallocate(&file, *size)?;
            }
            file.sync_all()
                .context("fsync of the data file failed during preparation")?;
            dir.sync_all()
                .context("fsync of the run directory failed during preparation")?;
            Ok(Prepared {
                data_file: Some(Arc::new(file)),
                dir,
                align: align_for(blksize),
            })
        }
        Workload::Rand {
            file_size,
            block_size,
            direct,
            prefill,
            pattern,
            seed,
            ..
        } => {
            let file = open_data_file(run_dir, *direct)?;
            let blksize = st_blksize(&file);
            if *direct {
                check_direct(*block_size, *file_size, blksize)?;
            }
            file.set_len(*file_size)
                .context("cannot set the data file length")?;
            if *prefill {
                workload::prefill_file(
                    &file,
                    *file_size,
                    *block_size,
                    *pattern,
                    *seed,
                    align_for(blksize),
                    *direct,
                )
                .context("prefill failed")?;
            }
            file.sync_all()
                .context("fsync of the data file failed during preparation")?;
            dir.sync_all()
                .context("fsync of the run directory failed during preparation")?;
            Ok(Prepared {
                data_file: Some(Arc::new(file)),
                dir,
                align: align_for(blksize),
            })
        }
        Workload::Files { .. } => {
            dir.sync_all()
                .context("fsync of the run directory failed during preparation")?;
            Ok(Prepared {
                data_file: None,
                dir,
                align: DEFAULT_ST_BLKSIZE as usize,
            })
        }
    }
}

fn open_data_file(run_dir: &Path, direct: bool) -> Result<File> {
    let path = run_dir.join("data.bin");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    if direct {
        options.custom_flags(libc::O_DIRECT);
    }
    options.open(&path).with_context(|| {
        if direct {
            format!(
                "cannot create {} with O_DIRECT; O_DIRECT is not supported or alignment is invalid on this filesystem",
                path.display()
            )
        } else {
            format!("cannot create {}", path.display())
        }
    })
}

fn st_blksize(file: &File) -> u64 {
    let value = file.metadata().map(|meta| meta.blksize()).unwrap_or(0);
    if value == 0 {
        DEFAULT_ST_BLKSIZE
    } else {
        value
    }
}

fn align_for(blksize: u64) -> usize {
    (blksize.next_power_of_two()).max(DEFAULT_ST_BLKSIZE) as usize
}

fn check_direct(block_size: u64, total: u64, blksize: u64) -> Result<()> {
    if block_size % blksize != 0 {
        bail!(
            "--direct requires --block-size ({block_size}) to be a multiple of the filesystem block size ({blksize})"
        );
    }
    if total % blksize != 0 {
        bail!(
            "--direct requires the data size ({total}) to be a multiple of the filesystem block size ({blksize})"
        );
    }
    Ok(())
}

fn fallocate(file: &File, size: u64) -> Result<()> {
    let result = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, size as i64) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("fallocate failed; --preallocate is not supported by this filesystem");
    }
    Ok(())
}

fn final_sync(sync: SyncMode, prepared: &Prepared) -> Result<()> {
    match sync {
        SyncMode::File => {
            if let Some(file) = &prepared.data_file {
                file.sync_all()
                    .context("final fsync of the data file failed")?;
            }
            prepared
                .dir
                .sync_all()
                .context("final fsync of the run directory failed")?;
        }
        SyncMode::Write => {
            prepared
                .dir
                .sync_all()
                .context("final fsync of the run directory failed")?;
        }
        SyncMode::Fs => {
            let result = unsafe { libc::syncfs(prepared.dir.as_raw_fd()) };
            if result != 0 {
                return Err(std::io::Error::last_os_error()).context("syncfs failed");
            }
        }
    }
    Ok(())
}

fn build_timeline(
    start: &[Counters],
    samples: &[(u64, Vec<Counters>)],
    end: &[Counters],
    names: &[String],
    end_ms: u64,
) -> Result<Vec<report::TimelineBucket>> {
    let mut buckets: BTreeMap<u64, report::TimelineBucket> = BTreeMap::new();
    let mut previous = start.to_vec();
    for (elapsed_ms, counters) in samples {
        let delta = diskstats::delta(&previous, counters, names)?;
        add_bucket(&mut buckets, (elapsed_ms / 1000) * 1000, &delta)?;
        previous = counters.clone();
    }
    let delta = diskstats::delta(&previous, end, names)?;
    add_bucket(&mut buckets, (end_ms / 1000) * 1000, &delta)?;
    Ok(buckets.into_values().collect())
}

fn add_bucket(
    buckets: &mut BTreeMap<u64, report::TimelineBucket>,
    offset_ms: u64,
    delta: &diskstats::Delta,
) -> Result<()> {
    let bucket = buckets.entry(offset_ms).or_insert(report::TimelineBucket {
        offset_ms,
        physical_bytes: 0,
        block_writes: 0,
    });
    bucket.physical_bytes = bucket
        .physical_bytes
        .checked_add(delta.physical_bytes)
        .context("timeline byte counter overflow")?;
    bucket.block_writes = bucket
        .block_writes
        .checked_add(delta.block_writes)
        .context("timeline write counter overflow")?;
    Ok(())
}

type Samples = Vec<(u64, Vec<Counters>)>;

struct Sampler {
    stop: mpsc::Sender<()>,
    handle: thread::JoinHandle<Result<Samples>>,
}

impl Sampler {
    fn spawn(devices: Vec<DeviceInfo>, interval: Duration, epoch: Instant) -> Result<Self> {
        let (stop, stop_rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || -> Result<Samples> {
            let mut samples = Vec::new();
            loop {
                match stop_rx.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        let counters = diskstats::read(&devices)?;
                        samples.push((epoch.elapsed().as_millis() as u64, counters));
                    }
                }
            }
            Ok(samples)
        });
        Ok(Self { stop, handle })
    }

    fn finish(self) -> Result<Samples> {
        let _ = self.stop.send(());
        self.handle
            .join()
            .map_err(|_| anyhow!("sampling thread panicked"))?
    }
}

struct Progress {
    stop: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
    done: Arc<AtomicU64>,
}

impl Progress {
    fn start(enabled: bool, total: u64) -> Self {
        let done = Arc::new(AtomicU64::new(0));
        if !enabled {
            return Self {
                stop: None,
                handle: None,
                done,
            };
        }
        let (stop, stop_rx) = mpsc::channel::<()>();
        let counter = Arc::clone(&done);
        let handle = thread::spawn(move || {
            let mut printed = false;
            loop {
                match stop_rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        let current = counter.load(Ordering::Relaxed);
                        let percent = if total == 0 {
                            100.0
                        } else {
                            current as f64 / total as f64 * 100.0
                        };
                        eprint!(
                            "\rprogress    {}/{} ({:.1}%)",
                            report::format_bytes(current),
                            report::format_bytes(total),
                            percent
                        );
                        let _ = std::io::stderr().flush();
                        printed = true;
                    }
                }
            }
            if printed {
                eprintln!();
            }
        });
        Self {
            stop: Some(stop),
            handle: Some(handle),
            done,
        }
    }

    fn done(&self) -> &AtomicU64 {
        &self.done
    }

    fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct RunDirGuard {
    path: PathBuf,
    keep: bool,
    disarm: bool,
}

impl RunDirGuard {
    fn new(path: PathBuf, keep: bool) -> Self {
        Self {
            path,
            keep,
            disarm: false,
        }
    }

    fn disarm(&mut self) {
        self.disarm = true;
    }
}

impl Drop for RunDirGuard {
    fn drop(&mut self) {
        if !self.disarm && !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn create_run_dir(target: &Path) -> Result<PathBuf> {
    let name = format!(".writescope-{}-{}", std::process::id(), unix_ms());
    let run_dir = target.join(name);
    fs::create_dir(&run_dir)
        .with_context(|| format!("cannot create run directory {}", run_dir.display()))?;
    Ok(run_dir)
}

fn finalize_run_dir(run_dir: &Path, keep: bool, report: &mut report::Report) {
    if keep {
        report.run_dir = Some(run_dir.display().to_string());
        return;
    }
    if let Err(error) = fs::remove_dir_all(run_dir) {
        report.run_dir = Some(run_dir.display().to_string());
        report.warnings.push(format!(
            "cannot remove run directory {}: {error}",
            run_dir.display()
        ));
    }
}

fn check_space(path: &Path, logical_bytes: u64) -> Result<()> {
    let required = logical_bytes
        .checked_add(SPACE_HEADROOM_BYTES)
        .context("required space overflow")?;
    let available = available_bytes(path)?;
    if available < required {
        bail!(
            "not enough free space in {}: need {} ({} data + {} headroom), available {}",
            path.display(),
            report::format_bytes(required),
            report::format_bytes(logical_bytes),
            report::format_bytes(SPACE_HEADROOM_BYTES),
            report::format_bytes(available)
        );
    }
    Ok(())
}

fn available_bytes(path: &Path) -> Result<u64> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains a NUL byte: {}", path.display()))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs failed for {}", path.display()));
    }
    (stat.f_bavail as u64)
        .checked_mul(stat.f_frsize as u64)
        .context("available space overflow")
}
