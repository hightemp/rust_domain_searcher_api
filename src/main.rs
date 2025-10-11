mod config;
mod progress;
mod service;
mod store;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use std::{fs, sync::Arc};

use axum::{
    body::Body,
    extract::Path as AxPath,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use clap::Parser;
use config::Config;
use progress::Progress;
use reqwest::Client;
use service::{run_service, ShutdownSignal};
use store::DomainStore;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Rust port of go_domain_searcher_api
#[derive(Parser, Debug)]
#[command(name = "rust_domain_searcher_api", version, author)]
struct Args {
    /// Path to YAML config
    #[arg(long = "config", default_value = "../domain_search.config.yaml")]
    config: String,

    /// Listen address, e.g. :8080 or 0.0.0.0:8080
    #[arg(long = "addr", default_value = ":8080")]
    addr: String,

    /// Reset storage: delete all stored domains (*.txt) and state file, then exit
    #[arg(long = "reset", default_value_t = false)]
    reset: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let args = Args::parse();

    // config
    let cfg: Config = config::load_config(&args.config).await?;
    fs::create_dir_all(&cfg.storage.dir)?;

    // storage
    let store = DomainStore::new(&cfg.storage.dir)?;

    // reset path
    if args.reset {
        store.reset(&cfg.storage.state_file)?;
        info!(
            "reset completed: removed domain files in {} and state {}",
            &cfg.storage.dir, &cfg.storage.state_file
        );
        return Ok(());
    }

    // http client (conservative defaults)
    let client = Client::builder()
        .pool_max_idle_per_host(cfg.limits.concurrency.max(1) as usize)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .timeout(cfg.http_check.timeout)
        .build()?;

    // progress
    let total_planned = (cfg.limits.max_candidates as i64).max(0);
    let prog = Progress::new(total_planned);
    let prog_arc = Arc::new(prog.clone());

    // background service
    let shutdown = ShutdownSignal::new();
    let shutdown_clone = shutdown.clone();
    let svc_cfg = cfg.clone();
    let svc_store = store.clone();
    let svc_client = client.clone();
    // run service as a future (avoid Send requirement of tokio::spawn)
    let svc_fut = run_service(svc_cfg, svc_store, prog, svc_client, shutdown_clone);

    // http routes
    let tlds = Arc::new(cfg.generator.tlds.clone());
    let app = Router::new()
        .route(
            "/stats/",
            get({
                let p = prog_arc.clone();
                let st = store.clone();
                move || stats_handler(p.clone(), st.clone())
            }),
        )
        .route(
            "/domain/*path",
            get({
                let st = store.clone();
                move |path: AxPath<String>| domain_handler(path, st.clone())
            }),
        )
        .route(
            "/tlds/",
            get({
                let tlds = tlds.clone();
                move || tlds_handler(tlds.clone())
            }),
        );

    // bind addr (support :8080)
    let addr_str = if args.addr.starts_with(':') {
        format!("0.0.0.0{}", args.addr)
    } else {
        args.addr.clone()
    };
    let addr: SocketAddr = addr_str.parse().unwrap_or_else(|_| "0.0.0.0:8080".parse().unwrap());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("api listening on {}", addr);

    // graceful shutdown when ctrl-c
    let server = axum::serve(listener, app);
    tokio::select! {
        res = server => {
            if let Err(e) = res {
                error!("server error: {e}");
            }
        }
        _ = svc_fut => {
            info!("service finished");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("signal received, shutting down...");
        }
    }

    // trigger background shutdown and give tasks time to finish
    shutdown.trigger();
    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok(())
}

// ------------------------- HTTP Handlers -------------------------

#[derive(serde::Serialize)]
struct StatsResp {
    elapsed: String,
    eta: String,
    found: i64,
    remaining: i64,
    speed_per_sec: f64,
    efficiency_percent: f64,
    percent: f64,
    generated: i64,
    checked: i64,
    total_planned: i64,
    domains_memory_bytes: u64,
    domains_memory_human: String,
    go_mem_alloc_bytes: u64, // not applicable in Rust; keep 0 for compatibility
}

fn human_bytes(n: u64) -> String {
    const UNIT: u64 = 1024;
    if n < UNIT {
        return format!("{n}B");
    }
    let mut div = UNIT;
    let mut exp = 0;
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    let suffixes = ['K', 'M', 'G', 'T', 'P', 'E'];
    format!("{:.1}{}iB", (n as f64) / (div as f64), suffixes[exp])
}

fn fmt_duration(d: Duration) -> String {
    let secs = (d.as_secs_f64() + 0.5) as i64;
    let mut h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let days = h / 24;
    h %= 24;
    if days > 0 {
        return format!("{days}d {:02}:{:02}:{:02}", h, m, s);
    }
    if h > 0 {
        return format!("{h}:{:02}:{:02}", m, s);
    }
    format!("{:02}:{:02}", m, s)
}

async fn stats_handler(prog: Arc<Progress>, store: DomainStore) -> impl IntoResponse {
    let (enq, chk, fnd, elapsed) = prog.snapshot();
    let elapsed_sec = elapsed.as_secs_f64();
    let speed = if elapsed_sec > 0.0 {
        (chk as f64) / elapsed_sec
    } else {
        0.0
    };
    let total_planned = prog.total_planned();
    let mut remaining: i64 = -1;
    let mut eta = Duration::from_secs(0);
    let percent: f64;
    if total_planned > 0 {
        if chk >= total_planned {
            remaining = 0;
            eta = Duration::from_secs(0);
            percent = 100.0;
        } else {
            remaining = total_planned - chk;
            eta = if speed > 0.0 {
                Duration::from_secs_f64((remaining as f64) / speed)
            } else {
                Duration::from_secs(0)
            };
            percent = (100.0 * (chk as f64) / (total_planned as f64)).min(100.0);
        }
    } else {
        percent = 0.0;
    }
    let eff = if chk > 0 {
        (fnd as f64) / (chk as f64) * 100.0
    } else {
        0.0
    };
    let dom_bytes = store.approx_bytes();
    let resp = StatsResp {
        elapsed: fmt_duration(elapsed),
        eta: if remaining >= 0 {
            fmt_duration(eta)
        } else {
            "-".to_string()
        },
        found: fnd,
        remaining,
        speed_per_sec: speed,
        efficiency_percent: eff,
        percent,
        generated: enq,
        checked: chk,
        total_planned,
        domains_memory_bytes: dom_bytes,
        domains_memory_human: human_bytes(dom_bytes),
        go_mem_alloc_bytes: 0,
    };
    (StatusCode::OK, Json(resp))
}

async fn domain_handler(AxPath(path): AxPath<String>, store: DomainStore) -> Response {
    // Expect path like ru.txt or ru.json or __all__.txt or __all__.json
    if path.is_empty() || path.contains('/') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let dot = path.rfind('.');
    let Some(dot) = dot else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if dot == 0 || dot == path.len() - 1 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let tld = path[..dot].to_lowercase();
    let ext = path[dot + 1..].to_lowercase();

    let list = if tld == "__all__" {
        store.list_all()
    } else {
        store.list(&tld)
    };

    match ext.as_str() {
        "txt" => {
            let body = list.join("\n") + "\n";
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "json" => match serde_json::to_vec(&list) {
            Ok(b) => Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json; charset=utf-8")
                .body(Body::from(b))
                .unwrap(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tlds_handler(cfg_tlds: Arc<Vec<String>>) -> impl IntoResponse {
    let mut uniq = std::collections::BTreeSet::new();
    for t in cfg_tlds.iter() {
        let mut s = t.trim().to_lowercase();
        if s.starts_with('.') {
            s = s[1..].to_string();
        }
        if !s.is_empty() {
            uniq.insert(s);
        }
    }
    let out: Vec<String> = uniq.into_iter().collect();
    (StatusCode::OK, Json(out))
}
