use crate::device::DeviceInfo;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counters {
    pub block_writes: u64,
    pub sectors_written: u64,
    pub flushes: Option<u64>,
    pub sectors_discarded: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    pub block_writes: u64,
    pub sectors_written: u64,
    pub physical_bytes: u64,
    pub flushes: Option<u64>,
    pub discarded_bytes: Option<u64>,
}

pub fn read(devices: &[DeviceInfo]) -> Result<Vec<Counters>> {
    let content = fs::read_to_string("/proc/diskstats").context("cannot read /proc/diskstats")?;
    let table = parse(&content);
    devices
        .iter()
        .map(|device| {
            table
                .get(&(device.major, device.minor))
                .copied()
                .with_context(|| {
                    format!(
                        "device {} ({}:{}) is missing from /proc/diskstats",
                        device.name, device.major, device.minor
                    )
                })
        })
        .collect()
}

pub fn parse(content: &str) -> HashMap<(u32, u32), Counters> {
    let mut table = HashMap::new();
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 14 {
            continue;
        }
        let Ok(major) = fields[0].parse::<u32>() else {
            continue;
        };
        let Ok(minor) = fields[1].parse::<u32>() else {
            continue;
        };
        let block_writes = fields[7].parse::<u64>().unwrap_or(0);
        let sectors_written = fields[9].parse::<u64>().unwrap_or(0);
        let sectors_discarded = fields.get(16).and_then(|value| value.parse::<u64>().ok());
        let flushes = fields.get(18).and_then(|value| value.parse::<u64>().ok());
        table.insert(
            (major, minor),
            Counters {
                block_writes,
                sectors_written,
                flushes,
                sectors_discarded,
            },
        );
    }
    table
}

pub fn delta(start: &[Counters], end: &[Counters], names: &[String]) -> Result<Delta> {
    if start.len() != end.len() || start.len() != names.len() {
        bail!("internal error: mismatched device counter arrays");
    }
    let mut block_writes = 0u64;
    let mut sectors_written = 0u64;
    let mut flushes = Some(0u64);
    let mut sectors_discarded = Some(0u64);

    for (index, (before, after)) in start.iter().zip(end).enumerate() {
        let name = &names[index];
        let writes = after
            .block_writes
            .checked_sub(before.block_writes)
            .with_context(|| format!("device counter reset during run: {name}"))?;
        let sectors = after
            .sectors_written
            .checked_sub(before.sectors_written)
            .with_context(|| format!("device counter reset during run: {name}"))?;
        block_writes = block_writes
            .checked_add(writes)
            .context("block write counter overflow")?;
        sectors_written = sectors_written
            .checked_add(sectors)
            .context("sector counter overflow")?;

        flushes = match (flushes, before.flushes, after.flushes) {
            (Some(total), Some(before), Some(after)) => Some(
                total
                    .checked_add(
                        after
                            .checked_sub(before)
                            .with_context(|| format!("device counter reset during run: {name}"))?,
                    )
                    .context("flush counter overflow")?,
            ),
            _ => None,
        };
        sectors_discarded = match (
            sectors_discarded,
            before.sectors_discarded,
            after.sectors_discarded,
        ) {
            (Some(total), Some(before), Some(after)) => Some(
                total
                    .checked_add(
                        after
                            .checked_sub(before)
                            .with_context(|| format!("device counter reset during run: {name}"))?,
                    )
                    .context("discard counter overflow")?,
            ),
            _ => None,
        };
    }

    Ok(Delta {
        block_writes,
        sectors_written,
        physical_bytes: sectors_written
            .checked_mul(512)
            .context("physical byte counter overflow")?,
        flushes,
        discarded_bytes: sectors_discarded.and_then(|sectors| sectors.checked_mul(512)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODERN: &str = "   8       0 sda 100 0 2000 5 50 0 800 6 0 0 0 0 0 4 0 7 0\n\
   8      17 sdb1 200 0 4000 5 70 0 1600 6 0 0 0 0 0 32 0 9 0\n";

    const OLD: &str = "   8       0 sda 100 0 2000 0 50 0 800 0 0 0 0 0\n";

    #[test]
    fn parses_modern_fields() {
        let table = parse(MODERN);
        let counters = table[&(8, 17)];
        assert_eq!(counters.block_writes, 70);
        assert_eq!(counters.sectors_written, 1600);
        assert_eq!(counters.sectors_discarded, Some(32));
        assert_eq!(counters.flushes, Some(9));
        let counters = table[&(8, 0)];
        assert_eq!(counters.sectors_discarded, Some(4));
        assert_eq!(counters.flushes, Some(7));
    }

    #[test]
    fn old_kernel_lines_miss_flush_and_discard() {
        let table = parse(OLD);
        let counters = table[&(8, 0)];
        assert_eq!(counters.block_writes, 50);
        assert_eq!(counters.sectors_written, 800);
        assert_eq!(counters.sectors_discarded, None);
        assert_eq!(counters.flushes, None);
    }

    #[test]
    fn sums_multiple_devices() {
        let table = parse(MODERN);
        let start = vec![table[&(8, 0)], table[&(8, 17)]];
        let end = vec![
            Counters {
                block_writes: 60,
                sectors_written: 900,
                flushes: Some(9),
                sectors_discarded: Some(16),
            },
            Counters {
                block_writes: 90,
                sectors_written: 1700,
                flushes: Some(12),
                sectors_discarded: Some(40),
            },
        ];
        let delta = delta(&start, &end, &["sda".into(), "sdb1".into()]).unwrap();
        assert_eq!(delta.block_writes, 30);
        assert_eq!(delta.sectors_written, 200);
        assert_eq!(delta.physical_bytes, 200 * 512);
        assert_eq!(delta.flushes, Some(5));
        assert_eq!(delta.discarded_bytes, Some(20 * 512));
    }

    #[test]
    fn counter_reset_is_an_error() {
        let names = vec!["sda".to_string()];
        let start = vec![Counters {
            block_writes: 10,
            sectors_written: 10,
            flushes: Some(1),
            sectors_discarded: Some(0),
        }];
        let end = vec![Counters {
            block_writes: 9,
            sectors_written: 10,
            flushes: Some(2),
            sectors_discarded: Some(0),
        }];
        let error = delta(&start, &end, &names).unwrap_err().to_string();
        assert!(error.contains("counter reset"));
    }
}
