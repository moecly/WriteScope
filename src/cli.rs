use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

pub const MAX_THREADS: usize = 1024;
pub const MAX_RAND_BLOCKS: u64 = 16_777_216;
pub const MAX_BUFFER_BYTES: u64 = 512 * 1024 * 1024;
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(10);

fn parse_size_arg(value: &str) -> std::result::Result<u64, parse_size::Error> {
    parse_size::parse_size(value)
}

#[derive(Debug, Parser)]
#[command(
    name = "writescope",
    version,
    about = "Measure filesystem write amplification using Linux block-device counters",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Target directory
    #[arg(short, long, global = true, default_value = ".", value_name = "DIR")]
    pub path: PathBuf,
    /// Emit JSON to stdout
    #[arg(long, global = true)]
    pub json: bool,
    /// Keep the generated run directory
    #[arg(long, global = true)]
    pub keep: bool,
    /// Ambient sampling window
    #[arg(
        long,
        global = true,
        default_value = "1s",
        value_name = "DURATION",
        value_parser = humantime::parse_duration
    )]
    pub baseline: Duration,
    /// Post-sync observation window
    #[arg(
        long,
        global = true,
        default_value = "1s",
        value_name = "DURATION",
        value_parser = humantime::parse_duration
    )]
    pub settle: Duration,
    /// Timeline sampling interval
    #[arg(
        long,
        global = true,
        default_value = "100ms",
        value_name = "DURATION",
        value_parser = humantime::parse_duration
    )]
    pub sample_interval: Duration,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sequential write of one file
    Seq(SeqArgs),
    /// Random-order write of every block in one file, exactly once
    Rand(RandArgs),
    /// Create and write many small files
    Files(FilesArgs),
    /// Resolve target filesystem/device and show current counters
    Info,
    /// Run a built-in end-to-end self-test of every write workload
    #[command(alias = "self-test")]
    Selftest,
    /// Benchmark sequential, random, and small-file workloads
    #[command(alias = "benchmark")]
    Bench(BenchArgs),
}

#[derive(Debug, Args)]
pub struct SeqArgs {
    /// Total bytes to write
    #[arg(long, default_value = "1GiB", value_parser = parse_size_arg)]
    pub size: u64,
    /// Write block size
    #[arg(long, default_value = "1MiB", value_parser = parse_size_arg)]
    pub block_size: u64,
    /// Sync strategy
    #[arg(long, value_enum, default_value = "file")]
    pub sync: SyncMode,
    /// Open the data file with O_DIRECT
    #[arg(long)]
    pub direct: bool,
    /// Preallocate the file with fallocate
    #[arg(long)]
    pub preallocate: bool,
    /// Buffer fill pattern
    #[arg(long, value_enum, default_value = "random")]
    pub pattern: Pattern,
    /// Number of writer threads
    #[arg(long, default_value_t = 1)]
    pub threads: usize,
}

#[derive(Debug, Args)]
pub struct RandArgs {
    /// Size of the target file
    #[arg(long, default_value = "256MiB", value_parser = parse_size_arg)]
    pub file_size: u64,
    /// Write block size
    #[arg(long, default_value = "4KiB", value_parser = parse_size_arg)]
    pub block_size: u64,
    /// Sync strategy
    #[arg(long, value_enum, default_value = "file")]
    pub sync: SyncMode,
    /// Fill every block sequentially before measuring
    #[arg(long)]
    pub prefill: bool,
    /// Open the data file with O_DIRECT
    #[arg(long)]
    pub direct: bool,
    /// Buffer fill pattern
    #[arg(long, value_enum, default_value = "random")]
    pub pattern: Pattern,
    /// Number of writer threads
    #[arg(long, default_value_t = 1)]
    pub threads: usize,
    /// Shuffle seed
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
}

#[derive(Debug, Args)]
pub struct FilesArgs {
    /// Number of files to create
    #[arg(long, default_value_t = 10_000)]
    pub count: u64,
    /// Size of each file
    #[arg(long, default_value = "4KiB", value_parser = parse_size_arg)]
    pub file_size: u64,
    /// Sync strategy
    #[arg(long, value_enum, default_value = "file")]
    pub sync: SyncMode,
    /// Buffer fill pattern
    #[arg(long, value_enum, default_value = "random")]
    pub pattern: Pattern,
    /// Number of writer threads
    #[arg(long, default_value_t = 1)]
    pub threads: usize,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Sequential workload size per run
    #[arg(long, default_value = "1GiB", value_parser = parse_size_arg)]
    pub seq_size: u64,
    /// Random workload file size per run
    #[arg(long, default_value = "256MiB", value_parser = parse_size_arg)]
    pub rand_size: u64,
    /// Number of small files per run
    #[arg(long, default_value_t = 2_000)]
    pub files_count: u64,
    /// Runs per workload; the median is reported
    #[arg(long, default_value_t = 3)]
    pub repeats: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SyncMode {
    File,
    Write,
    Fs,
}

impl SyncMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncMode::File => "file",
            SyncMode::Write => "write",
            SyncMode::Fs => "fs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Pattern {
    Random,
    Zero,
}

