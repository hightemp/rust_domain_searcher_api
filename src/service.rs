use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::StreamExt;
use parking_lot::RwLock;
use once_cell::sync::OnceCell;
use reqwest::{Client, Method};
use tokio::{select, sync::{mpsc, Semaphore}, time};
use tracing::{error, info, debug, warn};

use crate::config::{Config, GeneratorConfig, HTTPCheckConfig};
use crate::progress::Progress;
use crate::store::DomainStore;

// Public shutdown signal used by main.rs
#[derive(Clone)]
pub struct ShutdownSignal {
    inner: Arc<AtomicU64>,
}
impl ShutdownSignal {
    pub fn new() -> Self {
        Self { inner: Arc::new(AtomicU64::new(0)) }
    }
    pub async fn wait(&self) {
        loop {
            if self.inner.load(Ordering::Relaxed) != 0 {
                break;
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub fn trigger(&self) {
        self.inner.store(1, Ordering::Relaxed);
    }
}

// In-memory last domain for resume tracking
static LAST_DOMAIN: OnceCell<Arc<RwLock<String>>> = OnceCell::new();

fn last_domain_cell() -> Arc<RwLock<String>> {
    LAST_DOMAIN.get_or_init(|| Arc::new(RwLock::new(String::new()))).clone()
}

pub async fn run_service(
    cfg: Config,
    store: DomainStore,
    prog: Progress,
    client: Client,
    shutdown: ShutdownSignal,
) {
    let (tx, mut rx) = mpsc::channel::<String>(std::cmp::max(1, (cfg.limits.concurrency * 2) as usize));

    // RPS limiter (token bucket via semaphore + periodic refill)
    let rps = cfg.limits.rate_per_second.max(1) as usize;
    let limiter = Arc::new(Semaphore::new(0));
    {
        let refill = limiter.clone();
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                // Refill with rps tokens each second (bounded by consumption)
                refill.add_permits(rps);
            }
        });
    }
    info!("rps limiter: {} tokens/sec", rps);

    // Concurrency limiter
    let nw = cfg.limits.concurrency.max(1) as usize;
    let conc = Arc::new(Semaphore::new(nw));
    info!("concurrency limiter: {} workers", nw);
 
    // Single consumer that dispatches per-domain tasks respecting concurrency
    {
        let store = store.clone();
        let prog = prog.clone();
        let limiter = limiter.clone();
        let client = client.clone();
        let hc = cfg.http_check.clone();
        let conc = conc.clone();
        tokio::spawn(async move {
            while let Some(name) = rx.recv().await {
                let permit = conc.clone().acquire_owned().await.unwrap();
                let limiter = limiter.clone();
                let store = store.clone();
                let prog = prog.clone();
                let client = client.clone();
                let hc = hc.clone();
                tokio::spawn(async move {
                    // RPS token
                    let _rpsp = limiter.acquire().await.unwrap();
                    if let Ok(ok) = check_domain(&client, &name, &hc).await {
                        if ok {
                            store.add(&name);
                            prog.inc_found();
                        }
                    }
                    prog.inc_checked();
                    drop(permit);
                });
            }
        });
    }

