//! Atomic, private storage for manual inhibitors. Desktop observations are never saved.
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use vigil_core::{ManualInhibitors, ManualSnapshot};

pub fn default_path() -> Result<PathBuf, String> {
    let root = match std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required for manual state")?)
            .join(".local/state"),
    };
    if !root.is_absolute() {
        return Err("manual state directory must be absolute".into());
    }
    Ok(root.join("vigil/manual.json"))
}

pub struct Store {
    path: PathBuf,
    saved: RefCell<Option<ManualSnapshot>>,
    _lock: File,
}
impl Drop for Store {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}
impl Store {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("state file needs a parent directory")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| format!("manual state directory: {e}"))?;
        let meta = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err(
                "manual state parent must be a private directory (0700), not a symlink".into(),
            );
        }
        let lock_path = path.with_extension("lock");
        check_file(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)
            .map_err(|e| format!("manual state lock: {e}"))?;
        lock.try_lock()
            .map_err(|e| format!("manual state is already in use or cannot be locked: {e}"))?;
        Ok(Self {
            path,
            saved: RefCell::new(None),
            _lock: lock,
        })
    }
    pub fn load(&self, now: u64) -> Result<ManualInhibitors, String> {
        check_file(&self.path)?;
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ManualInhibitors::default());
            }
            Err(e) => return Err(format!("read manual state: {e}")),
        };
        let mut bytes = Vec::new();
        file.take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 1_048_576 {
            return Err("manual state exceeds 1 MiB".into());
        }
        let snapshot: ManualSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid manual state at {}: {e}", self.path.display()))?;
        let restored = ManualInhibitors::restore(snapshot.clone(), now, unix_now()?)?;
        self.saved.replace(Some(snapshot));
        Ok(restored)
    }
    pub fn save(&self, manual: &ManualInhibitors, now: u64) -> Result<(), String> {
        self.save_renewed(manual, now, None)
    }
    pub fn save_renewed(
        &self,
        manual: &ManualInhibitors,
        now: u64,
        renewed: Option<&str>,
    ) -> Result<(), String> {
        let mut snapshot = manual.snapshot(now, unix_now()?)?;
        if let Some(previous) = self.saved.borrow().as_ref() {
            snapshot.preserve_expirations_except(previous, renewed);
        }
        let bytes = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;
        if bytes.len() > 1_048_576 {
            return Err("manual state exceeds 1 MiB".into());
        }
        let temp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| format!("create manual state: {e}"))?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(|e| format!("save manual state: {e}"))?;
        self.saved.replace(Some(snapshot));
        Ok(())
    }
}
fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| format!("manual state clock: {e}"))
}
fn check_file(path: &std::path::Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 => Err(format!(
            "{} must be a private regular file (0600)",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vigil_core::ACTIONS;
    #[test]
    fn atomic_save_restart_clear_and_exclusive_access() {
        let dir = std::env::temp_dir().join(format!(
            "vigil-store-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("manual.json");
        let store = Store::open(path.clone()).unwrap();
        assert!(Store::open(path.clone()).is_err());
        let mut manual = store.load(0).unwrap();
        let id = manual
            .add(0, None, ACTIONS.to_vec(), "Persistent".into())
            .unwrap();
        store.save(&manual, 0).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        let mut restored = store.load(0).unwrap();
        assert_eq!(restored.status(0)[0].id, id);
        restored.clear();
        store.save(&restored, 0).unwrap();
        assert!(store.load(0).unwrap().status(0).is_empty());
        fs::write(&path, "broken").unwrap();
        assert!(store.load(0).unwrap_err().contains("invalid manual state"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken");
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_save_leaves_previous_file_readable() {
        let dir = std::env::temp_dir().join(format!(
            "vigil-store-failure-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("manual.json");
        let store = Store::open(path.clone()).unwrap();
        let mut manual = ManualInhibitors::default();
        store.save(&manual, 0).unwrap();
        manual.toggle();
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        fs::create_dir(&temp).unwrap();
        assert!(store.save(&manual, 0).is_err());
        assert!(store.load(0).unwrap().status(0).is_empty());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
}
