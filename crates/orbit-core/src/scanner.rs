use crate::model::{
    AccessStatus, EntryId, EntryNode, EntryType, ScanConfig, ScanProgress, ScanResult, ScanStatus,
};
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Information about a mounted disk volume obtained from `/proc/mounts` and `statvfs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskVolumeInfo {
    pub label: String,
    pub mount_point: String,
    pub device: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f64,
    pub is_root: bool,
    pub is_home: bool,
}

/// Lists physical/primary mounted disk volumes on the Linux system using `/proc/mounts` and `statvfs`.
pub fn list_system_volumes() -> Vec<DiskVolumeInfo> {
    let mut volumes: Vec<DiskVolumeInfo> = Vec::new();
    let mut seen_mounts: HashSet<String> = HashSet::new();

    // Always include `/` (Whole System Disk) first.
    if let Some(mut root_vol) =
        stat_volume("/", "Intero Disco (/)", "/dev/root", "linux", true, false)
    {
        if let Some((dev, fstype)) = find_mount_device_for_path("/") {
            root_vol.device = dev;
            root_vol.fs_type = fstype;
        }
        seen_mounts.insert("/".to_string());
        volumes.push(root_vol);
    }

    // Parse `/proc/mounts` for additional real block filesystems (`/dev/sd*`, `/dev/nvme*`, `/dev/mapper/*`, `/dev/vd*`)
    if let Ok(contents) = fs::read_to_string("/proc/mounts") {
        let mut seen_devices: HashSet<String> = HashSet::new();
        if let Some(first) = volumes.first() {
            if first.device.starts_with("/dev/") {
                seen_devices.insert(first.device.clone());
            }
        }

        for line in contents.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }
            let dev = parts[0];
            let mnt = unescape_mount_path(parts[1]);
            let fstype = parts[2];

            if !is_real_disk_filesystem(dev, &mnt, fstype) {
                continue;
            }
            if seen_mounts.contains(&mnt) || seen_devices.contains(dev) {
                continue;
            }

            let label = if mnt == "/" {
                "Intero Disco (/)".to_string()
            } else {
                format!("Volume {}", mnt)
            };
            if let Some(vol) = stat_volume(&mnt, &label, dev, fstype, mnt == "/", false) {
                if vol.total_bytes > 0 {
                    seen_mounts.insert(mnt);
                    seen_devices.insert(dev.to_string());
                    volumes.push(vol);
                }
            }
        }
    }

    // Also provide a quick shortcut for `$HOME` if it exists and isn't already listed as a mount point
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && !seen_mounts.contains(&home) && Path::new(&home).is_dir() {
            if let Some(mut home_vol) = stat_volume(
                &home,
                &format!("Home ({})", home),
                "home",
                "ext4",
                false,
                true,
            ) {
                if let Some((dev, fstype)) = find_mount_device_for_path(&home) {
                    home_vol.device = dev;
                    home_vol.fs_type = fstype;
                }
                volumes.push(home_vol);
            }
        }
    }

    volumes
}

fn find_mount_device_for_path(target_path: &str) -> Option<(String, String)> {
    let contents = fs::read_to_string("/proc/mounts").ok()?;
    let mut best_match: Option<(usize, String, String)> = None;
    for line in contents.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let dev = parts[0];
        let mnt = unescape_mount_path(parts[1]);
        let fstype = parts[2];
        if fstype == "rootfs" || fstype == "autofs" {
            continue;
        }
        if target_path == mnt
            || (mnt == "/" && target_path.starts_with('/'))
            || (target_path.starts_with(&mnt)
                && target_path.as_bytes().get(mnt.len()) == Some(&b'/'))
        {
            let score = mnt.len() + if dev.starts_with("/dev/") { 1000 } else { 0 };
            if best_match.as_ref().map(|m| score > m.0).unwrap_or(true) {
                best_match = Some((score, dev.to_string(), fstype.to_string()));
            }
        }
    }
    best_match.map(|(_, d, f)| (d, f))
}

fn is_real_disk_filesystem(dev: &str, mnt: &str, fstype: &str) -> bool {
    if !dev.starts_with("/dev/") {
        return false;
    }
    if dev.starts_with("/dev/loop") {
        return false;
    }
    let ignored_prefixes = [
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/snap",
        "/var/lib/docker",
        "/boot/efi",
    ];
    if ignored_prefixes
        .iter()
        .any(|p| mnt == *p || mnt.starts_with(&format!("{}/", p)))
    {
        return false;
    }
    // Exclude bind-mounts to individual user hidden dirs or system subdirs (`/bin`, `/etc`, `/lib`, `/sbin`, `/usr`, `/var`)
    let system_bind_mounts = ["/bin", "/etc", "/lib", "/lib64", "/sbin", "/usr", "/var"];
    if system_bind_mounts.contains(&mnt) || mnt.contains("/.") {
        return false;
    }
    matches!(
        fstype,
        "ext4" | "ext3" | "ext2" | "btrfs" | "xfs" | "zfs" | "f2fs" | "ntfs" | "vfat" | "exfat"
    )
}

fn unescape_mount_path(raw: &str) -> String {
    raw.replace("\\040", " ").replace("\\011", "\t")
}

fn stat_volume(
    path: &str,
    label: &str,
    device: &str,
    fs_type: &str,
    is_root: bool,
    is_home: bool,
) -> Option<DiskVolumeInfo> {
    let c_path = CString::new(path.as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let frsize = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    let total_bytes = (stat.f_blocks as u64).saturating_mul(frsize);
    let free_bytes = (stat.f_bfree as u64).saturating_mul(frsize);
    let available_bytes = (stat.f_bavail as u64).saturating_mul(frsize);
    let used_bytes = total_bytes.saturating_sub(free_bytes);
    let usage_percent = if total_bytes > 0 {
        ((used_bytes as f64) / (total_bytes as f64)) * 100.0
    } else {
        0.0
    };

    Some(DiskVolumeInfo {
        label: label.to_string(),
        mount_point: path.to_string(),
        device: device.to_string(),
        fs_type: fs_type.to_string(),
        total_bytes,
        used_bytes,
        available_bytes,
        usage_percent,
        is_root,
        is_home,
    })
}

/// Thread-safe cooperative cancellation token for long-running scans.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Creates a new uncancelled token.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signals the active scan to stop as soon as possible.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Returns `true` if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Events streamed by a background scan started via [`ScanHandle::spawn`].
#[derive(Debug, Clone)]
pub enum ScanEvent {
    Progress(ScanProgress),
    Finished(Box<ScanResult>),
}

/// Non-blocking handle to a background scan running on a worker thread.
pub struct ScanHandle {
    cancel_token: CancelToken,
    receiver: mpsc::Receiver<ScanEvent>,
    join_handle: Option<JoinHandle<()>>,
}

impl ScanHandle {
    /// Spawns a directory scan on a background thread so the UI or caller never blocks.
    pub fn spawn(config: ScanConfig) -> Self {
        let cancel_token = CancelToken::new();
        let token_clone = cancel_token.clone();
        let (tx, rx) = mpsc::channel();

        let join_handle = thread::spawn(move || {
            let result = scan_directory_with_progress(&config, &token_clone, |progress| {
                let _ = tx.send(ScanEvent::Progress(progress.clone()));
            });
            let _ = tx.send(ScanEvent::Finished(Box::new(result)));
        });

        Self {
            cancel_token,
            receiver: rx,
            join_handle: Some(join_handle),
        }
    }

    /// Requests cancellation of the running background scan.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Returns a clone of the underlying [`CancelToken`].
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel_token.clone()
    }

    /// Borrows the event receiver channel (`Progress` and `Finished`).
    pub fn receiver(&self) -> &mpsc::Receiver<ScanEvent> {
        &self.receiver
    }

    /// Waits for the scan to finish and returns the final [`ScanResult`],
    /// forwarding any intermediate [`ScanProgress`] events to `on_progress`.
    pub fn wait_with_progress<F>(mut self, mut on_progress: F) -> Option<ScanResult>
    where
        F: FnMut(ScanProgress),
    {
        let mut final_result = None;
        while let Ok(event) = self.receiver.recv() {
            match event {
                ScanEvent::Progress(p) => on_progress(p),
                ScanEvent::Finished(res) => {
                    final_result = Some(*res);
                    break;
                }
            }
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        final_result
    }
}

struct HardlinkGroup {
    primary_id: EntryId,
    member_ids: Vec<EntryId>,
}

struct DirWorkItem {
    entry_id: EntryId,
    abs_path: PathBuf,
    rel_path_raw: Vec<u8>,
    depth: u32,
    dev: u64,
    ino: u64,
    expanded: bool,
}

struct RawChildStat {
    name_os: OsString,
    child_abs_path: PathBuf,
    lstat_res: io::Result<fs::Metadata>,
}

struct RawDirReadOutcome {
    work_item: DirWorkItem,
    read_dir_err: Option<io::Error>,
    iter_errors: Vec<io::Error>,
    children: Vec<RawChildStat>,
}

/// Runs a synchronous scan with default no-op progress reporting.
pub fn scan_directory(config: &ScanConfig, cancel_token: &CancelToken) -> ScanResult {
    scan_directory_with_progress(config, cancel_token, |_| {})
}

/// Runs a recursive directory scan using an explicit heap stack (immune to stack overflow on deep
/// trees), parallel batched `read_dir` + `lstat` I/O when `follow_symlinks == false`, hard-link
/// deduplication, mount-point boundary guards, and throttled progress callbacks.
pub fn scan_directory_with_progress<F>(
    config: &ScanConfig,
    cancel_token: &CancelToken,
    mut on_progress: F,
) -> ScanResult
where
    F: FnMut(&ScanProgress),
{
    let start_instant = Instant::now();
    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let root_os = config.root_path.as_os_str();
    let root_path_raw = root_os.as_bytes().to_vec();
    let root_path_display = config.root_path.to_string_lossy().into_owned();

    let mut result = ScanResult {
        root_path_raw: root_path_raw.clone(),
        root_path_display: root_path_display.clone(),
        root_dev: 0,
        config: config.clone(),
        status: ScanStatus::Completed,
        started_at_unix_ms,
        duration_ms: 0,
        root_id: None,
        entries: Vec::new(),
        total_apparent_bytes: 0,
        total_allocated_bytes: 0,
        total_files_count: 0,
        total_dirs_count: 0,
        total_symlinks_count: 0,
        hardlink_duplicate_files_count: 0,
        hardlink_saved_allocated_bytes: 0,
        hardlink_saved_apparent_bytes: 0,
        unreadable_dirs_count: 0,
        permission_denied_count: 0,
        io_errors_count: 0,
        vanished_count: 0,
        excluded_mounts_count: 0,
        symlink_cycles_count: 0,
        root_error_message: None,
    };

    if cancel_token.is_cancelled() {
        result.status = ScanStatus::Cancelled;
        result.duration_ms = start_instant.elapsed().as_millis() as u64;
        return result;
    }

    let root_meta = match fs::symlink_metadata(&config.root_path) {
        Ok(m) => m,
        Err(err) => {
            result.status = ScanStatus::Failed;
            result.root_error_message = Some(err.to_string());
            if err.kind() == io::ErrorKind::PermissionDenied {
                result.permission_denied_count += 1;
            } else {
                result.io_errors_count += 1;
            }
            result.duration_ms = start_instant.elapsed().as_millis() as u64;
            return result;
        }
    };

    if !root_meta.is_dir() {
        result.status = ScanStatus::Failed;
        result.root_error_message = Some("Selected root path is not a directory".to_string());
        result.duration_ms = start_instant.elapsed().as_millis() as u64;
        return result;
    }

    let root_dev = root_meta.dev();
    let root_ino = root_meta.ino();
    result.root_dev = root_dev;

    let root_name_os = config
        .root_path
        .file_name()
        .unwrap_or_else(|| config.root_path.as_os_str());
    let root_id = EntryId(0);
    result.root_id = Some(root_id);

    result.entries.push(EntryNode {
        id: root_id,
        parent_id: None,
        children_ids: Vec::new(),
        rel_path_raw: Vec::new(),
        rel_path_display: String::new(),
        name_raw: root_name_os.as_bytes().to_vec(),
        name_display: root_name_os.to_string_lossy().into_owned(),
        is_valid_utf8: root_os.to_str().is_some(),
        depth: 0,
        entry_type: EntryType::Directory,
        apparent_bytes: root_meta.len(),
        allocated_bytes: root_meta.blocks().saturating_mul(512),
        subtree_apparent_bytes: 0,
        subtree_allocated_bytes: 0,
        direct_files_apparent_bytes: 0,
        direct_files_allocated_bytes: 0,
        subtree_files_count: 0,
        subtree_dirs_count: 0,
        subtree_symlinks_count: 0,
        subtree_error_count: 0,
        dev: root_dev,
        ino: root_ino,
        nlink: root_meta.nlink(),
        is_hardlink_duplicate: false,
        primary_hardlink_id: None,
        shared_link_paths_in_scan: 1,
        mtime_sec: Some(root_meta.mtime()),
        mtime_nsec: u32::try_from(root_meta.mtime_nsec()).ok(),
        access_status: AccessStatus::Ok,
        error_message: None,
    });

    let mut seen_inodes: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
    let mut stack: Vec<DirWorkItem> = vec![DirWorkItem {
        entry_id: root_id,
        abs_path: config.root_path.clone(),
        rel_path_raw: Vec::new(),
        depth: 0,
        dev: root_dev,
        ino: root_ino,
        expanded: false,
    }];

    // Fast parallel batch mode when `follow_symlinks == false` and `progress_interval_ms > 0`.
    let use_parallel_batches = !config.follow_symlinks && config.progress_interval_ms > 0;

    if use_parallel_batches {
        run_parallel_batched_scan(
            config,
            cancel_token,
            &mut result,
            &mut seen_inodes,
            stack,
            start_instant,
            &mut on_progress,
        );
    } else {
        run_sequential_dfs_scan(
            config,
            cancel_token,
            &mut result,
            &mut seen_inodes,
            &mut stack,
            start_instant,
            &mut on_progress,
        );
    }

    // Backfill `shared_link_paths_in_scan` for all multi-linked files seen during the scan.
    for group in seen_inodes.values() {
        let count = group.member_ids.len() as u32;
        if count > 1 {
            for &id in &group.member_ids {
                result.entries[id.0].shared_link_paths_in_scan = count;
            }
        }
    }

    if let Some(root_node) = result.entries.first() {
        result.total_apparent_bytes = root_node.subtree_apparent_bytes;
        result.total_allocated_bytes = root_node.subtree_allocated_bytes;
    }

    if result.status != ScanStatus::Cancelled {
        if result.permission_denied_count > 0 || result.io_errors_count > 0 {
            result.status = ScanStatus::Partial;
        } else {
            result.status = ScanStatus::Completed;
        }
    }

    result.duration_ms = start_instant.elapsed().as_millis() as u64;
    result
}

fn read_single_directory_io(
    work_item: DirWorkItem,
    cancel_token: &CancelToken,
) -> RawDirReadOutcome {
    if cancel_token.is_cancelled() {
        return RawDirReadOutcome {
            work_item,
            read_dir_err: None,
            iter_errors: Vec::new(),
            children: Vec::new(),
        };
    }

    let read_dir = match fs::read_dir(&work_item.abs_path) {
        Ok(rd) => rd,
        Err(err) => {
            return RawDirReadOutcome {
                work_item,
                read_dir_err: Some(err),
                iter_errors: Vec::new(),
                children: Vec::new(),
            };
        }
    };

    let mut child_names: Vec<OsString> = Vec::new();
    let mut iter_errors: Vec<io::Error> = Vec::new();

    for dir_entry_res in read_dir {
        if cancel_token.is_cancelled() {
            break;
        }
        match dir_entry_res {
            Ok(de) => child_names.push(de.file_name()),
            Err(err) => iter_errors.push(err),
        }
    }

    child_names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

    let mut children = Vec::with_capacity(child_names.len());
    for name_os in child_names {
        if cancel_token.is_cancelled() {
            break;
        }
        let child_abs_path = work_item.abs_path.join(&name_os);
        let lstat_res = fs::symlink_metadata(&child_abs_path);
        children.push(RawChildStat {
            name_os,
            child_abs_path,
            lstat_res,
        });
    }

    RawDirReadOutcome {
        work_item,
        read_dir_err: None,
        iter_errors,
        children,
    }
}

fn run_parallel_batched_scan<F>(
    config: &ScanConfig,
    cancel_token: &CancelToken,
    result: &mut ScanResult,
    seen_inodes: &mut HashMap<(u64, u64), HardlinkGroup>,
    mut frontier: Vec<DirWorkItem>,
    start_instant: Instant,
    on_progress: &mut F,
) where
    F: FnMut(&ScanProgress),
{
    let mut last_progress_emit = Instant::now();
    let progress_interval_ms = config.progress_interval_ms;
    let worker_threads = thread::available_parallelism()
        .map(|n| n.get().clamp(2, 12))
        .unwrap_or(4);
    let batch_size = worker_threads * 6;

    // Emit immediate initial progress tick so UI shows active scanning right away
    on_progress(&ScanProgress {
        dirs_scanned: 1,
        files_scanned: 0,
        symlinks_scanned: 0,
        apparent_bytes_so_far: 0,
        allocated_bytes_so_far: 0,
        errors_count: 0,
        current_rel_path: String::new(),
        elapsed_ms: start_instant.elapsed().as_millis() as u64,
    });

    while !frontier.is_empty() {
        if cancel_token.is_cancelled() {
            result.status = ScanStatus::Cancelled;
            break;
        }

        let take_count = frontier.len().min(batch_size);
        let split_idx = frontier.len() - take_count;
        let mut batch: Vec<DirWorkItem> = frontier.split_off(split_idx);
        batch.reverse(); // preserve deterministic order

        let outcomes: Vec<RawDirReadOutcome> = if batch.len() == 1 {
            vec![read_single_directory_io(batch.pop().unwrap(), cancel_token)]
        } else {
            let chunk_size = batch.len().div_ceil(worker_threads);
            let mut chunks: Vec<Vec<DirWorkItem>> = Vec::new();
            let mut iter = batch.into_iter();
            loop {
                let c: Vec<DirWorkItem> = iter.by_ref().take(chunk_size).collect();
                if c.is_empty() {
                    break;
                }
                chunks.push(c);
            }

            thread::scope(|s| {
                let mut handles = Vec::with_capacity(chunks.len());
                for chunk in chunks {
                    handles.push(s.spawn(move || {
                        chunk
                            .into_iter()
                            .map(|item| read_single_directory_io(item, cancel_token))
                            .collect::<Vec<_>>()
                    }));
                }
                let mut combined = Vec::new();
                for h in handles {
                    if let Ok(part) = h.join() {
                        combined.extend(part);
                    }
                }
                combined
            })
        };

        let mut next_dirs: Vec<DirWorkItem> = Vec::new();
        let empty_ancestors: HashSet<(u64, u64)> = HashSet::new();

        for outcome in outcomes {
            if cancel_token.is_cancelled() {
                result.status = ScanStatus::Cancelled;
                break;
            }

            let dir_id = outcome.work_item.entry_id;
            let dir_rel_raw = outcome.work_item.rel_path_raw;
            let dir_depth = outcome.work_item.depth;

            if last_progress_emit.elapsed().as_millis() as u64 >= progress_interval_ms {
                let progress = ScanProgress {
                    dirs_scanned: result.total_dirs_count + 1,
                    files_scanned: result.total_files_count,
                    symlinks_scanned: result.total_symlinks_count,
                    apparent_bytes_so_far: result.total_apparent_bytes,
                    allocated_bytes_so_far: result.total_allocated_bytes,
                    errors_count: result.permission_denied_count + result.io_errors_count,
                    current_rel_path: result.entries[dir_id.0].rel_path_display.clone(),
                    elapsed_ms: start_instant.elapsed().as_millis() as u64,
                };
                on_progress(&progress);
                last_progress_emit = Instant::now();
            }

            if let Some(err) = outcome.read_dir_err {
                let (status, is_perm) = classify_io_error(&err);
                let node = &mut result.entries[dir_id.0];
                node.access_status = status;
                node.error_message = Some(err.to_string());
                if status.is_error() {
                    node.subtree_error_count += 1;
                    result.unreadable_dirs_count += 1;
                }
                if is_perm {
                    result.permission_denied_count += 1;
                } else if status == AccessStatus::Vanished {
                    result.vanished_count += 1;
                } else {
                    result.io_errors_count += 1;
                }
                continue;
            }

            for err in outcome.iter_errors {
                let (status, is_perm) = classify_io_error(&err);
                if status.is_error() {
                    result.entries[dir_id.0].subtree_error_count += 1;
                }
                if is_perm {
                    result.permission_denied_count += 1;
                } else if status == AccessStatus::Vanished {
                    result.vanished_count += 1;
                } else {
                    result.io_errors_count += 1;
                }
            }

            let mut dirs_for_this_parent = Vec::new();

            for child_stat in outcome.children {
                if cancel_token.is_cancelled() {
                    result.status = ScanStatus::Cancelled;
                    break;
                }
                let name_os = child_stat.name_os;
                let child_abs_path = child_stat.child_abs_path;
                let child_rel_raw = join_rel_raw(&dir_rel_raw, &name_os);
                let child_rel_display = String::from_utf8_lossy(&child_rel_raw).into_owned();
                let name_raw = name_os.as_bytes().to_vec();
                let name_display = name_os.to_string_lossy().into_owned();
                let is_valid_utf8 =
                    std::str::from_utf8(&child_rel_raw).is_ok() && name_os.to_str().is_some();
                let child_depth = dir_depth + 1;

                let lstat_meta = match child_stat.lstat_res {
                    Ok(m) => m,
                    Err(err) => {
                        record_lstat_error(
                            result,
                            dir_id,
                            child_rel_raw,
                            child_rel_display,
                            name_raw,
                            name_display,
                            is_valid_utf8,
                            child_depth,
                            &err,
                        );
                        continue;
                    }
                };

                let ft = lstat_meta.file_type();
                if ft.is_symlink() {
                    record_skipped_symlink(
                        result,
                        dir_id,
                        child_rel_raw,
                        child_rel_display,
                        name_raw,
                        name_display,
                        is_valid_utf8,
                        child_depth,
                        &lstat_meta,
                    );
                    continue;
                }

                if ft.is_dir() {
                    process_directory_child(
                        result,
                        &mut dirs_for_this_parent,
                        &empty_ancestors,
                        config,
                        dir_id,
                        child_abs_path,
                        child_rel_raw,
                        child_rel_display,
                        name_raw,
                        name_display,
                        is_valid_utf8,
                        child_depth,
                        &lstat_meta,
                    );
                    continue;
                }

                process_file_or_other_child(
                    result,
                    seen_inodes,
                    dir_id,
                    child_rel_raw,
                    child_rel_display,
                    name_raw,
                    name_display,
                    is_valid_utf8,
                    child_depth,
                    &lstat_meta,
                );
            }

            next_dirs.extend(dirs_for_this_parent);
        }

        for item in next_dirs.into_iter().rev() {
            frontier.push(item);
        }
    }

    // Topological bottom-up aggregation: since every parent has `id.0 < child.id.0`,
    // iterating in reverse order visits all children before their parents.
    for idx in (0..result.entries.len()).rev() {
        if result.entries[idx].entry_type == EntryType::Directory {
            aggregate_directory_from_children(&mut result.entries, EntryId(idx));
        }
    }
}

fn run_sequential_dfs_scan<F>(
    config: &ScanConfig,
    cancel_token: &CancelToken,
    result: &mut ScanResult,
    seen_inodes: &mut HashMap<(u64, u64), HardlinkGroup>,
    stack: &mut Vec<DirWorkItem>,
    start_instant: Instant,
    on_progress: &mut F,
) where
    F: FnMut(&ScanProgress),
{
    let mut active_ancestors: HashSet<(u64, u64)> = HashSet::new();
    let mut last_progress_emit = Instant::now();
    let progress_interval_ms = config.progress_interval_ms;

    while let Some(mut top) = stack.pop() {
        if cancel_token.is_cancelled() {
            result.status = ScanStatus::Cancelled;
            break;
        }

        if top.expanded {
            active_ancestors.remove(&(top.dev, top.ino));
            aggregate_directory_from_children(&mut result.entries, top.entry_id);
            continue;
        }

        top.expanded = true;
        active_ancestors.insert((top.dev, top.ino));
        let dir_id = top.entry_id;
        let dir_abs_path = top.abs_path.clone();
        let dir_rel_raw = top.rel_path_raw.clone();
        let dir_depth = top.depth;
        stack.push(top);

        if progress_interval_ms == 0
            || last_progress_emit.elapsed().as_millis() as u64 >= progress_interval_ms
        {
            let progress = ScanProgress {
                dirs_scanned: result.total_dirs_count + 1,
                files_scanned: result.total_files_count,
                symlinks_scanned: result.total_symlinks_count,
                apparent_bytes_so_far: result.total_apparent_bytes,
                allocated_bytes_so_far: result.total_allocated_bytes,
                errors_count: result.permission_denied_count + result.io_errors_count,
                current_rel_path: result.entries[dir_id.0].rel_path_display.clone(),
                elapsed_ms: start_instant.elapsed().as_millis() as u64,
            };
            on_progress(&progress);
            last_progress_emit = Instant::now();
        }

        let read_dir = match fs::read_dir(&dir_abs_path) {
            Ok(rd) => rd,
            Err(err) => {
                let (status, is_perm) = classify_io_error(&err);
                let node = &mut result.entries[dir_id.0];
                node.access_status = status;
                node.error_message = Some(err.to_string());
                if status.is_error() {
                    node.subtree_error_count += 1;
                    result.unreadable_dirs_count += 1;
                }
                if is_perm {
                    result.permission_denied_count += 1;
                } else if status == AccessStatus::Vanished {
                    result.vanished_count += 1;
                } else {
                    result.io_errors_count += 1;
                }
                continue;
            }
        };

        let mut child_names: Vec<OsString> = Vec::new();
        for dir_entry_res in read_dir {
            if cancel_token.is_cancelled() {
                result.status = ScanStatus::Cancelled;
                break;
            }
            match dir_entry_res {
                Ok(de) => child_names.push(de.file_name()),
                Err(err) => {
                    let (status, is_perm) = classify_io_error(&err);
                    if status.is_error() {
                        result.entries[dir_id.0].subtree_error_count += 1;
                    }
                    if is_perm {
                        result.permission_denied_count += 1;
                    } else if status == AccessStatus::Vanished {
                        result.vanished_count += 1;
                    } else {
                        result.io_errors_count += 1;
                    }
                }
            }
        }

        if cancel_token.is_cancelled() {
            result.status = ScanStatus::Cancelled;
            break;
        }

        child_names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let mut dirs_to_push: Vec<DirWorkItem> = Vec::new();

        for name_os in child_names {
            if cancel_token.is_cancelled() {
                result.status = ScanStatus::Cancelled;
                break;
            }

            let child_abs_path = dir_abs_path.join(&name_os);
            let child_rel_raw = join_rel_raw(&dir_rel_raw, &name_os);
            let child_rel_display = String::from_utf8_lossy(&child_rel_raw).into_owned();
            let name_raw = name_os.as_bytes().to_vec();
            let name_display = name_os.to_string_lossy().into_owned();
            let is_valid_utf8 =
                std::str::from_utf8(&child_rel_raw).is_ok() && name_os.to_str().is_some();
            let child_depth = dir_depth + 1;

            let lstat_meta = match fs::symlink_metadata(&child_abs_path) {
                Ok(m) => m,
                Err(err) => {
                    record_lstat_error(
                        result,
                        dir_id,
                        child_rel_raw,
                        child_rel_display,
                        name_raw,
                        name_display,
                        is_valid_utf8,
                        child_depth,
                        &err,
                    );
                    continue;
                }
            };

            let ft = lstat_meta.file_type();

            if ft.is_symlink() {
                if !config.follow_symlinks {
                    record_skipped_symlink(
                        result,
                        dir_id,
                        child_rel_raw,
                        child_rel_display,
                        name_raw,
                        name_display,
                        is_valid_utf8,
                        child_depth,
                        &lstat_meta,
                    );
                    continue;
                } else {
                    match fs::metadata(&child_abs_path) {
                        Ok(target_meta) => {
                            if target_meta.is_dir() {
                                let key = (target_meta.dev(), target_meta.ino());
                                if active_ancestors.contains(&key) {
                                    let child_id = EntryId(result.entries.len());
                                    result.total_symlinks_count += 1;
                                    result.symlink_cycles_count += 1;
                                    result.entries.push(EntryNode {
                                        id: child_id,
                                        parent_id: Some(dir_id),
                                        children_ids: Vec::new(),
                                        rel_path_raw: child_rel_raw,
                                        rel_path_display: child_rel_display,
                                        name_raw,
                                        name_display,
                                        is_valid_utf8,
                                        depth: child_depth,
                                        entry_type: EntryType::Symlink,
                                        apparent_bytes: lstat_meta.len(),
                                        allocated_bytes: lstat_meta.blocks().saturating_mul(512),
                                        subtree_apparent_bytes: 0,
                                        subtree_allocated_bytes: 0,
                                        direct_files_apparent_bytes: 0,
                                        direct_files_allocated_bytes: 0,
                                        subtree_files_count: 0,
                                        subtree_dirs_count: 0,
                                        subtree_symlinks_count: 0,
                                        subtree_error_count: 0,
                                        dev: target_meta.dev(),
                                        ino: target_meta.ino(),
                                        nlink: target_meta.nlink(),
                                        is_hardlink_duplicate: false,
                                        primary_hardlink_id: None,
                                        shared_link_paths_in_scan: 1,
                                        mtime_sec: Some(lstat_meta.mtime()),
                                        mtime_nsec: u32::try_from(lstat_meta.mtime_nsec()).ok(),
                                        access_status: AccessStatus::SymlinkCycleSkipped,
                                        error_message: Some(
                                            "Symlink points to an ancestor directory (cycle avoided)"
                                                .to_string(),
                                        ),
                                    });
                                    result.entries[dir_id.0].children_ids.push(child_id);
                                    continue;
                                }
                                process_directory_child(
                                    result,
                                    &mut dirs_to_push,
                                    &active_ancestors,
                                    config,
                                    dir_id,
                                    child_abs_path,
                                    child_rel_raw,
                                    child_rel_display,
                                    name_raw,
                                    name_display,
                                    is_valid_utf8,
                                    child_depth,
                                    &target_meta,
                                );
                                continue;
                            } else {
                                process_file_or_other_child(
                                    result,
                                    seen_inodes,
                                    dir_id,
                                    child_rel_raw,
                                    child_rel_display,
                                    name_raw,
                                    name_display,
                                    is_valid_utf8,
                                    child_depth,
                                    &target_meta,
                                );
                                continue;
                            }
                        }
                        Err(err) => {
                            let is_loop = err.raw_os_error() == Some(libc::ELOOP);
                            if is_loop {
                                result.symlink_cycles_count += 1;
                            }
                            let child_id = EntryId(result.entries.len());
                            result.total_symlinks_count += 1;
                            result.entries.push(EntryNode {
                                id: child_id,
                                parent_id: Some(dir_id),
                                children_ids: Vec::new(),
                                rel_path_raw: child_rel_raw,
                                rel_path_display: child_rel_display,
                                name_raw,
                                name_display,
                                is_valid_utf8,
                                depth: child_depth,
                                entry_type: EntryType::Symlink,
                                apparent_bytes: lstat_meta.len(),
                                allocated_bytes: lstat_meta.blocks().saturating_mul(512),
                                subtree_apparent_bytes: 0,
                                subtree_allocated_bytes: 0,
                                direct_files_apparent_bytes: 0,
                                direct_files_allocated_bytes: 0,
                                subtree_files_count: 0,
                                subtree_dirs_count: 0,
                                subtree_symlinks_count: 0,
                                subtree_error_count: 0,
                                dev: lstat_meta.dev(),
                                ino: lstat_meta.ino(),
                                nlink: lstat_meta.nlink(),
                                is_hardlink_duplicate: false,
                                primary_hardlink_id: None,
                                shared_link_paths_in_scan: 1,
                                mtime_sec: Some(lstat_meta.mtime()),
                                mtime_nsec: u32::try_from(lstat_meta.mtime_nsec()).ok(),
                                access_status: if is_loop {
                                    AccessStatus::SymlinkCycleSkipped
                                } else {
                                    AccessStatus::SymlinkSkipped
                                },
                                error_message: Some(err.to_string()),
                            });
                            result.entries[dir_id.0].children_ids.push(child_id);
                            continue;
                        }
                    }
                }
            }

            if ft.is_dir() {
                process_directory_child(
                    result,
                    &mut dirs_to_push,
                    &active_ancestors,
                    config,
                    dir_id,
                    child_abs_path,
                    child_rel_raw,
                    child_rel_display,
                    name_raw,
                    name_display,
                    is_valid_utf8,
                    child_depth,
                    &lstat_meta,
                );
                continue;
            }

            process_file_or_other_child(
                result,
                seen_inodes,
                dir_id,
                child_rel_raw,
                child_rel_display,
                name_raw,
                name_display,
                is_valid_utf8,
                child_depth,
                &lstat_meta,
            );
        }

        for item in dirs_to_push.into_iter().rev() {
            stack.push(item);
        }
    }

    while let Some(rem) = stack.pop() {
        if rem.expanded {
            aggregate_directory_from_children(&mut result.entries, rem.entry_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_lstat_error(
    result: &mut ScanResult,
    dir_id: EntryId,
    child_rel_raw: Vec<u8>,
    child_rel_display: String,
    name_raw: Vec<u8>,
    name_display: String,
    is_valid_utf8: bool,
    child_depth: u32,
    err: &io::Error,
) {
    let (status, is_perm) = classify_io_error(err);
    let child_id = EntryId(result.entries.len());
    let err_cnt = u64::from(status.is_error());
    if is_perm {
        result.permission_denied_count += 1;
    } else if status == AccessStatus::Vanished {
        result.vanished_count += 1;
    } else {
        result.io_errors_count += 1;
    }
    result.entries.push(EntryNode {
        id: child_id,
        parent_id: Some(dir_id),
        children_ids: Vec::new(),
        rel_path_raw: child_rel_raw,
        rel_path_display: child_rel_display,
        name_raw,
        name_display,
        is_valid_utf8,
        depth: child_depth,
        entry_type: EntryType::Other,
        apparent_bytes: 0,
        allocated_bytes: 0,
        subtree_apparent_bytes: 0,
        subtree_allocated_bytes: 0,
        direct_files_apparent_bytes: 0,
        direct_files_allocated_bytes: 0,
        subtree_files_count: 0,
        subtree_dirs_count: 0,
        subtree_symlinks_count: 0,
        subtree_error_count: err_cnt,
        dev: 0,
        ino: 0,
        nlink: 1,
        is_hardlink_duplicate: false,
        primary_hardlink_id: None,
        shared_link_paths_in_scan: 1,
        mtime_sec: None,
        mtime_nsec: None,
        access_status: status,
        error_message: Some(err.to_string()),
    });
    result.entries[dir_id.0].children_ids.push(child_id);
}

#[allow(clippy::too_many_arguments)]
fn record_skipped_symlink(
    result: &mut ScanResult,
    dir_id: EntryId,
    child_rel_raw: Vec<u8>,
    child_rel_display: String,
    name_raw: Vec<u8>,
    name_display: String,
    is_valid_utf8: bool,
    child_depth: u32,
    lstat_meta: &fs::Metadata,
) {
    let child_id = EntryId(result.entries.len());
    result.total_symlinks_count += 1;
    result.entries.push(EntryNode {
        id: child_id,
        parent_id: Some(dir_id),
        children_ids: Vec::new(),
        rel_path_raw: child_rel_raw,
        rel_path_display: child_rel_display,
        name_raw,
        name_display,
        is_valid_utf8,
        depth: child_depth,
        entry_type: EntryType::Symlink,
        apparent_bytes: lstat_meta.len(),
        allocated_bytes: lstat_meta.blocks().saturating_mul(512),
        subtree_apparent_bytes: 0,
        subtree_allocated_bytes: 0,
        direct_files_apparent_bytes: 0,
        direct_files_allocated_bytes: 0,
        subtree_files_count: 0,
        subtree_dirs_count: 0,
        subtree_symlinks_count: 0,
        subtree_error_count: 0,
        dev: lstat_meta.dev(),
        ino: lstat_meta.ino(),
        nlink: lstat_meta.nlink(),
        is_hardlink_duplicate: false,
        primary_hardlink_id: None,
        shared_link_paths_in_scan: 1,
        mtime_sec: Some(lstat_meta.mtime()),
        mtime_nsec: u32::try_from(lstat_meta.mtime_nsec()).ok(),
        access_status: AccessStatus::SymlinkSkipped,
        error_message: None,
    });
    result.entries[dir_id.0].children_ids.push(child_id);
}

fn is_configured_exclusion(config: &ScanConfig, abs_path: &Path) -> bool {
    config
        .excluded_paths
        .iter()
        .any(|p| p.as_path() == abs_path)
}

/// `true` if the directory's name is in `excluded_names` (`target` only next to a `Cargo.toml`,
/// so that an unrelated folder called "target" is still walked). With name exclusions enabled, a
/// Python virtualenv is recognised by its `pyvenv.cfg` whatever its name (`.venv-gpu`, `env`).
fn is_excluded_name(config: &ScanConfig, abs_path: &Path) -> bool {
    if config.excluded_names.is_empty() {
        return false;
    }
    let Some(name) = abs_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !config.excluded_names.iter().any(|n| n == name) {
        return abs_path.join("pyvenv.cfg").is_file();
    }
    name != "target"
        || abs_path
            .parent()
            .map(|p| p.join("Cargo.toml").is_file())
            .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn process_directory_child(
    result: &mut ScanResult,
    dirs_to_push: &mut Vec<DirWorkItem>,
    active_ancestors: &HashSet<(u64, u64)>,
    config: &ScanConfig,
    parent_id: EntryId,
    child_abs_path: PathBuf,
    child_rel_raw: Vec<u8>,
    child_rel_display: String,
    name_raw: Vec<u8>,
    name_display: String,
    is_valid_utf8: bool,
    child_depth: u32,
    meta: &fs::Metadata,
) {
    let dev = meta.dev();
    let ino = meta.ino();
    let child_id = EntryId(result.entries.len());

    // Never traverse configured exclusions (by default the Linux kernel pseudo-filesystems `/proc`,
    // `/sys`, `/dev`, `/run`), and respect `cross_mounts == false` for other filesystem devices
    // (`st_dev != root_dev`).
    let is_pseudo = is_configured_exclusion(config, &child_abs_path);
    let is_name = !is_pseudo && is_excluded_name(config, &child_abs_path);
    if is_pseudo || is_name || (!config.cross_mounts && dev != result.root_dev) {
        result.total_dirs_count += 1;
        result.excluded_mounts_count += 1;
        result.entries.push(EntryNode {
            id: child_id,
            parent_id: Some(parent_id),
            children_ids: Vec::new(),
            rel_path_raw: child_rel_raw,
            rel_path_display: child_rel_display,
            name_raw,
            name_display,
            is_valid_utf8,
            depth: child_depth,
            entry_type: EntryType::Directory,
            apparent_bytes: meta.len(),
            allocated_bytes: meta.blocks().saturating_mul(512),
            subtree_apparent_bytes: 0,
            subtree_allocated_bytes: 0,
            direct_files_apparent_bytes: 0,
            direct_files_allocated_bytes: 0,
            subtree_files_count: 0,
            subtree_dirs_count: 0,
            subtree_symlinks_count: 0,
            subtree_error_count: 0,
            dev,
            ino,
            nlink: meta.nlink(),
            is_hardlink_duplicate: false,
            primary_hardlink_id: None,
            shared_link_paths_in_scan: 1,
            mtime_sec: Some(meta.mtime()),
            mtime_nsec: u32::try_from(meta.mtime_nsec()).ok(),
            access_status: AccessStatus::ExcludedMount,
            error_message: Some(if is_name {
                format!(
                    "Directory esclusa per nome: dipendenze, cache o build ({})",
                    child_abs_path.display()
                )
            } else if is_pseudo {
                format!(
                    "Percorso escluso dalla configurazione di scansione ({})",
                    child_abs_path.display()
                )
            } else {
                format!(
                    "Directory su un mount point/filesystem differente (st_dev={} != root_dev={})",
                    dev, result.root_dev
                )
            }),
        });
        result.entries[parent_id.0].children_ids.push(child_id);
        return;
    }

    if active_ancestors.contains(&(dev, ino)) {
        result.total_dirs_count += 1;
        result.symlink_cycles_count += 1;
        result.entries.push(EntryNode {
            id: child_id,
            parent_id: Some(parent_id),
            children_ids: Vec::new(),
            rel_path_raw: child_rel_raw,
            rel_path_display: child_rel_display,
            name_raw,
            name_display,
            is_valid_utf8,
            depth: child_depth,
            entry_type: EntryType::Directory,
            apparent_bytes: meta.len(),
            allocated_bytes: meta.blocks().saturating_mul(512),
            subtree_apparent_bytes: 0,
            subtree_allocated_bytes: 0,
            direct_files_apparent_bytes: 0,
            direct_files_allocated_bytes: 0,
            subtree_files_count: 0,
            subtree_dirs_count: 0,
            subtree_symlinks_count: 0,
            subtree_error_count: 0,
            dev,
            ino,
            nlink: meta.nlink(),
            is_hardlink_duplicate: false,
            primary_hardlink_id: None,
            shared_link_paths_in_scan: 1,
            mtime_sec: Some(meta.mtime()),
            mtime_nsec: u32::try_from(meta.mtime_nsec()).ok(),
            access_status: AccessStatus::SymlinkCycleSkipped,
            error_message: Some("Directory cycle detected".to_string()),
        });
        result.entries[parent_id.0].children_ids.push(child_id);
        return;
    }

    result.total_dirs_count += 1;
    result.entries.push(EntryNode {
        id: child_id,
        parent_id: Some(parent_id),
        children_ids: Vec::new(),
        rel_path_raw: child_rel_raw.clone(),
        rel_path_display: child_rel_display,
        name_raw,
        name_display,
        is_valid_utf8,
        depth: child_depth,
        entry_type: EntryType::Directory,
        apparent_bytes: meta.len(),
        allocated_bytes: meta.blocks().saturating_mul(512),
        subtree_apparent_bytes: 0,
        subtree_allocated_bytes: 0,
        direct_files_apparent_bytes: 0,
        direct_files_allocated_bytes: 0,
        subtree_files_count: 0,
        subtree_dirs_count: 0,
        subtree_symlinks_count: 0,
        subtree_error_count: 0,
        dev,
        ino,
        nlink: meta.nlink(),
        is_hardlink_duplicate: false,
        primary_hardlink_id: None,
        shared_link_paths_in_scan: 1,
        mtime_sec: Some(meta.mtime()),
        mtime_nsec: u32::try_from(meta.mtime_nsec()).ok(),
        access_status: AccessStatus::Ok,
        error_message: None,
    });
    result.entries[parent_id.0].children_ids.push(child_id);

    dirs_to_push.push(DirWorkItem {
        entry_id: child_id,
        abs_path: child_abs_path,
        rel_path_raw: child_rel_raw,
        depth: child_depth,
        dev,
        ino,
        expanded: false,
    });
}

#[allow(clippy::too_many_arguments)]
fn process_file_or_other_child(
    result: &mut ScanResult,
    seen_inodes: &mut HashMap<(u64, u64), HardlinkGroup>,
    parent_id: EntryId,
    child_rel_raw: Vec<u8>,
    child_rel_display: String,
    name_raw: Vec<u8>,
    name_display: String,
    is_valid_utf8: bool,
    child_depth: u32,
    meta: &fs::Metadata,
) {
    let ft = meta.file_type();
    let entry_type = if ft.is_file() {
        EntryType::File
    } else {
        EntryType::Other
    };

    let dev = meta.dev();
    let ino = meta.ino();
    let nlink = meta.nlink();
    let apparent_bytes = meta.len();
    let allocated_bytes = meta.blocks().saturating_mul(512);
    let child_id = EntryId(result.entries.len());

    let mut is_hardlink_duplicate = false;
    let mut primary_hardlink_id = None;

    if nlink > 1 && ino != 0 {
        let group = seen_inodes
            .entry((dev, ino))
            .or_insert_with(|| HardlinkGroup {
                primary_id: child_id,
                member_ids: Vec::new(),
            });
        if group.primary_id != child_id {
            is_hardlink_duplicate = true;
            primary_hardlink_id = Some(group.primary_id);
            result.hardlink_duplicate_files_count += 1;
            result.hardlink_saved_apparent_bytes = result
                .hardlink_saved_apparent_bytes
                .saturating_add(apparent_bytes);
            result.hardlink_saved_allocated_bytes = result
                .hardlink_saved_allocated_bytes
                .saturating_add(allocated_bytes);
        }
        group.member_ids.push(child_id);
    }

    if entry_type == EntryType::File {
        result.total_files_count += 1;
    }

    let contrib_apparent = if is_hardlink_duplicate {
        0
    } else {
        apparent_bytes
    };
    let contrib_allocated = if is_hardlink_duplicate {
        0
    } else {
        allocated_bytes
    };

    result.total_apparent_bytes = result.total_apparent_bytes.saturating_add(contrib_apparent);
    result.total_allocated_bytes = result
        .total_allocated_bytes
        .saturating_add(contrib_allocated);

    result.entries.push(EntryNode {
        id: child_id,
        parent_id: Some(parent_id),
        children_ids: Vec::new(),
        rel_path_raw: child_rel_raw,
        rel_path_display: child_rel_display,
        name_raw,
        name_display,
        is_valid_utf8,
        depth: child_depth,
        entry_type,
        apparent_bytes,
        allocated_bytes,
        subtree_apparent_bytes: contrib_apparent,
        subtree_allocated_bytes: contrib_allocated,
        direct_files_apparent_bytes: 0,
        direct_files_allocated_bytes: 0,
        subtree_files_count: u64::from(entry_type == EntryType::File),
        subtree_dirs_count: 0,
        subtree_symlinks_count: 0,
        subtree_error_count: 0,
        dev,
        ino,
        nlink,
        is_hardlink_duplicate,
        primary_hardlink_id,
        shared_link_paths_in_scan: 1,
        mtime_sec: Some(meta.mtime()),
        mtime_nsec: u32::try_from(meta.mtime_nsec()).ok(),
        access_status: AccessStatus::Ok,
        error_message: None,
    });
    result.entries[parent_id.0].children_ids.push(child_id);
}

fn aggregate_directory_from_children(entries: &mut [EntryNode], dir_id: EntryId) {
    let children_ids = entries[dir_id.0].children_ids.clone();
    let mut sub_app = 0u64;
    let mut sub_alloc = 0u64;
    let mut direct_app = 0u64;
    let mut direct_alloc = 0u64;
    let mut sub_files = 0u64;
    let mut sub_dirs = 0u64;
    let mut sub_symlinks = 0u64;
    let mut sub_errors = entries[dir_id.0].subtree_error_count;

    for cid in children_ids {
        let child = &entries[cid.0];
        sub_app = sub_app.saturating_add(child.subtree_apparent_bytes);
        sub_alloc = sub_alloc.saturating_add(child.subtree_allocated_bytes);
        sub_errors = sub_errors.saturating_add(child.subtree_error_count);

        match child.entry_type {
            EntryType::Directory => {
                sub_dirs += 1 + child.subtree_dirs_count;
                sub_files += child.subtree_files_count;
                sub_symlinks += child.subtree_symlinks_count;
            }
            EntryType::File => {
                sub_files += 1;
                direct_app = direct_app.saturating_add(child.subtree_apparent_bytes);
                direct_alloc = direct_alloc.saturating_add(child.subtree_allocated_bytes);
            }
            EntryType::Symlink => {
                sub_symlinks += 1;
            }
            EntryType::Other => {
                direct_app = direct_app.saturating_add(child.subtree_apparent_bytes);
                direct_alloc = direct_alloc.saturating_add(child.subtree_allocated_bytes);
            }
        }
    }

    let dir_node = &mut entries[dir_id.0];
    dir_node.subtree_apparent_bytes = sub_app;
    dir_node.subtree_allocated_bytes = sub_alloc;
    dir_node.direct_files_apparent_bytes = direct_app;
    dir_node.direct_files_allocated_bytes = direct_alloc;
    dir_node.subtree_files_count = sub_files;
    dir_node.subtree_dirs_count = sub_dirs;
    dir_node.subtree_symlinks_count = sub_symlinks;
    dir_node.subtree_error_count = sub_errors;
}

fn join_rel_raw(parent_rel: &[u8], child_name: &OsStr) -> Vec<u8> {
    let child_bytes = child_name.as_bytes();
    if parent_rel.is_empty() {
        child_bytes.to_vec()
    } else {
        let mut out = Vec::with_capacity(parent_rel.len() + 1 + child_bytes.len());
        out.extend_from_slice(parent_rel);
        out.push(b'/');
        out.extend_from_slice(child_bytes);
        out
    }
}

fn classify_io_error(err: &io::Error) -> (AccessStatus, bool) {
    match err.kind() {
        io::ErrorKind::PermissionDenied => (AccessStatus::PermissionDenied, true),
        io::ErrorKind::NotFound => (AccessStatus::Vanished, false),
        _ => (AccessStatus::IoError, false),
    }
}

/// Helper to reconstruct a relative [`PathBuf`] from raw POSIX bytes (`rel_path_raw`).
pub fn raw_bytes_to_path_buf(bytes: &[u8]) -> PathBuf {
    Path::new(OsStr::from_bytes(bytes)).to_path_buf()
}
