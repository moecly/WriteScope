use crate::diskstats;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

const UNSUPPORTED_LEAF_PREFIXES: [&str; 6] = ["loop", "zram", "nbd", "ram", "sr", "fd"];
const MAX_DEVICE_DEPTH: usize = 8;

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub major: u32,
    pub minor: u32,
    pub logical_block_size: u32,
    pub rotational: bool,
}

#[derive(Debug)]
pub struct Target {
    pub path: PathBuf,
    pub filesystem: String,
    pub mount_point: PathBuf,
    pub mount_source: String,
    pub mount_options: String,
    pub super_options: String,
    pub devices: Vec<DeviceInfo>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub mount_options: String,
    pub fstype: String,
    pub source: PathBuf,
    pub super_options: String,
}

pub fn resolve(path: &Path) -> Result<Target> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("cannot resolve target path {}", path.display()))?;
    if !fs::metadata(&canonical)?.is_dir() {
        bail!("target path is not a directory: {}", canonical.display());
    }

    let content =
        fs::read_to_string("/proc/self/mountinfo").context("cannot read /proc/self/mountinfo")?;
    let entries = parse_mountinfo(&content);
    let entry = select_mount(&entries, &canonical).with_context(|| {
        format!(
            "no mount point in /proc/self/mountinfo matches {}",
            canonical.display()
        )
    })?;

    let source_display = entry.source.display();
    if !entry.source.starts_with("/dev/") {
        bail!(
            "cannot measure {} mounted from {} at {}: the mount source is not a block device (tmpfs, overlay and network filesystems are not measurable)",
            entry.fstype,
            source_display,
            entry.mount_point.display()
        );
    }

    let source_path = fs::canonicalize(&entry.source).with_context(|| {
        format!(
            "cannot resolve mount source {} for {}",
            source_display,
            entry.mount_point.display()
        )
    })?;
    let source_meta = fs::metadata(&source_path)
        .with_context(|| format!("cannot stat mount source {}", source_path.display()))?;
    if !source_meta.file_type().is_block_device() {
        bail!(
            "mount source {} is not a block device ({} at {})",
            source_path.display(),
            entry.fstype,
            entry.mount_point.display()
        );
    }

    let initial_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("cannot derive device name from {}", source_path.display()))?
        .to_string();

    let mut leaves = Vec::new();
    let mut visited = HashSet::new();
    expand(&initial_name, 0, &mut leaves, &mut visited)?;

    let mut devices: Vec<DeviceInfo> = Vec::new();
    for leaf in leaves {
        if devices
            .iter()
            .any(|device| device.major == leaf.major && device.minor == leaf.minor)
        {
            continue;
        }
        devices.push(leaf);
    }

    diskstats::read(&devices)?;

    Ok(Target {
        path: canonical,
        filesystem: entry.fstype.clone(),
        mount_point: entry.mount_point.clone(),
        mount_source: entry.source.display().to_string(),
        mount_options: entry.mount_options.clone(),
        super_options: entry.super_options.clone(),
        devices,
        warnings: btrfs_warnings(&entry.fstype, &initial_name),
    })
}

fn expand(
    name: &str,
    depth: usize,
    out: &mut Vec<DeviceInfo>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if depth > MAX_DEVICE_DEPTH {
        bail!(
            "device hierarchy under {} is deeper than {MAX_DEVICE_DEPTH} levels",
            name
        );
    }
    if !visited.insert(name.to_string()) {
        return Ok(());
    }

    let slaves = list_slaves(name)?;
    if slaves.is_empty() {
        if let Some(prefix) = UNSUPPORTED_LEAF_PREFIXES
            .iter()
            .find(|prefix| name.starts_with(*prefix))
        {
            bail!(
                "cannot measure device {name} ({prefix} device): backing file or physical device carries no block counters"
            );
        }
        if name.starts_with("dm-") || name.starts_with("md") {
            bail!("cannot measure device {name}: no underlying physical device resolved");
        }
        out.push(device_info(name)?);
        return Ok(());
    }

    for slave in slaves {
        expand(&slave, depth + 1, out, visited)?;
    }
    Ok(())
}

fn list_slaves(name: &str) -> Result<Vec<String>> {
    let path = format!("/sys/class/block/{name}/slaves");
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot list slaves of {name} ({path})"))
        }
    };
    let mut slaves = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {path}"))?;
        if let Some(slave) = entry.file_name().to_str() {
            slaves.push(slave.to_string());
        }
    }
    slaves.sort();
    Ok(slaves)
}

