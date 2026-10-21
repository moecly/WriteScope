# writescope

在 Linux 上测量文件系统写放大（write amplification）的 CLI 工具。它运行内置的可控写负载，并读取 `/proc/diskstats` 中目标文件系统底层块设备的写计数，输出逻辑字节、物理字节与 WAF。

## 度量定义

- **logical_bytes**：应用实际写入的文件数据字节数。
  - `seq` 等于 `--size`；
  - `rand` 等于 `--file-size`；
  - `files` 等于 `--count * --file-size`。
  - 文件系统元数据不计入 logical。
- **physical_bytes**：测量窗口内目标文件系统底层块设备（分区或整盘，见下文）的 `sectors_written * 512` 增量。它包含文件系统数据、日志与元数据，不扣减 discard（TRIM）。`sectors` 固定按 512 字节换算，与设备逻辑块大小无关。
- **WAF** = `physical_bytes / logical_bytes`。
- **测量窗口**：负载开始 → 负载结束并完成最终 `fsync`/`syncfs` → 额外 settle 窗口结束。settle 用于捕获 ext4 journal checkpoint、btrfs delayed refs 等延迟写入。
- **ambient**：baseline 窗口内观测到的背景写速率，仅用于报告，不从结果中扣减。

## 构建

```bash
nix develop          # 或确保 rustc/cargo 在 PATH 中
just build           # cargo build --release
./target/release/writescope --help
```

## 一键自检

内置 `selftest` 子命令，无需脚本即可在目标目录验证工具链与各负载路径：

```bash
writescope --path /mnt/data selftest
# 等价别名
writescope --path /mnt/data self-test
```

它会依次实际执行 `seq`、`seq --direct --preallocate --sync write`、`rand --threads 4`、`rand --prefill --direct`、`files --threads 4`、`files --sync fs`、`--keep` 清理检查，并校验每次结果的 logical 字节、active+settle、timeline 总和、块写入计数、WAF 有效性及运行目录清理。输出逐项 `PASS`/`FAIL`，任何一项失败时退出码为 1。

自检的 baseline/settle 取 `min(用户值, 100ms)` 以缩短耗时，且始终清理运行目录（忽略 `--keep`）；目标目录需约 100 MiB 可用空间，实际耗时取决于磁盘（本项目测试机 ext4 HDD 约 1–2 分钟，btrfs SSD 约 5 秒）。`--json` 输出结构化结果：

```json
{
  "schema_version": 1,
  "tool": "writescope",
  "version": "0.1.0",
  "target": {"path": "...", "filesystem": "ext4", "devices": [{"name": "sdb1"}]},
  "baseline_ms": 100,
  "settle_ms": 100,
  "checks": [{"name": "seq 64MiB", "ok": true, "detail": "WAF 1.012x, physical 64.8 MiB, 3 buckets"}],
  "passed": 8,
  "failed": 0
}
```

开发期可用 `just verify`（fmt/clippy + 单元测试 + 自检）或 `just smoke`（仅自检）：

```bash
just verify
just smoke /mnt/data
```

## 发布

GitHub Actions（`.github/workflows/release.yml`）在推送 `v*` 标签时构建并发布：

```bash
git tag v0.1.0        # 必须与 Cargo.toml 的 version 一致
git push origin v0.1.0
```

也可以在仓库 Actions → `release` → Run workflow 手动触发：工作流读取 `Cargo.toml` 的 version，自动创建 `v<version>` 标签并发布，无需输入参数。

工作流先跑 `cargo test --locked`，再 `cargo build --release --locked`，Release 资产是**未压缩的原始可执行文件** `writescope-linux-x86_64`（Linux x86_64，musl 静态链接，已 strip），不生成 tar.gz/xz/zip；下载后无需额外运行库：

```bash
gh release download v0.1.0 -p writescope-linux-x86_64
chmod +x writescope-linux-x86_64
./writescope-linux-x86_64 --path . selftest
```

仓库需提交 `Cargo.lock`（`--locked` 构建依赖它）。

## 用法

全局选项可以放在子命令前后：

```text
writescope [GLOBAL OPTIONS] <COMMAND>

Commands:
  seq    Sequential write of one file
  rand   Random-order write of every block in one file, exactly once
  files  Create and write many small files
  info   Resolve target filesystem/device and show current counters

Global options:
  -p, --path <DIR>                 Target directory [default: .]
      --json                       Emit JSON to stdout
      --keep                       Keep the generated run directory
      --baseline <DURATION>        Ambient sampling window [default: 1s]
      --settle <DURATION>          Post-sync observation window [default: 1s]
      --sample-interval <DURATION> Timeline sampling interval [default: 100ms]
```

