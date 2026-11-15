# rsiscsi

English | [简体中文](README.md)

An iSCSI initiator implemented in Rust: a protocol engine + CLI + native desktop application (GPUI / gpui-kit),
plus an optional in-house kernel extension (kext) for mounting a LUN read-only as a local disk.
Unless the in-house kext is explicitly loaded, it does not modify iSCSIKit, install system extensions, or change system settings.

## Build and Test

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace
cargo test -p iscsi-core -p iscsi-gui
cargo clippy -p iscsi-core -p iscsi-cli --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p iscsi-cli -- --help
cargo run -p iscsi-gui
```

`gpui-kit` 0.9.1 requires Rust >= 1.100 (`std::hint::cold_path`,
`Option::map_or_default`), so `rust-toolchain.toml` pins the locally verified version 1.100.0.
Global toolchain settings are not changed.

By default, tests access only local mock Targets. A real read-only smoke test requires explicit configuration:

```sh
ISCSI_TEST_HOST=10.1.10.1 ISCSI_TEST_LUN=1 \
  cargo test -p iscsi-core real_target_smoke_env_gated -- --nocapture
```

Use `ISCSI_TEST_IQN` to specify another IQN. The test does not write to the remote Target.

## Packaging

```sh
./scripts/package-dmg.sh
```

The output is `dist/rsiscsi-<版本>-<架构>.dmg`. The script builds only the user-space application and uses ad-hoc signing;
it does not include a DriverKit extension or require disabling SIP or changing system security settings.
Without developer signing and notarization, `spctl` rejects this DMG. To launch it for the first time,
click "Open Anyway" in "System Settings > Privacy & Security" (the right-click Open bypass is no longer available starting with macOS 15),
or run:

```sh
xattr -dr com.apple.quarantine /Applications/iSCSI.app
```

The "使用说明.txt" file in the DMG contains the same steps. This version does not create `/dev/disk*`,
so the Target Disk is not visible in Finder or Disk Utility.

## Local Read-only Mounting (In-house kext)

The user-space version above does not create `/dev/disk*`. To make a LUN actually appear in Disk Utility and Finder,
this repository includes an in-house kext (`kext/`, an `IOBlockStorageDevice` subclass) and a Rust daemon
(`crates/iscsi-blockd`). The kernel only transfers blocks; `iscsi-core` still handles the iSCSI protocol in user space.
There is no dependency on iSCSIKit and no linking against libiscsi.

```sh
./kext/build.sh                                  # Produces kext/build/ISCSIVirtualDisk.kext
sudo ./scripts/load-kext.sh                      # Approve in System Settings after the first load
sudo cargo run -p iscsi-blockd -- \
  --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1
