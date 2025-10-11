use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
};

use parking_lot::RwLock;
use tracing::error;

#[derive(Clone)]
pub struct DomainStore {
    dir: Arc<PathBuf>,
    count: Arc<AtomicI64>,
    locks: Arc<RwLock<std::collections::HashMap<String, Arc<Mutex<()>>>>>,
}

impl DomainStore {
    pub fn new<P: AsRef<Path>>(dir: P) -> anyhow::Result<Self> {
        let p = dir.as_ref();
        fs::create_dir_all(p)?;
        Ok(Self {
            dir: Arc::new(p.to_path_buf()),
            count: Arc::new(AtomicI64::new(0)),
            locks: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    #[inline]
    fn tld_from_domain(domain: &str) -> Option<String> {
        let idx = domain.rfind('.')?;
        if idx == 0 || idx == domain.len() - 1 {
            return None;
        }
        Some(domain[idx + 1..].to_string())
    }

    fn lock_for(&self, tld: &str) -> Arc<Mutex<()>> {
        if let Some(m) = self.locks.read().get(tld) {
            return m.clone();
        }
        let mut w = self.locks.write();
        w.entry(tld.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    #[inline]
    fn path_for(&self, tld: &str) -> PathBuf {
        self.dir.join(format!("{}.txt", tld))
    }

    pub fn add(&self, domain: &str) {
        let Some(tld) = Self::tld_from_domain(domain) else { return };
        if let Err(e) = fs::create_dir_all(&*self.dir) {
            error!("mkdir storage dir error: {e}");
            return;
        }
        let lk = self.lock_for(&tld);
        let _g = lk.lock().unwrap();
        let path = self.path_for(&tld);
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", domain);
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn list(&self, tld: &str) -> Vec<String> {
        let t = tld.trim().to_lowercase();
        if t.is_empty() {
            return vec![];
        }
        let path = self.path_for(&t);
        let Ok(f) = fs::File::open(path) else { return vec![] };
        let reader = BufReader::new(f);
        reader
            .lines()
            .filter_map(|l| l.ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn list_all(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(1024);
        let Ok(entries) = fs::read_dir(&*self.dir) else { return out };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("txt") {
                continue;
            }
            if let Ok(f) = fs::File::open(&path) {
                let reader = BufReader::new(f);
                for line in reader.lines().flatten() {
                    let s = line.trim();
                    if !s.is_empty() {
                        out.push(s.to_string());
                    }
                }
            }
        }
        out
    }

    pub fn approx_bytes(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&*self.dir) else { return 0 };
        let mut total = 0u64;
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            total = total.saturating_add(md.len());
        }
        total
    }

    pub fn total_count(&self) -> i64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn reset(&self, state_file: &str) -> anyhow::Result<()> {
        let entries = fs::read_dir(&*self.dir)?;
        for e in entries {
            if let Ok(ent) = e {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) == Some("txt") {
                    let _ = fs::remove_file(p);
                }
            }
        }
        if !state_file.trim().is_empty() {
            let _ = std::fs::remove_file(state_file);
        }
        self.count.store(0, Ordering::Relaxed);
        Ok(())
    }
}