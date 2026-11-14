//! Classic iPods (click wheel and earlier), which show up as USB disks with an
//! `iPod_Control` folder.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Ipod {
    pub mount_point: PathBuf,
    /// Apple model number from SysInfo, e.g. "M9244" or "MA147".
    pub model_number: Option<String>,
    /// FireWire GUID from SysInfo, which identifies the iPod.
    pub firewire_guid: Option<String>,
}

impl Ipod {
    pub fn database_path(&self) -> PathBuf {
        self.mount_point.join("iPod_Control/iTunes/iTunesDB")
    }

    /// A stable ID: the FireWire GUID, or the volume name without one.
    pub fn id(&self) -> String {
        self.firewire_guid.clone().unwrap_or_else(|| self.name())
    }

    /// The volume name, which is the name given to the iPod.
    pub fn name(&self) -> String {
        self.mount_point
            .file_name()
            .map_or_else(|| "iPod".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Total and free bytes of the disk, and bytes taken by synced music.
    pub fn storage(&self) -> std::io::Result<(u64, u64, u64)> {
        let (total, free) = disk_space(&self.mount_point)?;
        Ok((
            total,
            free,
            dir_size(&self.mount_point.join("iPod_Control/Music")),
        ))
    }
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map_or(0, |m| m.len()),
        })
        .sum()
}

fn disk_space(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let block = st.f_frsize as u64;
    Ok((st.f_blocks as u64 * block, st.f_bavail as u64 * block))
}

/// Finds mounted iPods in the usual mount locations.
pub fn find() -> Vec<Ipod> {
    let mut roots = vec![PathBuf::from("/Volumes")];
    if let Some(user) = std::env::var_os("USER") {
        roots.push(Path::new("/media").join(&user));
        roots.push(Path::new("/run/media").join(&user));
    }
    let mut found: Vec<_> = roots
        .iter()
        .filter_map(|r| std::fs::read_dir(r).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter_map(|p| at(&p))
        .collect();
    found.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    found
}

/// Returns the iPod mounted at `mount_point`, if it is one.
pub fn at(mount_point: &Path) -> Option<Ipod> {
    let control = mount_point.join("iPod_Control");
    if !control.join("iTunes").is_dir() {
        return None;
    }
    let sysinfo = std::fs::read_to_string(control.join("Device/SysInfo")).unwrap_or_default();
    Some(Ipod {
        mount_point: mount_point.to_path_buf(),
        model_number: sysinfo_value(&sysinfo, "ModelNumStr"),
        firewire_guid: sysinfo_value(&sysinfo, "FirewireGuid"),
    })
}

/// Reads a `Key: value` line from SysInfo.
fn sysinfo_value(sysinfo: &str, key: &str) -> Option<String> {
    sysinfo.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key)
            .then(|| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sysinfo() {
        let s = "BoardHwName: iPod Q14\nModelNumStr: M9244\nvisibleBuildID: 0x02208000 (2.2)\n";
        assert_eq!(sysinfo_value(s, "ModelNumStr").as_deref(), Some("M9244"));
        assert_eq!(sysinfo_value(s, "pszSerialNumber"), None);
    }

    #[test]
    fn detects_ipod_folders() {
        let dir = std::env::temp_dir().join(format!("bt-ipod-test-{}", std::process::id()));
        let device = dir.join("iPod_Control/Device");
        std::fs::create_dir_all(dir.join("iPod_Control/iTunes")).unwrap();
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("SysInfo"), "ModelNumStr: MA147\n").unwrap();
        let ipod = at(&dir).unwrap();
        assert_eq!(ipod.model_number.as_deref(), Some("MA147"));
        assert!(at(&device).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