impl Pattern {
    pub fn as_str(self) -> &'static str {
        match self {
            Pattern::Random => "random",
            Pattern::Zero => "zero",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Workload {
    Seq {
        size: u64,
        block_size: u64,
        sync: SyncMode,
        direct: bool,
        preallocate: bool,
        pattern: Pattern,
        threads: usize,
    },
    Rand {
        file_size: u64,
        block_size: u64,
        sync: SyncMode,
        prefill: bool,
        direct: bool,
        pattern: Pattern,
        threads: usize,
        seed: u64,
    },
    Files {
        count: u64,
        file_size: u64,
        sync: SyncMode,
        pattern: Pattern,
        threads: usize,
    },
}

impl Workload {
    pub fn logical_bytes(&self) -> Result<u64> {
        match self {
            Workload::Seq { size, .. } => Ok(*size),
            Workload::Rand { file_size, .. } => Ok(*file_size),
            Workload::Files {
                count, file_size, ..
            } => count
                .checked_mul(*file_size)
                .ok_or_else(|| anyhow::anyhow!("--count * --file-size overflows u64")),
        }
    }

    pub fn sync(&self) -> SyncMode {
        match self {
            Workload::Seq { sync, .. }
            | Workload::Rand { sync, .. }
            | Workload::Files { sync, .. } => *sync,
        }
    }
}

#[derive(Debug)]
pub struct GlobalOpts {
    pub path: PathBuf,
    pub json: bool,
    pub keep: bool,
    pub baseline: Duration,
    pub settle: Duration,
    pub sample_interval: Duration,
}

#[derive(Debug)]
pub struct BenchPlan {
    pub seq: Workload,
    pub rand: Workload,
    pub files: Workload,
    pub repeats: u32,
}

#[derive(Debug)]
pub enum Action {
    Info,
    Measure(Workload),
    Selftest,
    Bench(BenchPlan),
}

#[derive(Debug)]
pub struct Validated {
    pub opts: GlobalOpts,
    pub action: Action,
}

pub fn validate(cli: Cli) -> Result<Validated> {
    if !cli.path.exists() {
        bail!("target path does not exist: {}", cli.path.display());
    }
    if !std::fs::metadata(&cli.path)?.is_dir() {
        bail!("target path is not a directory: {}", cli.path.display());
    }
    if cli.sample_interval < MIN_SAMPLE_INTERVAL {
        bail!(
            "--sample-interval must be at least {}ms",
            MIN_SAMPLE_INTERVAL.as_millis()
        );
    }

    let action = match cli.command {
        Command::Info => Action::Info,
        Command::Seq(args) => Action::Measure(validate_seq(args)?),
        Command::Rand(args) => Action::Measure(validate_rand(args)?),
        Command::Files(args) => Action::Measure(validate_files(args)?),
        Command::Selftest => Action::Selftest,
        Command::Bench(args) => Action::Bench(validate_bench(args)?),
    };

    let opts = GlobalOpts {
        path: cli.path,
        json: cli.json,
        keep: cli.keep,
        baseline: cli.baseline,
        settle: cli.settle,
        sample_interval: cli.sample_interval,
    };
    Ok(Validated { opts, action })
}

fn check_size(name: &str, value: u64) -> Result<()> {
    if value == 0 {
        bail!("{name} must be greater than zero");
    }
    if value > i64::MAX as u64 {
        bail!("{name} exceeds i64::MAX");
    }
    Ok(())
}

fn check_threads(threads: usize, limit: u64) -> Result<()> {
    if threads == 0 || threads > MAX_THREADS {
        bail!("--threads must be between 1 and {MAX_THREADS}");
    }
    if threads as u64 > limit {
        bail!("--threads ({threads}) must not exceed the number of write units ({limit})");
    }
    Ok(())
}

fn check_buffer(block_size: u64, threads: usize) -> Result<()> {
    let total = block_size
        .checked_mul(threads as u64)
        .ok_or_else(|| anyhow::anyhow!("buffer size overflow"))?;
    if total > MAX_BUFFER_BYTES {
        bail!(
            "buffers would need {} MiB ({} x {} threads), exceeding the {} MiB limit",
            total / (1024 * 1024),
            block_size,
            threads,
            MAX_BUFFER_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

fn validate_seq(args: SeqArgs) -> Result<Workload> {
    check_size("--size", args.size)?;
    check_size("--block-size", args.block_size)?;
    if args.size % args.block_size != 0 {
        bail!(
            "--size ({}) must be a multiple of --block-size ({})",
            args.size,
            args.block_size
        );
    }
    let total_blocks = args.size / args.block_size;
    check_threads(args.threads, total_blocks)?;
    check_buffer(args.block_size, args.threads)?;
    Ok(Workload::Seq {
        size: args.size,
        block_size: args.block_size,
        sync: args.sync,
        direct: args.direct,
        preallocate: args.preallocate,
        pattern: args.pattern,
        threads: args.threads,
    })
}

fn validate_rand(args: RandArgs) -> Result<Workload> {
    check_size("--file-size", args.file_size)?;
    check_size("--block-size", args.block_size)?;
    if args.file_size % args.block_size != 0 {
        bail!(
            "--file-size ({}) must be a multiple of --block-size ({})",
            args.file_size,
            args.block_size
        );
    }
    let total_blocks = args.file_size / args.block_size;
    if total_blocks > MAX_RAND_BLOCKS {
        bail!(
            "--file-size / --block-size gives {total_blocks} blocks, exceeding the limit of {MAX_RAND_BLOCKS}; increase --block-size or reduce --file-size"
        );
    }
    check_threads(args.threads, total_blocks)?;
    check_buffer(args.block_size, args.threads)?;
    Ok(Workload::Rand {
        file_size: args.file_size,
        block_size: args.block_size,
        sync: args.sync,
        prefill: args.prefill,
        direct: args.direct,
        pattern: args.pattern,
        threads: args.threads,
        seed: args.seed,
    })
}

fn validate_files(args: FilesArgs) -> Result<Workload> {
    check_size("--count", args.count)?;
    check_size("--file-size", args.file_size)?;
    args.count
        .checked_mul(args.file_size)
        .ok_or_else(|| anyhow::anyhow!("--count * --file-size overflows u64"))?;
    check_threads(args.threads, args.count)?;
    check_buffer(args.file_size.min(1024 * 1024), args.threads)?;
    Ok(Workload::Files {
        count: args.count,
        file_size: args.file_size,
        sync: args.sync,
        pattern: args.pattern,
        threads: args.threads,
    })
}

fn validate_bench(args: BenchArgs) -> Result<BenchPlan> {
    const SEQ_BLOCK: u64 = 1024 * 1024;
    const BLOCK: u64 = 4096;
    const THREADS: usize = 4;

    if !(1..=9).contains(&args.repeats) {
        bail!("--repeats must be between 1 and 9");
    }

    let rand_threads = (args.rand_size / BLOCK).clamp(1, THREADS as u64) as usize;
    let files_threads = args.files_count.clamp(1, THREADS as u64) as usize;

    let seq = validate_seq(SeqArgs {
        size: args.seq_size,
        block_size: SEQ_BLOCK,
        sync: SyncMode::File,
        direct: false,
        preallocate: false,
        pattern: Pattern::Random,
        threads: 1,
    })?;
    let rand = validate_rand(RandArgs {
        file_size: args.rand_size,
        block_size: BLOCK,
        sync: SyncMode::File,
        prefill: true,
        direct: false,
        pattern: Pattern::Random,
        threads: rand_threads,
        seed: 42,
    })?;
    let files = validate_files(FilesArgs {
        count: args.files_count,
        file_size: BLOCK,
        sync: SyncMode::File,
        pattern: Pattern::Random,
        threads: files_threads,
    })?;

    Ok(BenchPlan {
        seq,
        rand,
        files,
        repeats: args.repeats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(count: u64, file_size: u64) -> FilesArgs {
        FilesArgs {
            count,
            file_size,
            sync: SyncMode::File,
            pattern: Pattern::Random,
            threads: 1,
        }
    }

    #[test]
    fn files_overflow_is_rejected() {
        assert!(validate_files(files(u64::MAX, 2)).is_err());
        assert!(validate_files(files(1, u64::MAX)).is_err());
    }

    #[test]
    fn seq_alignment_is_checked() {
        let args = SeqArgs {
            size: 1000,
            block_size: 512,
            sync: SyncMode::File,
            direct: false,
            preallocate: false,
            pattern: Pattern::Random,
            threads: 1,
        };
        assert!(validate_seq(args).is_err());
    }

    #[test]
    fn threads_limit_blocks() {
        let args = SeqArgs {
            size: 4096,
            block_size: 4096,
            sync: SyncMode::File,
            direct: false,
            preallocate: false,
            pattern: Pattern::Random,
            threads: 2,
        };
        assert!(validate_seq(args).is_err());
    }
}