diskutil list                                    # In another terminal
sudo ./scripts/unload-kext.sh
```

The first version is **read-only**: the kext immediately returns `kIOReturnNotWritable` for write requests, and the daemon only calls
`Session::read_into`. An unsigned (ad-hoc) kext on Apple silicon requires first lowering the startup security policy
to Permissive Security and approving it in System Settings; the scripts do not change system security settings.
See [`docs/kext-mount.md`](docs/kext-mount.md) for the full prerequisites, ABI, shared-memory layout, and troubleshooting steps.

## Code Signing and Distribution

This project currently **uses ad-hoc signing throughout** and has no paid Apple Developer Program account (USD 99/year).
This has two practical effects:

- **App / DMG**: Without Developer ID signing and notarization, Gatekeeper rejects the first launch. Click
  "Open Anyway" in "System Settings > Privacy & Security", or manually run
  `xattr -dr com.apple.quarantine /Applications/iSCSI.app`.
- **kernel extension (kext)**: An ad-hoc-signed kext on Apple silicon requires lowering the startup security policy
  (Permissive Security) and approval in System Settings; approval is usually required again after a major system upgrade.

Plan: Once this repository's downloads, Stars, or donations accumulate enough to cover the annual developer fee, we will immediately enroll in the
Apple Developer Program, sign the App and DMG with a Developer ID Application certificate, and submit them for notarization,
removing the first barrier above, and switch to a properly signed kext.

Note that **a developer account cannot remove the kext startup security policy requirement**: any third-party kext
on Apple silicon, signed or unsigned, requires user approval and a lowered startup security policy. A complete solution requires moving the kernel component
to a DriverKit System Extension (dext), which also requires a paid account and is this project's long-term direction.

To support this project (with a Star, Issue feedback, or a donation), contact the author through the repository homepage; no specific timeline is promised.

## Releases and Downloads

CI builds the DMG on `v*` tag pushes or manual runs and uploads it to the
"软件集合/rsiscsi/<version>/" folder on the CTFile drive. Binaries are **not** attached to
GitHub Releases, for two reasons:

- **Downloads in mainland China**: GitHub Release downloads are often unreachable or very slow
  there, while the drive link works directly.
- **Funding development**: keeping every download on one sharing page makes it easier to
  accumulate sponsorship for the Apple Developer Program fee (everything is ad-hoc signed
  today, see the previous section).

CI builds and publishes on the GitHub-hosted `macos-latest` runner. CTFile rejects WebDAV
logins from foreign IPs with `Access denied: Login from different location detected`, so
`WEBDAV_URL` points at a reverse proxy on a NAS in mainland China that forwards to
`dav.ctfile.com`; see [`docs/reverse-proxy.md`](docs/reverse-proxy.md) for setup and
troubleshooting.

The same flow runs locally. Put the credentials in `.env.release`
(git-ignored, template in `.env.release.example`):

```sh
WEBDAV_URL=https://dav.ctfile.com/ws35579
WEBDAV_USERNAME=you@example.com
WEBDAV_PASSWORD=...
WEBDAV_ROOT_PATH=/
SOFTWARE_COLLECTION_DIR=软件集合/rsiscsi
```

Then one command builds, signs and uploads:

```sh
./scripts/release-local.sh
```

Artifacts land in `软件集合/rsiscsi/<version>/` as the DMG plus `.sha256`. Missing folders are
created and re-runs overwrite same-named files. `WEBDAV_ROOT_PATH=/` means `软件集合` sits at
the drive root; a leading `/` in `SOFTWARE_COLLECTION_DIR` ignores the root path.

CI runs only on tags or manual dispatch, so regular pushes do not consume macOS minutes.
Locally, `./scripts/package-dmg.sh` still produces the same DMG.

## CLI

```sh
cargo run -p iscsi-cli -- discover --host 10.1.10.1
cargo run -p iscsi-cli -- ls --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836
cargo run -p iscsi-cli -- inq --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 --lun 1
cargo run -p iscsi-cli -- read --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1 --lba 0 --blocks 256 --out target/block.bin
cargo run -p iscsi-cli -- bench --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1 --pattern seq-read --bs 1MiB --qd 1,16,32 --duration 5 --json
```

With `--json`, successful results go to stdout and error JSON goes to stderr; Cargo output and logs are not mixed into the results.
The binary name is `rsiscsi`; the Cargo package name is `iscsi-cli`.
Exit codes: 0 success, 2 arguments, 3 connection, 4 login, 5 I/O in an established Session.

Global options include `--nodelay=false`, `--header-digest`, and `--data-digest`.
`--queue-depth` limits in-flight commands in a Session (default 128); `--qd` selects the Benchmark concurrency depth
and must not exceed the Session limit. `--warmup` defaults to 1 second; `--duration` is the measured test duration in seconds.
Block sizes support B, KiB, MiB, GiB, and decimal KB/MB; they must be aligned to the logical block size.

CHAP can be specified with `--chap-user` / `--chap-secret`; command-line arguments may be visible to other processes on the same machine.
Do not put credentials in scripts, logs, or commits. Debug output for protocol structures does not display the secret.

### Write Safety

`seq-write` / `rand-write` permanently overwrite the selected region, including during warmup.
Both the CLI and the core `benchmark` require explicit destructive-write confirmation and a region:
`--allow-destructive-write --start-lba <起始块> --range-blocks <块数>`.
Without confirmation, the request is rejected before connecting; the core library also rejects it before any SCSI I/O.
No disposable real LBA region was identified for this round, so no real NAS write test was performed.
The write protocol (all immediate data, immediate data plus multiple R2T rounds, no immediate data, CRC32C, and cache synchronization)
and both write Benchmark modes were verified against local mock Targets.

### Local Cache

**Default policy: read caching is enabled with automatic capacity; Write-back is disabled.** Unlike WCE/RCD in the Target's MODE SENSE pages,
this is client-side memory unique to each Session. It does not change the Target's cache policy and is not shared across processes, reconnections, or Targets.

- Read cache: `--cache-size auto|<大小>|off`. `auto` (the default for the read subcommand) uses
  1/8 of local memory, clamped to 128 MiB-2 GiB and rounded down to a multiple of 64 MiB; it falls back to 512 MiB if memory information is unavailable.
  `bench` defaults to `off` so local cache hits do not mask actual LUN throughput; explicitly enabling it changes what the throughput figures mean.
  Fixed 64 KiB segments with a byte-budgeted LRU;
  caching applies only when both the byte offset and length are segment-aligned. Consecutive missing segments are read together, up to 1 MiB per read.
  Unaligned requests are passed through in full. A capacity smaller than one segment stores no segments.
- Default write-through: `--write-back off` (default). Writes are sent to the LUN immediately and invalidate overlapping local read cache entries;
  no extra SYNCHRONIZE CACHE is sent, preserving the original completion semantics. Write-through does not mean data is durable against power loss at the Target.
- **Dangerous Write-back**: `--write-back <容量|auto>` also requires `--allow-destructive-write`;
  `auto` uses the recommended value: 1/4 of the automatic read capacity, clamped to 64-256 MiB.
  Aligned writes may return after being stored only in memory. When the dirty-data capacity is exceeded, on explicit `flush` or `logout`, or
  when `--flush-interval <毫秒>` expires, writes are coalesced in ascending LBA order, followed by a single
  SYNCHRONIZE CACHE (group commit). Dirty data is cleared only if everything succeeds.
  Unaligned writes first flush existing dirty data, then write through the original request without read-modify-write or splitting.

**A crash or power loss will lose unflushed writes.** Write-back is not a durable log, transaction, or remotely shared cache;
simply dropping the last Session cannot guarantee an asynchronous flush either. Callers must explicitly call `flush` /
`logout` and handle errors. If `logout(self)` fails and a retry is needed, the caller must retain another
Session clone; the GUI retains the original handle, logs flush failures, and blocks disconnection, Target switching, and window closure.
A CLI flush failure on exit returns 5 and reports both the unflushed byte count and the loss of in-memory data caused by process exit;
the CLI provides neither persistent recovery nor implicit retries. A failed exit must not be treated as evidence that data has reached the Disk.
Forced process termination cannot be intercepted. The ordinary read cache also provides no multi-initiator coherence:
changes to the LUN by other clients do not automatically invalidate this in-memory cache. Enable it only when data is stable or access is exclusive.

```sh
# Read twice in the same Session; read-only, without enabling Write-back.
cargo run -p iscsi-cli -- read --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1 --lba 0 --blocks 256 --out target/cached-read.bin \
  --cache-size 1MiB --repeat 2 --json