`size` 类参数接受 `1GiB`、`64MiB`、`4KiB`、`512` 等写法；`duration` 使用 humantime 语法（`500ms`、`2s`）。

### 顺序写

```bash
writescope --path /mnt/data seq --size 1GiB --block-size 1MiB --sync file
```

把文件划分为等长连续块，按线程分配连续块区间，每块写一次。`--preallocate` 先用 `fallocate` 分配整个文件再写入，不支持 `fallocate` 的文件系统会直接报错。

### 随机顺序写

```bash
writescope --path /mnt/data rand --file-size 256MiB --block-size 4KiB --threads 4
```

对文件内**每个块恰好写一次**，写入顺序是该线程负责区间内块索引的随机排列（Fisher-Yates），因此可复现且不会出现空闲，也不会重复写同一块。默认创建稀疏文件后首次填充；`--prefill` 会先在测量窗口外顺序写满并 `fsync`，测量阶段再随机覆写。

`--seed` 默认 42。每块索引使用 u32，`file-size / block-size` 必须 ≤ 16,777,216（shuffle 内存上限 64 MiB），超出时请增大 `--block-size` 或减小 `--file-size`。

### 小文件

```bash
writescope --path /mnt/data files --count 10000 --file-size 4KiB --threads 4
```

在 run 目录中扁平创建 `f-00000000`、`f-00000001`…… 每个文件写入 `--file-size` 字节，按 ≤1 MiB 的 chunk 循环写。

### `--sync` 语义

| 值 | seq / rand | files |
| --- | --- | --- |
| `file`（默认） | 负载结束后 `fsync` 数据文件 | 每个文件写完后 `fsync` |
| `write` | 每次 `write` 后 `fsync` | 每个 chunk 写完后 `fsync`（4 KiB 文件时等同 `file`） |
| `fs` | 负载结束后 `syncfs`，刷新整个文件系统 | 同左 |

`file`/`write` 模式在最终同步时还会 `fsync` run 目录，确保目录项落盘。`fs` 模式会刷新整个文件系统的脏数据，可能把无关写入计入本次测量。

### 其他选项

- `--direct`：以 `O_DIRECT` 打开数据文件，绕过 page cache。要求 `--block-size` 和负载大小都是文件系统块大小（`st_blksize`）的整数倍；不满足或文件系统不支持时直接报错，不会自动降级为 buffered I/O。
- `--preallocate`：仅 `seq` 可用，使用 `fallocate` 预分配整个文件。
- `--prefill`：仅 `rand` 可用，测量前顺序写满文件。
- `--threads`：1..=1024，且不超过写单元数量（seq/rand 的块数、files 的文件数）。每个线程一个缓冲区，`block_size * threads` 不得超过 512 MiB。
- `--pattern`：`random` 为每个写单元生成新的确定性随机内容（不是重复写入同一缓冲区），避免压缩文件系统把重复内容误判为可压缩数据；`zero` 为全零。

## 示例输出

```text
target      /mnt/hdd0/work (ext4 on /dev/sdb1, /mnt/hdd0)
devices     sdb1 (8:17, 512 B logical, rotational)
workload    seq: 256.0 MiB, block 1.0 MiB, sync=file, pattern=random, threads=1
logical     256.0 MiB
physical    256.5 MiB (active 256.0 MiB, settle 0.5 MiB)
WAF         1.002x
active      1.234 s
settle      1.000 s
throughput  207.5 MiB/s logical
block I/O   812 writes, 4 flushes, 0 B discarded
ambient     0 B/s (0.00% of physical)
```

`info` 不创建文件，只解析设备并做一次 baseline 采样：

```text
target      /mnt/hdd0/work
filesystem  ext4
mount       /dev/sdb1 on /mnt/hdd0 (rw,nosuid,nodev,relatime; rw)
devices     sdb1 (8:17, 512 B logical, rotational)
ambient     0 B/s over 1.000 s
counters    sdb1: 24785048 sectors written (11.8 GiB), 32018 block writes, 0 flushes
```

## JSON 输出

`--json` 时 stdout 始终是单个 JSON 文档（`schema_version` 为 1）：