    // Resume state management
    let state_path = if cfg.storage.state_file.trim().is_empty() {
        Path::new(&cfg.storage.dir).join("state.json")
    } else {
        PathBuf::from(&cfg.storage.state_file)
    };
    info!("resume: enabled={}, state_file={}", cfg.storage.resume, state_path.display());
    let last = last_domain_cell();
    if cfg.storage.resume {
        if let Ok(s) = std::fs::read_to_string(&state_path) {
            if let Ok(st) = serde_json::from_str::<ResumeState>(&s) {
                let ld = st.last_domain.trim().to_string();
                if !ld.is_empty() {
                    info!("resume: loaded last='{}'", ld);
                }
                *last.write() = ld;
                // restore progress counters if present
                if st.enqueued > 0 || st.checked > 0 || st.found > 0 || st.total_planned > 0 {
                    let tp = if st.total_planned > 0 { st.total_planned } else { prog.total_planned() };
                    prog.set_initial(st.enqueued, st.checked, st.found, tp);
                    info!("resume: restored progress enqueued={} checked={} found={} total_planned={}", st.enqueued, st.checked, st.found, tp);
                }
            }
        }
        // periodic saver
        let state_path_clone = state_path.clone();
        let last_for_saver = last.clone();
        let prog_for_saver = prog.clone();
        tokio::spawn(async move {
            let mut prev = String::new();
            let mut ticker = time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let cur = last_for_saver.read().clone();
                if !cur.is_empty() && cur != prev {
                    let _ = save_resume(&state_path_clone, &cur, &prog_for_saver);
                    debug!("resume: saved last='{}'", cur);
                    prev = cur;
                }
            }
        });
    }

    // Generator task
    // generator executed inside the select! loop below to avoid non-Send captures

    // Loop control: race generation against shutdown without spawning non-Send futures
    info!("service entering main loop");
    loop {
        let cfg_gen = cfg.generator.clone();
        let tx_gen = tx.clone();
        let last_for_gen = last_domain_cell();
        let mut sent: i64 = 0;

        select! {
            _ = shutdown.wait() => {
                break;
            }
            _ = async {
                let resume_from = last_for_gen.read().clone();
                info!("generator start: resume_from='{}'", resume_from);
                if let Err(e) = generate_candidates(cfg_gen, resume_from, |d| {
                    // report last domain
                    *last_for_gen.write() = d.to_string();
                    // send candidate (drop silently if channel is full)
                    match tx_gen.try_send(d.to_string()) {
                        Ok(_) => {
                            sent += 1;
                            // track enqueue for stats
                            prog.inc_enqueued();
                            if cfg.limits.max_candidates > 0 && sent >= cfg.limits.max_candidates as i64 {
                                info!("generator hit max_candidates={}, stopping pass", cfg.limits.max_candidates);
                                return false;
                            }
                            true
                        }
                        Err(_) => true,
                    }
                }).await {
                    error!("generator error: {e}");
                } else {
                    info!("generator finished: enqueued_sent={}", sent);
                }
            } => {
                if !cfg.run.loop_ {
                    break;
                }
                // small pause to avoid log noise
                time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    // final save resume
    if cfg.storage.resume {
        let cur = last_domain_cell().read().clone();
        let _ = save_resume(&state_path, &cur, &prog);
    }

    info!("service stopped");
}

async fn check_domain(client: &Client, domain: &str, hc: &HTTPCheckConfig) -> anyhow::Result<bool> {
    let method = if hc.method.trim().is_empty() {
        Method::GET
    } else {
        Method::from_bytes(hc.method.as_bytes()).unwrap_or(Method::GET)
    };
    let schemes = if hc.try_https_first {
        ["https", "http"]
    } else {
        ["http", "https"]
    };

    for _attempt in 0..=hc.retry {
        for scheme in schemes {
            let url = format!("{scheme}://{domain}/");
            let req = client.request(method.clone(), &url).build()?;
            let resp = client.execute(req).await;
            match resp {
                Ok(resp) => {
                    let status = resp.status().as_u16() as i32;
                    // read limited body (stream chunks)
                    let mut stream = resp.bytes_stream();
                    let mut read: u64 = 0;
                    let limit = hc.body_limit.bytes.max(1);
                    while let Some(chunk) = stream.next().await {
                        let bytes = match chunk {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        read = read.saturating_add(bytes.len() as u64);
                        if read >= limit {
                            break;
                        }
                    }
                    if status >= hc.accept_status_min && status <= hc.accept_status_max {
                        debug!("reachable: {} status={}", url, status);
                        return Ok(true);
                    }
                }
                Err(e) => {
                    debug!("request error for {}: {}", url, e);
                }
            }
        }
    }
    Ok(false)
}

// generate labels and domains; resume_from is lexicographic full domain to start after
async fn generate_candidates<F>(gen: GeneratorConfig, resume_from: String, mut emit: F) -> anyhow::Result<()>
where
    F: FnMut(&str) -> bool,
{
    let alpha = if gen.alphabet.is_empty() {
        "abcdefghijklmnopqrstuvwxyz0123456789-".to_string()
    } else {
        gen.alphabet.clone()
    };
    let alpha_chars: Vec<char> = alpha.chars().collect();
    let allowed: std::collections::HashSet<char> = alpha_chars.iter().copied().collect();
    let is_allowed = |c: char| allowed.contains(&c);
    let resume = resume_from.to_lowercase();
    let mut started = resume.is_empty();

    for ln in gen.min_length..=gen.max_length {
        let ln = ln as usize;
        let mut idx = vec![0usize; ln];
        loop {
            // build label
            let mut valid = true;
            let mut prev_hyphen = false;
            let mut label = String::with_capacity(ln);
            for i in 0..ln {
                let r = alpha_chars[idx[i]];
                if !is_allowed(r) {
                    valid = false;
                    break;
                }
                if r == '-' {
                    if !gen.allow_hyphen
                        || (gen.forbid_leading_hyphen && i == 0)
                        || (gen.forbid_trailing_hyphen && i == ln - 1)
                        || (gen.forbid_double_hyphen && prev_hyphen)
                    {
                        valid = false;
                        break;
                    }
                    prev_hyphen = true;
                } else {
                    prev_hyphen = false;
                }
                label.push(r);
            }
            if valid {
                for tld in &gen.tlds {
                    let mut t = tld.trim().to_lowercase();
                    if t.is_empty() || !t.starts_with('.') {
                        continue;
                    }
                    let domain = format!("{label}{t}");
                    let dl = domain.to_lowercase();
                    if !started {
                        if dl <= resume {
                            if dl == resume {
                                started = true; // skip equal, start after it
                            }
                            continue;
                        }
                        started = true; // dl > resume
                    }
                    // update last
                    *last_domain_cell().write() = domain.clone();
                    if !emit(&domain) {
                        return Ok(());
                    }
                }
            }
            // increment odometer
            let mut carry = 1usize;
            let mut i = ln as isize - 1;
            while i >= 0 && carry > 0 {
                let ii = i as usize;
                idx[ii] += carry;
                if idx[ii] >= alpha_chars.len() {
                    idx[ii] = 0;
                    carry = 1;
                } else {
                    carry = 0;
                }
                i -= 1;
            }
            if carry > 0 {
                break;
            }
            // cooperative yield
            tokio::task::yield_now().await;
        }
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ResumeState {
    last_domain: String,
    updated_at_unix: u64,
    #[serde(default)]
    enqueued: i64,
    #[serde(default)]
    checked: i64,
    #[serde(default)]
    found: i64,
    #[serde(default)]
    total_planned: i64,
}

fn save_resume(path: &Path, last: &str, prog: &Progress) -> anyhow::Result<()> {
    if last.trim().is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let (enq, chk, fnd, _elapsed) = prog.snapshot();
    let st = ResumeState {
        last_domain: last.trim().to_string(),
        updated_at_unix: now_unix(),
        enqueued: enq,
        checked: chk,
        found: fnd,
        total_planned: prog.total_planned(),
    };
    let data = serde_json::to_vec(&st)?;
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}