```

`hits` / `misses` count **bytes**; the hit rate is `hits / (hits + misses)`.
`misses` counts bytes actually read successfully from the LUN, including unaligned pass-through reads;
`bytes_served` counts bytes supplied from the local read cache or dirty segments. All statistics are zero when caching is disabled.
`bench --json` adds a `cache` field; existing result fields are unchanged.
`read --repeat` defaults to 1; two separately launched CLI processes do not share a cache.

The Rust API consists of `Session::enable_cache`, `read_cached`, `write_cached`, and `cache_stats`;
`read_pooled` / `read_buffer` retain the original pooled direct-read path. Existing write APIs always write through;
if dirty data already exists, it is flushed first to preserve ordering. Configuration changes are allowed only while idle and with no dirty data;
`try_enable_cache` returns an error, while the compatible `enable_cache` logs the error and preserves the original configuration.
Periodic flush errors can be observed through `cache_flush_error()` and are cleared after a successful retry.

On the GUI Benchmark page, the read cache defaults to "Auto (recommended)", and Write-back defaults to disabled (with "Recommended (auto)" preselected);
capacity is selected from dropdown presets: 64 MiB-1 GiB for the read cache and 64-512 MiB for Write-back, with actual MiB values shown next to automatic/recommended values.
Write-back requires a second "Allow writes" confirmation. The Session page shows capacity, hit rate, and dirty data only when caching is enabled.

Large sequential reads, working sets larger than the cache, or infrequent repeat access usually gain nothing and instead incur an extra memory copy.
The previous read-only baseline of approximately 165 MiB/s from the NAS backend cannot be overcome with a low-hit-rate client cache.
See [`docs/local-cache.md`](docs/local-cache.md) for the measurement method and this round's read-only acceptance results.

### Disk Visibility

Connecting only establishes a SCSI Session; it does not create `/dev/disk*` or mount a filesystem, so Disk Utility and Finder
cannot see it. To expose a LUN as a block device, load the in-house kext and run `iscsi-blockd`;
see "Local Read-only Mounting (In-house kext)" above. After a successful connection, the "Disk" section on the "Session" page performs a read-only `diskutil` scan:

- Newly detected Disks without a partition table are marked "Uninitialized" and offer "Open Disk Utility";
  initialization is confirmed by the user in Disk Utility. The application never erases a Disk automatically.
- Disks with an existing filesystem that is mounted show the volume mount point and offer "Open in Finder".
- The scan only calls `diskutil list -plist` and converts the output to JSON; it does not write to any Disk.
  "Refresh" rescans, and results are cleared on disconnection.

## Core Design

- One Tokio socket owner per connection, with mpsc queuing, Semaphore limiting, ITT slots, and generation numbers;
  tags are not reused within a connection, and commands obey the Target window with sequence-number wraparound checks.
- PDUs for each request batch and each R2T round are encoded into a reusable send buffer and sent together with `write_all`.
- `read_pooled` / `read_buffer` receive Data-In directly into owned pooled buffers,
  without an intermediate payload buffer. With caching disabled, Benchmark workers reuse the same buffer in a loop;
  explicitly enabling caching uses `read_cached` / `write_cached`, including memory copies in the cache layer.
- Safe Rust cannot transfer an arbitrary short-lived `&mut [u8]` to an independent `'static` socket task;
  therefore, the compatibility API `read_into` retains one final copy, and `read` also allocates a returned Vec.
  Task communication such as oneshot still allocates metadata; the entire command path is not claimed to be allocation-free.
