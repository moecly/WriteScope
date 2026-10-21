use crate::cli::{Pattern, SyncMode, Workload};
use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

pub const SEED_SEQ: u64 = 42;

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
const MAX_CHUNK_BYTES: u64 = 1024 * 1024;

pub fn block_ranges(total_blocks: u64, threads: usize) -> Vec<(u64, u64)> {
    let base = total_blocks / threads as u64;
    let remainder = total_blocks % threads as u64;
    let mut ranges = Vec::with_capacity(threads);
    let mut start = 0u64;
    for index in 0..threads {
        let len = base + u64::from((index as u64) < remainder);
        ranges.push((start, len));
        start += len;
    }
    ranges
}

fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(GOLDEN);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn next(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value >> 12;
    value ^= value << 25;
    value ^= value >> 27;
    *state = value;
    value.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

pub fn seed_for(seed: u64, tag: u64) -> u64 {
    let value = mix(seed ^ tag.wrapping_mul(GOLDEN));
    if value == 0 {
        GOLDEN
    } else {
        value
    }
}

pub fn fill_pattern(buffer: &mut [u8], pattern: Pattern, state: &mut u64) {
    match pattern {
        Pattern::Zero => buffer.fill(0),
        Pattern::Random => {
            let mut chunks = buffer.chunks_exact_mut(8);
            for chunk in &mut chunks {
                chunk.copy_from_slice(&next(state).to_le_bytes());
            }
            let remainder = chunks.into_remainder();
            if !remainder.is_empty() {
                let value = next(state).to_le_bytes();
                let len = remainder.len();
                remainder.copy_from_slice(&value[..len]);
            }
        }
    }
}

pub fn shuffle_indices(len: usize, state: &mut u64) -> Vec<u32> {
    let mut indices: Vec<u32> = (0..len as u32).collect();
    for index in (1..len).rev() {
        let swap_with = (next(state) % (index as u64 + 1)) as usize;
        indices.swap(index, swap_with);
    }
    indices
}

pub struct AlignedBuffer {
    data: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuffer {
    pub fn new(len: usize, align: usize) -> Self {
        let align = align.max(1);
        let data = vec![0u8; len + align];
        let offset = data.as_ptr().align_offset(align);
        assert!(offset != usize::MAX, "cannot align a {align}-byte buffer");
        Self { data, offset, len }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data[self.offset..self.offset + self.len]
    }
}

fn write_context(error: std::io::Error, direct: bool) -> anyhow::Error {
    if direct && error.raw_os_error() == Some(libc::EINVAL) {
        anyhow::Error::new(error)
            .context("O_DIRECT is not supported or alignment is invalid on this filesystem")
    } else {
        anyhow::Error::new(error)
    }
}

fn write_block(file: &File, buffer: &[u8], offset: u64, direct: bool) -> Result<()> {
    file.write_all_at(buffer, offset)
        .map_err(|error| write_context(error, direct))
}

pub fn prefill_file(
    file: &File,
    size: u64,
    block_size: u64,
    pattern: Pattern,
    seed: u64,
    align: usize,
    direct: bool,
) -> Result<()> {
    let mut buffer = AlignedBuffer::new(block_size as usize, align);
    let mut state = seed_for(seed, 0);
    let mut offset = 0u64;
    while offset < size {
        fill_pattern(buffer.as_mut_slice(), pattern, &mut state);
        write_block(file, buffer.as_mut_slice(), offset, direct)?;
        offset += block_size;
    }
    Ok(())
}

pub fn execute(
    workload: &Workload,
    file: Option<&Arc<File>>,
    run_dir: &Path,
    align: usize,
    progress: &AtomicU64,
    abort: &AtomicBool,
) -> Result<u64> {
    match workload {
        Workload::Seq {
            size,
            block_size,
            sync,
            direct,
            preallocate: _,
            pattern,
            threads,
        } => {
            let file = file
                .ok_or_else(|| anyhow!("internal error: sequential workload has no data file"))?
                .clone();
            let total_blocks = size / block_size;
            let ranges = block_ranges(total_blocks, *threads);
            let block = *block_size;
            let sync = *sync;
            let direct = *direct;
            let pattern = *pattern;
            let writes = thread::scope(|scope| {
                let handles: Vec<_> = ranges
                    .into_iter()
                    .enumerate()
                    .map(|(index, (start, len))| {
                        let file = Arc::clone(&file);
                        scope.spawn(move || -> Result<u64> {
                            let mut buffer = AlignedBuffer::new(block as usize, align);
                            let mut state = seed_for(SEED_SEQ, index as u64 + 1);
                            let mut written = 0u64;
                            for block_index in start..start + len {
                                if abort.load(Ordering::Relaxed) {
                                    break;
                                }
                                fill_pattern(buffer.as_mut_slice(), pattern, &mut state);
                                write_block(
                                    &file,
                                    buffer.as_mut_slice(),
                                    block_index * block,
                                    direct,
                                )
                                .with_context(|| format!("cannot write block {block_index}"))?;
                                if sync == SyncMode::Write {
                                    file.sync_all().context("fsync failed")?;
                                }
                                progress.fetch_add(block, Ordering::Relaxed);
                                written += block;
                            }
                            Ok(written)
                        })
                    })
                    .collect();
                join_workers(handles, abort)
            })?;
            Ok(writes)
        }
        Workload::Rand {
            file_size,
            block_size,
            sync,
            prefill: _,
            direct,
            pattern,
            threads,
            seed,
        } => {
            let file = file
                .ok_or_else(|| anyhow!("internal error: random workload has no data file"))?
                .clone();
            let total_blocks = file_size / block_size;
            let ranges = block_ranges(total_blocks, *threads);
            let block = *block_size;
            let sync = *sync;
            let direct = *direct;
            let pattern = *pattern;
            let seed = *seed;
            let writes = thread::scope(|scope| {
                let handles: Vec<_> = ranges
                    .into_iter()
                    .enumerate()
                    .map(|(index, (start, len))| {
                        let file = Arc::clone(&file);
                        scope.spawn(move || -> Result<u64> {
                            let mut state = seed_for(seed, index as u64 + 1);
                            let order = shuffle_indices(len as usize, &mut state);
                            let mut buffer = AlignedBuffer::new(block as usize, align);
                            let mut written = 0u64;
                            for relative in order {
                                if abort.load(Ordering::Relaxed) {
                                    break;
                                }
                                let block_index = start + u64::from(relative);
                                fill_pattern(buffer.as_mut_slice(), pattern, &mut state);
                                write_block(
                                    &file,
                                    buffer.as_mut_slice(),
                                    block_index * block,
                                    direct,
                                )
                                .with_context(|| format!("cannot write block {block_index}"))?;
                                if sync == SyncMode::Write {
                                    file.sync_all().context("fsync failed")?;
                                }
                                progress.fetch_add(block, Ordering::Relaxed);
                                written += block;
                            }
                            Ok(written)
                        })
                    })
                    .collect();
                join_workers(handles, abort)
            })?;
            Ok(writes)
        }
        Workload::Files {
            count,
            file_size,
            sync,
            pattern,
            threads,
        } => {
            let ranges = block_ranges(*count, *threads);
            let chunk = (*file_size).min(MAX_CHUNK_BYTES);
            let sync = *sync;
            let pattern = *pattern;
            let file_size = *file_size;
            let writes = thread::scope(|scope| {
                let handles: Vec<_> = ranges
                    .into_iter()
                    .map(|(start, len)| {
                        scope.spawn(move || -> Result<u64> {
                            let mut buffer = AlignedBuffer::new(chunk as usize, align);
                            let mut written = 0u64;
                            for file_index in start..start + len {
                                if abort.load(Ordering::Relaxed) {
                                    break;
                                }
                                let mut state = seed_for(SEED_SEQ, file_index + 1);
                                let path = run_dir.join(format!("f-{file_index:08}"));
                                let mut file = OpenOptions::new()
                                    .write(true)
                                    .create_new(true)
                                    .open(&path)
                                    .with_context(|| format!("cannot create {}", path.display()))?;
                                let mut remaining = file_size;
                                while remaining > 0 {
                                    let len = remaining.min(chunk) as usize;
                                    let slice = &mut buffer.as_mut_slice()[..len];
                                    fill_pattern(slice, pattern, &mut state);
                                    file.write_all(slice).with_context(|| {
                                        format!("cannot write {}", path.display())
                                    })?;
                                    if sync == SyncMode::Write {
                                        file.sync_all().with_context(|| {
                                            format!("fsync failed for {}", path.display())
                                        })?;
                                    }
                                    remaining -= len as u64;
                                }
                                if sync == SyncMode::File {
                                    file.sync_all().with_context(|| {
                                        format!("fsync failed for {}", path.display())
                                    })?;
                                }
                                progress.fetch_add(file_size, Ordering::Relaxed);
                                written += file_size;
                            }
                            Ok(written)
                        })
                    })
                    .collect();
                join_workers(handles, abort)
            })?;
            Ok(writes)
        }
    }
}

fn join_workers<'scope>(
    handles: Vec<thread::ScopedJoinHandle<'scope, Result<u64>>>,
    abort: &AtomicBool,
) -> Result<u64> {
    let mut total = 0u64;
    let mut first_error: Option<anyhow::Error> = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(written)) => {
                total = total
                    .checked_add(written)
                    .context("written byte counter overflow")?;
            }
            Ok(Err(error)) => {
                abort.store(true, Ordering::Relaxed);
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(_) => {
                abort.store(true, Ordering::Relaxed);
                if first_error.is_none() {
                    first_error = Some(anyhow!("worker thread panicked"));
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ranges_cover_everything() {
        assert_eq!(block_ranges(10, 3), vec![(0, 4), (4, 3), (7, 3)]);
        let ranges = block_ranges(10, 3);
        let total: u64 = ranges.iter().map(|(_, len)| len).sum();
        assert_eq!(total, 10);
        let mut cursor = 0;
        for (start, len) in ranges {
            assert_eq!(start, cursor);
            cursor += len;
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut state = seed_for(42, 1);
        let order = shuffle_indices(1000, &mut state);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..1000u32).collect::<Vec<_>>());
    }

    #[test]
    fn prng_is_reproducible() {
        let mut first = seed_for(42, 7);
        let mut second = seed_for(42, 7);
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        fill_pattern(&mut a, Pattern::Random, &mut first);
        fill_pattern(&mut b, Pattern::Random, &mut second);
        assert_eq!(a, b);
        assert_ne!(a, [0u8; 64]);
    }

    #[test]
    fn aligned_buffer_is_aligned() {
        let mut buffer = AlignedBuffer::new(4096, 4096);
        assert_eq!(buffer.as_mut_slice().as_ptr() as usize % 4096, 0);
        assert_eq!(buffer.as_mut_slice().len(), 4096);
    }
}
