use std::{
    path::{Path, PathBuf},
    sync::Arc,
    collections::HashMap,
};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tokio::io::AsyncWriteExt;

#[derive(Clone)]
pub struct DomainStore {
    dir: Arc<PathBuf>,
    tx: mpsc::Sender<String>,
}

impl DomainStore {
    pub fn new<P: AsRef<Path>>(dir: P) -> anyhow::Result<Self> {
        let p = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&p)?;
        let dir_arc = Arc::new(p);

        let (tx, mut rx) = mpsc::channel::<String>(10000);
        let dir_clone = dir_arc.clone();

        tokio::spawn(async move {
            let mut buffer: HashMap<String, Vec<String>> = HashMap::new();
            let mut last_flush = time::Instant::now();
            // Flush every 2 seconds or if buffer is large
            let flush_interval = Duration::from_secs(2);
            
            loop {
                let timeout = time::sleep_until(last_flush + flush_interval);
                
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(domain) => {
                                if let Some(tld) = Self::extract_tld(&domain) {
                                    buffer.entry(tld).or_default().push(domain);
                                }
                                // Soft limit to trigger flush
                                if buffer.len() > 500 || buffer.values().map(|v| v.len()).sum::<usize>() > 5000 {
                                    Self::flush_buffer(&dir_clone, &mut buffer).await;
                                    last_flush = time::Instant::now();
                                }
                            }
                            None => {
                                // Channel closed
                                Self::flush_buffer(&dir_clone, &mut buffer).await;
                                break;
                            }
                        }
                    }
                    _ = timeout => {
                        if !buffer.is_empty() {
                            Self::flush_buffer(&dir_clone, &mut buffer).await;
                        }
                        last_flush = time::Instant::now();
                    }
                }
            }
        });

        Ok(Self {
            dir: dir_arc,
            tx,
        })
    }

    fn extract_tld(domain: &str) -> Option<String> {
        let idx = domain.rfind('.')?;
        if idx == 0 || idx == domain.len() - 1 {
            return None;
        }
        Some(domain[idx + 1..].to_string())
    }

    async fn flush_buffer(dir: &Path, buffer: &mut HashMap<String, Vec<String>>) {
        for (tld, domains) in buffer.drain() {
            let path = dir.join(format!("{}.txt", tld));
            // Use tokio fs for async writing
            let res = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await;
                
            match res {
                Ok(mut f) => {
                    let mut chunk = String::with_capacity(domains.len() * 20);
                    for d in domains {
                        chunk.push_str(&d);
                        chunk.push('\n');
                    }
                    if let Err(e) = f.write_all(chunk.as_bytes()).await {
                        tracing::error!("failed to write to {}: {}", path.display(), e);
                    }
                }
                Err(e) => {
                    tracing::error!("failed to open {}: {}", path.display(), e);
                }
            }
        }
    }

    pub fn add(&self, domain: &str) {
        let d = domain.to_string();
        let tx = self.tx.clone();
        // We spawn a task to send to channel to avoid blocking the caller
        tokio::spawn(async move {
            let _ = tx.send(d).await;
        });
    }

    pub fn list(&self, tld: &str) -> Vec<String> {
        let t = tld.trim().to_lowercase();
        if t.is_empty() {
            return vec![];
        }
        let path = self.dir.join(format!("{}.txt", t));
        if !path.exists() {
            return vec![];
        }
        if let Ok(f) = std::fs::File::open(path) {
             use std::io::BufRead;
             let reader = std::io::BufReader::new(f);
             return reader.lines().filter_map(|l| l.ok()).collect();
        }
        vec![]
    }

    pub fn list_all(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&*self.dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                    if let Ok(f) = std::fs::File::open(&path) {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(f);
                        for line in reader.lines().flatten() {
                            out.push(line);
                            if out.len() >= 100_000 { // Safety limit
                                return out;
                            }
                        }
                    }
                }
            }
        }
        out
    }

    pub fn approx_bytes(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(&*self.dir) else { return 0 };
        let mut total = 0u64;
        for e in entries.flatten() {
            if let Ok(md) = e.metadata() {
                total += md.len();
            }
        }
        total
    }

    pub fn reset(&self, state_file: &str) -> anyhow::Result<()> {
        let entries = std::fs::read_dir(&*self.dir)?;
        for e in entries {
            if let Ok(ent) = e {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) == Some("txt") {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        if !state_file.trim().is_empty() {
            let _ = std::fs::remove_file(state_file);
        }
        Ok(())
    }
}