- Digests are disabled by default; when enabled, they use `crc32c`, cover padding, and are little-endian on the wire.
  Framing/header digest handling accounts for optional AHS; current Target response opcodes do not define AHS,
  so received Target AHS is validated and then explicitly rejected, not silently ignored.
- Supports a single connection, ERL 0, InitialR2T=Yes, and in-order DataPDU/DataSequence.
  Supports Targets rejecting ImmediateData; does not support reconnection recovery, bidirectional SCSI, mutual CHAP, or task management.
- Protocol framing errors, digest errors, and timeouts terminate the connection and release all waiters;
  canceled requests that have not been sent are discarded. Canceling a sent command terminates the connection under ERL 0, without retrying writes.
  Remote completion status is uncertain after a sent write times out or is canceled; no transactional rollback semantics are provided.
- `SessionStats` provides an EWMA with a three-second time constant; raw counters use atomics.
- Benchmarks use a unified measurement clock; warmup is excluded from results, and all in-flight commands are fully drained.
  The denominator ends at the last I/O completion and excludes histogram aggregation.
  HDR histograms use three significant digits; p50/p95/p99 are not backed by an unbounded list of per-command samples.
  Write results exclude the subsequent SYNCHRONIZE CACHE time, which is recorded separately.

### Address Parsing

Connection entry points always use `Target { addr, iqn, lun, chap }`; IQNs are not split on colons.
`Target::parse_url` parses the structure; `Target::to_url` emits a canonical form with an explicit port/LUN.
For a byte-for-byte round trip of the original URL, use `TargetUrl::parse(...).as_str()`;
it preserves omitted defaults, case, leading zeros, and percent-encoded text without normalizing on load.
The historical `iscsi://iqn...` form requires a separately supplied fallback portal.
URLs and the return value of `to_url()` may contain credentials and must not be logged.

## Desktop Application

`cargo run -p iscsi-gui` opens a compact 900x620 window (a dark monochrome theme inspired by Volt Native):
saved Targets and SendTargets discovery are on the left; the right side switches between "Target / Session / Benchmark".
Targets are saved in `~/Library/Application Support/rsiscsi/targets.toml` (directory mode 0700, file mode 0600,
atomic writes using a temporary file + rename), with IQNs preserved as-is, without normalization.
Connection, INQUIRY/capacity reads, and live rates (1 Hz EWMA) run in separate Tokio tasks;
results return to the UI thread through a channel without blocking rendering.

