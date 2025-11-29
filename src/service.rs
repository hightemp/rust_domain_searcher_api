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
use tokio::{select, sync::mpsc, time};
use tracing::{error, info, debug};
use hickory_resolver::{TokioAsyncResolver, config::{ResolverConfig, ResolverOpts}};

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
    // Increase channel size for buffering
    let (tx, rx) = mpsc::channel::<String>(10000);

    // DNS Resolver
    let resolver = TokioAsyncResolver::tokio(
        ResolverConfig::google(),
        ResolverOpts::default(),
    );
    let resolver = Arc::new(resolver);

    // Concurrency limiter
    let concurrency = cfg.limits.concurrency.max(1) as usize;
    info!("concurrency: {} workers", concurrency);

    // Pipeline: Generator -> Channel -> Stream -> DNS -> HTTP -> Store
    {
        let store = store.clone();
        let prog = prog.clone();
        let client = client.clone();
        let hc = cfg.http_check.clone();
        let resolver = resolver.clone();
        
        // Convert receiver to stream
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        
        // Process stream with concurrency
        let process_fut = stream.for_each_concurrent(concurrency, move |domain: String| {
            let store = store.clone();
            let prog = prog.clone();
            let client = client.clone();
            let hc = hc.clone();
            let resolver = resolver.clone();
            
            async move {
                // 1. DNS Resolve (Fast Filter)
                let has_ip = match resolver.lookup_ip(&domain).await {
                    Ok(ips) => ips.iter().next().is_some(),
                    Err(_) => false,
                };

                if has_ip {
                    // 2. HTTP Check (Slow Check)
                    if let Ok(ok) = check_domain(&client, &domain, &hc).await {
                        if ok {
                            store.add(&domain);
                            prog.inc_found();
                        }
                    }
                }
                
                prog.inc_checked();
                *last_domain_cell().write() = domain.clone();
            }
        });

        // Spawn processor
        tokio::spawn(process_fut);
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
            let mut ticker = time::interval(Duration::from_secs(5)); // Save every 5s
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

    // Generator Loop
    info!("service entering main loop");
    loop {
        let cfg_gen = cfg.generator.clone();
        let tx_gen = tx.clone();
        let last_for_gen = last_domain_cell();

        select! {
            _ = shutdown.wait() => {
                break;
            }
            res = async {
                let resume_from = last_for_gen.read().clone();
                info!("generator start: resume_from='{}'", resume_from);
                generate_candidates(
                    cfg_gen,
                    resume_from,
                    &tx_gen,
                    &prog,
                    cfg.limits.max_candidates as i64,
                ).await
            } => {
                match res {
                    Ok(sent) => info!("generator finished: enqueued_sent={}", sent),
                    Err(e) => error!("generator error: {e}"),
                }
                if !cfg.run.loop_ {
                    break;
                }
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
            // Short timeout for connection
            let req = client.request(method.clone(), &url).build()?;
            let resp = client.execute(req).await;
            match resp {
                Ok(resp) => {
                    let status = resp.status().as_u16() as i32;
                    // Just check status, don't read body if not needed
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
async fn generate_candidates(
    gen: GeneratorConfig,
    resume_from: String,
    tx: &mpsc::Sender<String>,
    prog: &Progress,
    max_candidates: i64,
) -> anyhow::Result<i64> {
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
    let mut sent: i64 = 0;

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
                    let t = tld.trim().to_lowercase();
                    if t.is_empty() || !t.starts_with('.') {
                        continue;
                    }
                    let domain = format!("{label}{t}");
                    let dl = domain.to_lowercase();
                    if !started {
                        if dl <= resume {
                            if dl == resume {
                                started = true; 
                            }
                            continue;
                        }
                        started = true; 
                    }
                    
                    if tx.send(domain.clone()).await.is_ok() {
                        prog.inc_enqueued();
                        sent += 1;
                        if max_candidates > 0 && sent >= max_candidates {
                            return Ok(sent);
                        }
                    } else {
                        return Ok(sent);
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
    Ok(sent)
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