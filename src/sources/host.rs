//! Host health: load, memory, swap, disk use, and on Linux the pressure-stall
//! averages. Read through `sysinfo` so it works on Linux and macOS.

use crate::model::{Disk, Host};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::{DiskRefreshKind, Disks, System};

/// Disk use changes slowly; listing mounts every tick is wasted work.
const DISK_EVERY: Duration = Duration::from_secs(30);

/// How many parts a host reading is made of: load, memory and disks. A read
/// where all of them failed has failed outright.
pub const PARTS: usize = 3;

pub struct HostReader {
    sys: Result<System, String>,
    host_name: String,
    cpus: usize,
    mounts: Vec<PathBuf>,
    disks: Vec<Disk>,
    disks_at: Option<Instant>,
    /// The last reading, so a part that fails keeps its last good values.
    last: Host,
}

/// One reading of the host, and the parts of it that could not be read.
pub struct HostRead {
    pub host: Host,
    pub errors: Vec<String>,
}

impl HostReader {
    /// `paths` are the directories whose filesystems are shown (for example
    /// `/` and the home directory); duplicates on one filesystem collapse.
    pub fn new(paths: Vec<PathBuf>) -> Self {
        HostReader {
            sys: System::new().map_err(|e| e.to_string()),
            host_name: System::host_name().unwrap_or_default(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            mounts: paths,
            disks: Vec::new(),
            disks_at: None,
            last: Host::default(),
        }
    }

    pub fn read(&mut self) -> HostRead {
        let mut errors = Vec::new();
        let mut host = Host {
            name: self.host_name.clone(),
            cpus: self.cpus,
            ..self.last.clone()
        };
        match System::load_average() {
            Ok(l) => host.load = [l.one, l.five, l.fifteen],
            Err(e) => errors.push(format!("load: {e}")),
        }
        match self.sys.as_mut() {
            Ok(sys) => {
                sys.refresh_memory();
                host.mem_total = sys.total_memory();
                host.mem_available = sys.available_memory();
                host.swap_total = sys.total_swap();
                host.swap_free = sys.free_swap();
            }
            Err(e) => errors.push(format!("memory: {e}")),
        }
        let disks_due = self.disks_at.is_none_or(|t| t.elapsed() >= DISK_EVERY);
        if !disks_due {
            host.disks = self.disks.clone();
        } else {
            match Disks::new_with_refreshed_list_specifics(
                DiskRefreshKind::nothing().with_storage(),
            ) {
                Ok(disks) => {
                    let list: Vec<(PathBuf, u64, u64)> = (&disks)
                        .into_iter()
                        .map(|d| {
                            (
                                d.mount_point().to_path_buf(),
                                d.total_space(),
                                d.available_space(),
                            )
                        })
                        .collect();
                    host.disks = pick_disks(&self.mounts, &list);
                    self.disks = host.disks.clone();
                    self.disks_at = Some(Instant::now());
                }
                // Not stamped, so the next tick asks again.
                Err(e) => {
                    host.disks = self.disks.clone();
                    errors.push(format!("disks: {e}"));
                }
            }
        }
        host.psi_mem = read_psi(Path::new("/proc/pressure/memory"));
        host.psi_cpu = read_psi(Path::new("/proc/pressure/cpu"));
        self.last = host.clone();
        HostRead { host, errors }
    }
}

/// For each path, the filesystem with the longest mount point containing it.
pub fn pick_disks(paths: &[PathBuf], list: &[(PathBuf, u64, u64)]) -> Vec<Disk> {
    let mut out: Vec<Disk> = Vec::new();
    for p in paths {
        let best = list
            .iter()
            .filter(|(m, total, _)| *total > 0 && p.starts_with(m))
            .max_by_key(|(m, _, _)| m.as_os_str().len());
        if let Some((mount, total, avail)) = best {
            let mount = mount.display().to_string();
            if out.iter().any(|d| d.mount == mount) {
                continue;
            }
            out.push(Disk {
                mount,
                used_pct: (total - avail) as f64 * 100.0 / *total as f64,
            });
        }
    }
    out
}

/// `some avg10=0.37 ...` -> 0.37, from a Linux pressure-stall file.
pub fn parse_psi(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    let field = line.split_whitespace().find(|f| f.starts_with("avg10="))?;
    field["avg10=".len()..].parse().ok()
}

fn read_psi(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path)
        .ok()
        .as_deref()
        .and_then(parse_psi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psi_avg10() {
        let text = "some avg10=0.37 avg60=0.14 avg300=0.41 total=4719284027\nfull avg10=0.37 avg60=0.14 avg300=0.32 total=3718155615\n";
        assert_eq!(parse_psi(text), Some(0.37));
        assert_eq!(parse_psi(""), None);
    }

    #[test]
    fn disks_for_paths() {
        let gib = 1 << 30;
        let list = vec![
            (PathBuf::from("/"), 20 * gib, 16 * gib),
            (PathBuf::from("/home"), 100 * gib, 50 * gib),
        ];
        let d = pick_disks(
            &[
                PathBuf::from("/"),
                PathBuf::from("/home/someone"),
                PathBuf::from("/home/x"),
            ],
            &list,
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].mount, "/");
        assert!((d[0].used_pct - 20.0).abs() < 0.01);
        assert_eq!(d[1].mount, "/home");
        assert!((d[1].used_pct - 50.0).abs() < 0.01);
    }

    #[test]
    fn reads_this_host() {
        let mut r = HostReader::new(vec![PathBuf::from("/")]);
        let read = r.read();
        assert!(read.errors.is_empty(), "{:?}", read.errors);
        assert!(read.host.cpus >= 1);
        assert!(read.host.mem_total > 0);
    }
}