The interface supports Chinese and English, selected automatically from the system language at startup (`zh-Hans`/`zh-TW` and similar select Chinese; others select English).
Use `RSISCSI_LANG=zh` or `RSISCSI_LANG=en` to override the language without affecting system settings.

Write tests are hidden by default: the "Allow writes (will destroy data on the LUN)" confirmation
appears only after selecting sequential/random write. Without confirmation, the Run button is disabled and a red warning is shown.
Reads are limited to the first 16 GiB of the LUN by default; connection and information reads do not send writes or SYNCHRONIZE CACHE.
Never write to unknown data regions.

## File Layout

```text
crates/iscsi-core/Cargo.toml          Core package dependencies
crates/iscsi-core/src/lib.rs          Public API and ownership notes
crates/iscsi-core/src/config.rs       Target structure, lossless URLs, connection and negotiation parameters
crates/iscsi-core/src/error.rs        Typed errors
crates/iscsi-core/src/wire.rs         BHS/PDU, CRC32C, text keys, CHAP encoding/decoding and unit tests
crates/iscsi-core/src/login.rs        TCP tuning, staged login, CHAP and parameter/window negotiation
crates/iscsi-core/src/session.rs      Session API, socket owner, ITT pipeline and SCSI transport
crates/iscsi-core/src/cache.rs        Toggleable segment LRU, dirty segments, statistics and automatic capacity
crates/iscsi-gui/src/disks.rs         Read-only diskutil detection: blank Disk guidance and opening Finder
crates/iscsi-core/src/scsi.rs         CDB, LUN, INQUIRY/VPD and capacity parsing
crates/iscsi-core/src/pool.rs         Bounded reusable buffer cache
crates/iscsi-core/src/stats.rs        Atomic counters and EWMA sampling
crates/iscsi-core/src/bench.rs        Four patterns, warmup, QD sweep, HDR latency and throughput
crates/iscsi-core/tests/mock_target.rs Local protocol, concurrency, cancellation and Benchmark integration tests
crates/iscsi-cli/Cargo.toml           CLI package and rsiscsi binary definition
crates/iscsi-cli/src/main.rs          Chinese clap commands, JSON, file output and exit codes
crates/iscsi-gui/Cargo.toml           GUI package dependencies (gpui-kit 0.9.1)
crates/iscsi-gui/src/i18n.rs          Bilingual strings and system language detection
crates/iscsi-gui/src/main.rs          Dark theme, window dimensions and GPUI startup
crates/iscsi-gui/src/state.rs         Target model, atomic configuration reads/writes, Benchmark parameters and safety checks
crates/iscsi-gui/src/engine.rs        Tokio runtime bridge, event channel, cancellation and disconnection
crates/iscsi-gui/src/views/mod.rs     Window structure: title bar, sidebar, tabs, status bar
crates/iscsi-gui/src/views/sidebar.rs Target list and discovery panel
crates/iscsi-gui/src/views/editor.rs  Target editing form and URL import
crates/iscsi-gui/src/views/session.rs Session controls, device information, Disk guidance, negotiated parameters and logs
crates/iscsi-gui/src/views/benchmark.rs Benchmark form, results and throughput bar chart
crates/iscsi-gui/src/views/updates.rs Reduction of background events into UI state
packaging/Info.plist                  App bundle metadata (version replaced by the script)
packaging/使用说明.txt                Installation instructions and limitations in the DMG
scripts/package-dmg.sh                Build the .app, apply ad-hoc signing and generate the DMG
scripts/release-local.sh              Build and upload to CTFile (needs .env.release)
scripts/publish-webdav.sh             Standalone WebDAV upload
scripts/publish-ctfile.sh             CTFile REST upload, when an open API key exists
```

See `docs/phase1-verification.md` for real Target verification data.

## License

This project is released under the GNU Affero General Public License v3.0 or later;
see [`LICENSE`](LICENSE) for the full terms. When providing a modified version of this project over a network,
you must also provide the corresponding source code.
