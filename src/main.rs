mod accept;
mod config;
mod elog;
mod http;
mod simulate;
mod store;
mod util;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::accept::{classify_accept, log_accept_resource, resource_backoff, AcceptClass, ConnSlot};
use crate::config::Config;
use crate::http::{handle, App};
use crate::simulate::Simulator;
use crate::store::Store;
use crate::util::{
    Bucket, MAX_CONNECTIONS, MAX_HEADERS, MAX_HEADER_BYTES, REQUEST_BURST, REQUEST_RATE,
    REQUEST_TIMEOUT_SECS,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    let mut stats = false;
    let mut listen_override = None;
    let mut dev = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config_path = args.next().map(PathBuf::from),
            "--listen" => listen_override = args.next(),
            "--dev" => dev = true,
            "--print-stats" => stats = true,
            "--help" => {
                eprintln!("aegis-observatory [--config FILE] [--listen HOST:PORT] [--dev] [--print-stats]");
                eprintln!("--dev is local testing only. AEGIS_SKIP_DISK_CHECK is ignored without it.");
                return;
            }
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
    }
    let mut cfg = Config::load(config_path.as_deref()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    if let Some(l) = listen_override {
        cfg.listen = l;
    }
    if stats {
        print!("{}", crate::store::readonly_stats(&cfg.data_dir, &cfg.db_path()));
        return;
    }
    if dev {
        std::env::set_var("AEGIS_DEV", "1");
    }
    if crate::util::skip_disk_requested() && !dev {
        eprintln!("fatal: AEGIS_SKIP_DISK_CHECK requires --dev and is forbidden in deploy");
        std::process::exit(2);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .thread_name("aegis-http")
        .build()
        .expect("runtime");
    if let Err(e) = rt.block_on(run(cfg)) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run(cfg: Config) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
    crate::elog::ErrorLog::init(cfg.data_dir.join("error.log"));
    if crate::util::skip_disk_allowed() {
        eprintln!("disk_protection=DISABLED (dev-only skip; forbidden in deploy)");
    } else {
        eprintln!("disk_protection=enabled (128MiB free + 16MiB data-dir budget)");
    }
    eprintln!(
        "limits connections={} inflight={} write_inflight={} db_queue={} header_timeout={}s body_timeout={}s keep_alive=off log_cap={}",
        crate::util::env_usize("AEGIS_MAX_CONNECTIONS", crate::util::MAX_CONNECTIONS),
        crate::util::env_usize("AEGIS_MAX_INFLIGHT", crate::util::MAX_INFLIGHT),
        crate::util::env_usize("AEGIS_MAX_WRITE_INFLIGHT", crate::util::MAX_WRITE_INFLIGHT),
        crate::util::env_usize("AEGIS_DB_QUEUE", crate::util::DB_QUEUE),
        crate::util::REQUEST_TIMEOUT_SECS,
        crate::util::BODY_TIMEOUT_SECS,
        crate::util::LOG_CAP_BYTES
    );
    eprintln!(
        "discovery: public_base_url={} bind=loopback A2A=jsonrpc-{} artifacts={} simulate={}",
        cfg.public_base_url,
        crate::http::A2A_PROTOCOL_VERSION,
        cfg.artifacts_enabled,
        cfg.simulate_enabled
    );
    let store = Store::start(cfg.data_dir.clone(), cfg.instance_id.clone())?;
    let sim = Simulator::new(cfg.simulate_enabled, cfg.simulate_canary.clone());
    let app = Arc::new(App {
        store,
        sim: Mutex::new(sim),
        inflight: AtomicUsize::new(0),
        write_inflight: AtomicUsize::new(0),
        requests: Mutex::new(Bucket::new(REQUEST_RATE, REQUEST_BURST)),
        sim_enabled: cfg.simulate_enabled,
        canary: cfg.simulate_canary.clone(),
        public_base_url: cfg.public_base_url.clone(),
    });
    let addr: SocketAddr = cfg
        .listen
        .parse()
        .map_err(|e| format!("listen {}: {e}", cfg.listen))?;
    if !addr.ip().is_loopback() {
        return Err("refuse to bind a non-loopback address".into());
    }
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    eprintln!("aegis-observatory listening on {bound}");
    let conns = Arc::new(AtomicUsize::new(0));
    let max_conn = crate::util::env_usize("AEGIS_MAX_CONNECTIONS", MAX_CONNECTIONS);
    let mut resource_streak = 0u32;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("shutdown");
                break;
            }
            acc = listener.accept() => {
                match acc {
                    Ok((stream, _)) => {
                        resource_streak = 0;
                        let Some(slot) = ConnSlot::try_acquire(&conns, max_conn) else {
                            drop(stream);
                            continue;
                        };
                        let app = app.clone();
                        tokio::spawn(async move {
                            // Drop releases the slot on IO error, client close, handler
                            // return (including body 408), and task cancel. release panic=abort
                            // kills the process, so a leaked slot does not outlive the binary.
                            let _slot = slot;
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let app = app.clone();
                                async move { handle(req, app).await }
                            });
                            let mut http = hyper::server::conn::http1::Builder::new();
                            http.timer(TokioTimer::new())
                                .keep_alive(false)
                                .max_headers(MAX_HEADERS)
                                .max_header_size(MAX_HEADER_BYTES)
                                .header_read_timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                                .max_buf_size(8192);
                            let _ = http.serve_connection(io, svc).await;
                        });
                    }
                    Err(e) => match classify_accept(&e) {
                        AcceptClass::Retry => {}
                        AcceptClass::Resource => {
                            log_accept_resource();
                            tokio::time::sleep(resource_backoff(resource_streak)).await;
                            resource_streak = resource_streak.saturating_add(1);
                        }
                        AcceptClass::Fatal => {
                            return Err(format!("accept fatal: {e}"));
                        }
                    },
                }
            }
        }
    }
    Ok(())
}
