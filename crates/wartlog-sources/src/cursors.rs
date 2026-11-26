//! Collector resume positions (`state/cursors.toml`). A cursor is committed only after the records
//! it covers are durable (see `Sink::send_durable`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct CursorStore {
    path: PathBuf,
    map: Arc<Mutex<BTreeMap<String, String>>>,
}

impl CursorStore {
    pub fn open(root: &Path) -> CursorStore {
        let path = root.join("state").join("cursors.toml");
        let map = std::fs::read_to_string(&path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
        CursorStore { path, map: Arc::new(Mutex::new(map)) }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.map.lock().unwrap().get(key).cloned()
    }

    /// Sets and persists a cursor. The lock is held through the write so concurrent setters
    /// never race on the temporary file.
    pub fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut m = self.map.lock().unwrap();
        if m.get(key).map(String::as_str) == Some(value) {
            return Ok(());
        }
        m.insert(key.to_string(), value.to_string());
        self.save(&m)
    }

    /// Removes every cursor whose key starts with `prefix` and is not in `keep`.
    pub fn retain_prefix(&self, prefix: &str, keep: &dyn Fn(&str) -> bool) -> anyhow::Result<()> {
        let mut m = self.map.lock().unwrap();
        let before = m.len();
        m.retain(|k, _| !k.starts_with(prefix) || keep(k));
        if m.len() == before {
            return Ok(());
        }
        self.save(&m)
    }

    fn save(&self, m: &BTreeMap<String, String>) -> anyhow::Result<()> {
        let text = toml::to_string(m)?;
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn persists() {
        let d = tempfile::tempdir().unwrap();
        let c = super::CursorStore::open(d.path());
        c.set("journald:user", "s=abc;i=1").unwrap();
        let c2 = super::CursorStore::open(d.path());
        assert_eq!(c2.get("journald:user").as_deref(), Some("s=abc;i=1"));
    }
}
