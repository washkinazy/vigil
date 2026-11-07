//! Exit after an atomic package replacement so Waybar can launch the new client.
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

pub struct Executable {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Executable {
    pub fn current() -> Result<Self, String> {
        let path = std::env::current_exe().map_err(|e| format!("watcher executable path: {e}"))?;
        // /proc/self/exe identifies the running image, even if replaced during startup.
        let meta =
            fs::metadata("/proc/self/exe").map_err(|e| format!("watcher executable: {e}"))?;
        Ok(Self {
            path,
            device: meta.dev(),
            inode: meta.ino(),
        })
    }
    pub fn replaced(&self) -> bool {
        // An absent path during a package transaction is not yet a usable replacement.
        fs::metadata(&self.path).is_ok_and(|m| m.dev() != self.device || m.ino() != self.inode)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_atomic_replacement_but_waits_through_missing_path() {
        let dir = std::env::temp_dir().join(format!("vigil-watcher-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("vigilctl");
        fs::write(&path, b"old").unwrap();
        let old = fs::File::open(&path).unwrap();
        let meta = old.metadata().unwrap();
        let watcher = Executable {
            path: path.clone(),
            device: meta.dev(),
            inode: meta.ino(),
        };
        assert!(!watcher.replaced());
        fs::remove_file(&path).unwrap();
        assert!(!watcher.replaced());
        fs::write(&path, b"new").unwrap();
        assert!(watcher.replaced());
        fs::remove_dir_all(dir).unwrap();
    }
}