```json
{
  "schema_version": 1,
  "tool": "writescope",
  "version": "0.1.0",
  "started_unix_ms": 1760000000000,
  "target": {
    "path": "/mnt/hdd0/work",
    "filesystem": "ext4",
    "mount_point": "/mnt/hdd0",
    "mount_source": "/dev/sdb1",
    "mount_options": "rw,nosuid,nodev,relatime",
    "super_options": "rw",
    "devices": [
      {"name": "sdb1", "major": 8, "minor": 17, "logical_block_size": 512, "rotational": true}
    ]
  },
  "workload": {"kind": "seq", "size": 268435456, "block_size": 1048576, "sync": "file", "direct": false, "preallocate": false, "pattern": "random", "threads": 1},
  "result": {
    "logical_bytes": 268435456,
    "physical_bytes": 269000000,
    "physical_active_bytes": 268500000,
    "physical_settle_bytes": 500000,
    "waf": 1.002,
    "active_ms": 1234,
    "settle_ms": 1000,
    "logical_mib_per_sec": 207.5,
    "block_writes": 812,
    "flushes": 4,
    "discarded_bytes": 0,
    "ambient_bytes_per_sec": 0.0,
    "ambient_ratio": 0.0,
    "timeline": [{"offset_ms": 0, "physical_bytes": 123456, "block_writes": 12}]
  },
  "run_dir": null,
  "warnings": []
}
```

`workload.kind` 固定为 `seq` / `rand` / `files`；`sync` 为 `file` / `write` / `fs`；`pattern` 为 `random` / `zero`。`flushes`、`discarded_bytes` 在内核不提供对应字段时为 `null`。`timeline` 按 1 秒分桶，所有桶之和等于 `physical_bytes`。`info --json` 输出结构包含 `target`、`baseline_ms`、`ambient_bytes_per_sec`、`counters`、`warnings`。

## 结果解读

- **WAF ≈ 1**：写入基本原样落到块层，文件系统开销很小。
- **WAF > 1**：元数据、日志（ext4 journal、XFS log）、写时复制（btrfs）、对齐填充、`--sync write` 带来的频繁 flush 都会放大物理写入。小文件与 `files + sync file` 通常放大最明显。
- **WAF < 1**：压缩（如 btrfs zstd）或去重导致物理字节少于逻辑字节；工具会给出 warning。
- **ambient_ratio**：背景写入占测量窗口物理流量的比例；超过 5% 时给出 warning。ambient 不会被扣减，请在空闲系统上测量。

## 设备解析

工具不使用 `stat().st_dev`（btrfs 会返回匿名设备号），而是：

1. 取 `/proc/self/mountinfo` 中与目标路径匹配的最长挂载点；
2. 以挂载 source（如 `/dev/sdb1`、`/dev/mapper/root`）作为起点；
3. 递归展开 `/sys/class/block/<dev>/slaves`，取最底层的叶子块设备；
4. 叶子设备的 `sectors_written` 增量即为物理写入。

分区（如 `sda5`）本身就是计数对象，不会上溯到整盘；dm/LVM/LUKS/md 的 slave 才是实际设备。分区缺少 `queue/` 属性时从父盘读取，仍缺失则回退为 512 字节逻辑块、非 rotational。

## 限制

- 只统计块层写入计数，不包含 SSD FTL 内部写放大，也不做 eBPF 级别的进程归因。
- 只支持 Linux，且目标必须位于真实块设备上。tmpfs、ramfs、overlay、网络文件系统不可测；loop、zram、nbd、ram、sr、fd 设备会直接报错（请测量其 backing file 或物理设备）。
- dm/md 设备无法解析出任何 slave 时报错。
- btrfs 跨多设备时只统计当前解析到的叶子设备，并给出 warning；结果可能偏低。
- 背景 I/O 不扣减，只报告 ambient 速率与占比。
- 延迟写入依赖 settle 窗口吸收；settle 设得过短时 WAF 可能偏低。
- 部分内核/设备上分区不报告 flush 计数（`flushes` 为 `null` 或 0），不影响 WAF。
- 运行前会用 `statvfs` 检查可用空间（负载数据量 + 32 MiB）。空间不足时直接报错，不创建文件。

## 清理

每次运行都会在目标目录下创建 `.writescope-<pid>-<timestamp>/`，测量成功后默认删除。使用 `--keep` 可保留现场：`run_dir` 会出现在输出/JSON 中，可手动检查 `data.bin` 或小文件后删除。

工具不安装 SIGINT handler；中断运行可能留下 `.writescope-*` 目录，请手动删除：

```bash
fd -H -t d '^\.writescope-' <target-dir> -x rm -rf
```