fn device_info(name: &str) -> Result<DeviceInfo> {
    let dev = fs::read_to_string(format!("/sys/class/block/{name}/dev"))
        .with_context(|| format!("cannot read /sys/class/block/{name}/dev"))?;
    let (major, minor) = parse_dev_number(dev.trim())
        .with_context(|| format!("cannot parse device number {dev:?} for {name}"))?;
    let logical_block_size = queue_value(name, "logical_block_size")
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(512);
    let rotational = queue_value(name, "rotational")
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    Ok(DeviceInfo {
        name: name.to_string(),
        major,
        minor,
        logical_block_size,
        rotational,
    })
}

fn queue_value(name: &str, key: &str) -> Option<String> {
    if let Ok(value) = fs::read_to_string(format!("/sys/class/block/{name}/queue/{key}")) {
        return Some(value);
    }
    let parent = queue_owner(name);
    if parent == name {
        return None;
    }
    fs::read_to_string(format!("/sys/class/block/{parent}/queue/{key}")).ok()
}

fn queue_owner(name: &str) -> String {
    let link = format!("/sys/class/block/{name}");
    let Ok(path) = fs::canonicalize(&link) else {
        return name.to_string();
    };
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|parent| parent.to_str())
        .unwrap_or(name)
        .to_string()
}

fn parse_dev_number(value: &str) -> Result<(u32, u32)> {
    let (major, minor) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("expected major:minor, got {value:?}"))?;
    Ok((major.trim().parse()?, minor.trim().parse()?))
}

fn btrfs_warnings(fstype: &str, initial_name: &str) -> Vec<String> {
    if fstype != "btrfs" {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir("/sys/fs/btrfs") else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        let devices_dir = entry.path().join("devices");
        if !devices_dir.join(initial_name).exists() {
            continue;
        }
        let count = match fs::read_dir(&devices_dir) {
            Ok(devices) => devices.flatten().count(),
            Err(_) => continue,
        };
        if count > 1 {
            warnings.push(format!(
                "btrfs spans {count} devices, but only the resolved leaf devices are counted"
            ));
        }
    }
    warnings
}

pub fn parse_mountinfo(content: &str) -> Vec<MountEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if separator < 6 || fields.len() < separator + 4 {
            continue;
        }
        entries.push(MountEntry {
            mount_point: PathBuf::from(unescape(fields[4])),
            mount_options: unescape(fields[5]),
            fstype: unescape(fields[separator + 1]),
            source: PathBuf::from(unescape(fields[separator + 2])),
            super_options: unescape(fields[separator + 3]),
        });
    }
    entries
}

pub fn select_mount<'a>(entries: &'a [MountEntry], target: &Path) -> Option<&'a MountEntry> {
    let mut best: Option<&MountEntry> = None;
    for entry in entries {
        if !target.starts_with(&entry.mount_point) {
            continue;
        }
        let depth = entry.mount_point.components().count();
        let better = match best {
            None => true,
            Some(current) => depth >= current.mount_point.components().count(),
        };
        if better {
            best = Some(entry);
        }
    }
    best
}

pub fn unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &bytes[index + 1..index + 4];
            if digits.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                let code = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
                out.push(code);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(line: &str) -> MountEntry {
        parse_mountinfo(line).remove(0)
    }

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape("/mnt/with\\040space"), "/mnt/with space");
        assert_eq!(unescape("a\\134b"), "a\\b");
        assert_eq!(unescape("plain"), "plain");
    }

    #[test]
    fn longest_mount_point_wins() {
        let entries = vec![
            mount("1 0 8:1 / / rw - ext4 /dev/sdb1 rw"),
            mount("2 0 8:17 / /mnt/hdd0 rw - ext4 /dev/sdb1 rw"),
            mount("3 0 8:18 / /mnt/hdd0/f rw - ext4 /dev/sdb2 rw"),
        ];
        let target = PathBuf::from("/mnt/hdd0/f/develop");
        let selected = select_mount(&entries, &target).unwrap();
        assert_eq!(selected.mount_point, PathBuf::from("/mnt/hdd0/f"));
        assert_eq!(selected.source, PathBuf::from("/dev/sdb2"));
    }

    #[test]
    fn later_entry_wins_on_tie() {
        let entries = vec![
            mount("1 0 8:1 / /mnt/data rw - ext4 /dev/sdb1 rw"),
            mount("2 0 8:2 / /mnt/data rw - ext4 /dev/sdb2 rw"),
        ];
        let selected = select_mount(&entries, Path::new("/mnt/data/x")).unwrap();
        assert_eq!(selected.source, PathBuf::from("/dev/sdb2"));
    }

    #[test]
    fn parses_mount_options_and_super_options() {
        let entry = mount("36 1 0:32 / /tmp rw,relatime shared:5 - btrfs /dev/mapper/root rw,ssd");
        assert_eq!(entry.fstype, "btrfs");
        assert_eq!(entry.source, PathBuf::from("/dev/mapper/root"));
        assert_eq!(entry.mount_options, "rw,relatime");
        assert_eq!(entry.super_options, "rw,ssd");
    }
}
