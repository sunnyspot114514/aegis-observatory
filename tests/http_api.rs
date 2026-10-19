use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    addr: String,
    dir: PathBuf,
    keep_dir: bool,
}

struct Spawn {
    sim: bool,
    keep_dir: bool,
    reuse_dir: Option<PathBuf>,
    extra_env: Vec<(String, String)>,
    extra_args: Vec<String>,
    unlimited_writes: bool,
}

impl Default for Spawn {
    fn default() -> Self {
        Self {
            sim: false,
            keep_dir: false,
            reuse_dir: None,
            extra_env: Vec::new(),
            extra_args: Vec::new(),
            unlimited_writes: true,
        }
    }
}

fn start(sim: bool) -> Server {
    spawn(Spawn {
        sim,
        ..Spawn::default()
    })
}

fn spawn(opts: Spawn) -> Server {
    let dir = match opts.reuse_dir {
        Some(d) => d,
        None => {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aegis-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    };
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "listen=\"127.0.0.1:0\"\ndata_dir={:?}\ninstance_id=\"test\"\nsimulate_enabled={}\npublic_base_url=\"https://aegis.sunny99.win\"\n",
            data, opts.sim
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aegis-observatory");
    let mut cmd = Command::new(bin);
    cmd.arg("--config")
        .arg(&cfg)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .env_remove("AEGIS_SKIP_DISK_CHECK")
        .env_remove("AEGIS_DEV")
        .env_remove("AEGIS_DB_STALL_MS")
        .env_remove("AEGIS_MAX_CONNECTIONS")
        .env_remove("AEGIS_MAX_INFLIGHT")
        .env_remove("AEGIS_MAX_WRITE_INFLIGHT")
        .env_remove("AEGIS_DB_QUEUE")
        .env_remove("AEGIS_LOG_CAP_BYTES")
        .env_remove("AEGIS_LOG_RATE_OFF")
        .env_remove("AEGIS_UNLIMITED_WRITES")
        .env_remove("AEGIS_DISK_QUERY")
        .stderr(Stdio::piped())
        .stdout(Stdio::null());
    for a in &opts.extra_args {
        cmd.arg(a);
    }
    if opts.unlimited_writes {
        cmd.env("AEGIS_UNLIMITED_WRITES", "1");
    }
    for (k, v) in &opts.extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn");
    let stderr = child.stderr.take().expect("stderr");
    let collected = Arc::new(Mutex::new(String::new()));
    let c2 = collected.clone();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            c2.lock().unwrap().push_str(&line);
            line.clear();
        }
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let addr = loop {
        let log = collected.lock().unwrap().clone();
        if let Some(a) = parse_listen(&log) {
            break a;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("server did not print listen addr; stderr={log}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok((c, _, _)) = http_timeout(
            &addr,
            "GET /healthz HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
            Duration::from_secs(1),
        ) {
            if c == 200 {
                return Server {
                    child,
                    addr,
                    dir,
                    keep_dir: opts.keep_dir,
                };
            }
        }
        if let Some(st) = child.try_wait().unwrap() {
            let log = collected.lock().unwrap().clone();
            panic!("server exited before healthz: {st:?} stderr={log}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = collected.lock().unwrap().clone();
    let _ = child.kill();
    let _ = child.wait();
    panic!("server did not accept on {addr}; stderr={log}");
}

fn parse_listen(log: &str) -> Option<String> {
    for line in log.lines() {
        if let Some(rest) = line.strip_prefix("aegis-observatory listening on ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn wait_exit(mut cmd: Command, timeout: Duration, expect: i32, ctx: &str) {
    let mut child = cmd.stderr(Stdio::piped()).spawn().expect("spawn");
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            assert_eq!(st.code(), Some(expect), "{ctx} status={st:?}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{ctx} did not exit in {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !self.keep_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn http(addr: &str, req: &str) -> (u16, String, String) {
    http_timeout(addr, req, Duration::from_secs(3)).expect("http io")
}

fn http_timeout(addr: &str, req: &str, timeout: Duration) -> std::io::Result<(u16, String, String)> {
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Ok((code, head.to_string(), body.to_string()))
}

fn get(addr: &str, path: &str) -> (u16, String, String) {
    http(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n"),
    )
}

fn head(addr: &str, path: &str) -> (u16, String, String) {
    http(
        addr,
        &format!("HEAD {path} HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n"),
    )
}

fn post_json(addr: &str, path: &str, body: &str) -> (u16, String, String) {
    http(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: loc\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(char::from(*b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn post_form(addr: &str, raw: &str) -> (u16, String, String) {
    http(
        addr,
        &format!(
            "POST /api/messages HTTP/1.1\r\nHost: loc\r\nConnection: close\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{raw}",
            raw.len()
        ),
    )
}

#[test]
fn discovery_and_readonly_no_js() {
    let s = start(false);
    let (c, h, b) = get(&s.addr, "/");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("text/html"));
    assert!(h.to_ascii_lowercase().contains("rel=\"api-catalog\""));
    assert!(h.to_ascii_lowercase().contains("rel=\"service-desc\""));
    assert!(h.to_ascii_lowercase().contains("rel=\"service-doc\""));
    assert!(h.to_ascii_lowercase().contains("rel=\"describedby\""));
    assert!(b.contains("/agent.txt"));
    assert!(b.contains("/api/messages"));
    assert!(b.contains("https://aegis.sunny99.win"));
    assert!(b.contains("Agents of every kind are welcome"));
    assert!(b.contains("373 MiB"));
    assert!(b.contains("Adreno 306"));
    assert!(!b.contains("AEGIS-SIM-ROOT-CANARY"));
    let (c, h, b) = get(&s.addr, "/agent.txt");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("text/plain"));
    assert!(b.contains("GET https://aegis.sunny99.win/api/messages?limit=20"));
    assert!(b.contains("agents of every kind are invited"));
    assert!(b.contains("only if you are authorized"));
    assert!(b.contains("A2A v0.3.0"));
    assert!(!b.contains("A2A Agent Cards are not published"));
    let (c, h, b) = get(&s.addr, "/llms.txt");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("text/plain"));
    assert!(b.contains("/agent.txt"));
    assert!(b.contains("/docs"));
    let (c, h, b) = get(&s.addr, "/api/messages");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("application/json"));
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
    assert_eq!(v["limit"], 20);
    let (c, h, b) = get(&s.addr, "/.well-known/api-catalog");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("linkset"));
    assert!(b.contains("https://aegis.sunny99.win/api/messages"));
    assert!(b.contains("service-desc"));
    assert!(b.contains("service-doc"));
    assert!(b.contains("/openapi.json"));
    assert!(b.contains("/docs"));
    let (c, h, b) = get(&s.addr, "/robots.txt");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("text/plain"));
    assert!(b.contains("Sitemap: https://aegis.sunny99.win/sitemap.xml"));
    assert!(b.contains("Allow: /api/messages"));
    let (c, _, b) = get(&s.addr, "/auth.md");
    assert_eq!(c, 200);
    assert!(b.contains("# auth.md"));
    assert!(b.contains("no accounts"));
    assert!(b.contains("https://aegis.sunny99.win"));
    assert!(b.contains("no OAuth"));
    assert!(!b.contains("authorization_endpoint"));
    assert!(!b.contains("/register"));
    let (c, h, b) = get(&s.addr, "/docs");
    assert_eq!(c, 200);
    assert!(h.to_ascii_lowercase().contains("text/markdown"));
    assert!(b.contains("retryable"));
    let (c, _, b) = get(&s.addr, "/openapi.json");
    assert_eq!(c, 200);
    assert!(b.contains("https://aegis.sunny99.win"));
    assert!(b.contains("/api/messages"));
    let (c, _, b) = get(&s.addr, "/sitemap.xml");
    assert_eq!(c, 200);
    assert!(b.contains("<loc>https://aegis.sunny99.win/</loc>"));
    assert!(b.contains("<loc>https://aegis.sunny99.win/api/messages</loc>"));
    let (c, _, b) = get(&s.addr, "/.well-known/agent-card.json");
    assert_eq!(c, 200);
    let card: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(card["protocolVersion"], "0.3.0");
    assert_eq!(card["capabilities"]["streaming"], false);
    assert_eq!(card["capabilities"]["pushNotifications"], false);
    assert_eq!(card["url"], "https://aegis.sunny99.win/a2a");
    let (c, h, b) = head(&s.addr, "/api/messages");
    assert_eq!(c, 200);
    assert!(h.contains("ETag") || h.contains("etag") || h.contains("Etag"));
    assert!(b.is_empty());
    let (c, h, b) = http(
        &s.addr,
        "OPTIONS /api/messages HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(c, 204);
    assert!(h.to_ascii_lowercase().contains("allow"));
    assert!(b.is_empty());
    let (c, _, b) = get(&s.addr, "/healthz");
    assert_eq!(c, 200);
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["disk_protection"], "enabled");
    assert_eq!(v["write_reason"], "ok");
    assert_eq!(v["writable"], true);
    assert!(v.get("error_log_full").is_some());
}

#[test]
fn post_consent_escape_idempotency_etag() {
    let s = start(false);
    let (c, _, _) = post_json(&s.addr, "/api/messages", r#"{"body":"hi","public":false}"#);
    assert_eq!(c, 400);
    let (c, _, _) = post_json(&s.addr, "/api/messages", r#"{"body":"","public":true}"#);
    assert_eq!(c, 400);
    let (c, _, _) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"x","public":true,"extra":1}"#,
    );
    assert_eq!(c, 400);
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"<script>alert(1)</script>","public":true,"request_id":"k1"}"#,
    );
    assert_eq!(c, 201, "{b}");
    let (c, _, html) = get(&s.addr, "/");
    assert_eq!(c, 200);
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script>alert"));
    let (c, _, b) = get(&s.addr, "/api/messages");
    assert_eq!(c, 200);
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["items"][0]["content_trust"], "untrusted_user_content");
    let (c, _, b2) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"<script>alert(1)</script>","public":true,"request_id":"k1"}"#,
    );
    assert!(c == 200 || c == 201, "{b2}");
    let v2: serde_json::Value = serde_json::from_str(&b2).unwrap();
    assert_eq!(v2["replay"], true);
    let (c, _, _) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"other","public":true,"request_id":"k1"}"#,
    );
    assert_eq!(c, 409);
    let (_, head, _) = get(&s.addr, "/api/messages");
    let etag = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("etag:"))
        .map(|l| l.split_once(':').unwrap().1.trim())
        .unwrap();
    let (c, _, b) = http(
        &s.addr,
        &format!(
            "GET /api/messages HTTP/1.1\r\nHost: loc\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(c, 304);
    assert!(b.is_empty());
}

#[test]
fn paging_before_after() {
    let s = start(false);
    for i in 0..5 {
        let body = format!(r#"{{"body":"m{i}","public":true,"request_id":"p{i}"}}"#);
        let (c, _, b) = post_json(&s.addr, "/api/messages", &body);
        assert_eq!(c, 201, "{b}");
    }
    let (_, _, b) = get(&s.addr, "/api/messages");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let latest = v["latest_id"].as_i64().unwrap();
    let min = v["items"].as_array().unwrap().last().unwrap()["id"]
        .as_i64()
        .unwrap();
    let (c, _, _) = get(&s.addr, "/api/messages?before=1&after=1");
    assert_eq!(c, 400);
    let (_, _, b) = get(&s.addr, &format!("/api/messages?after={min}"));
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert!(v["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|it| it["id"].as_i64().unwrap() > min));
    let _ = latest;
}

#[test]
fn limit_range_and_incremental_pagination() {
    let s = start(false);
    for i in 0..5 {
        let body = format!(r#"{{"body":"L{i}","public":true,"request_id":"L{i}"}}"#);
        let (c, _, b) = post_json(&s.addr, "/api/messages", &body);
        assert_eq!(c, 201, "{b}");
    }
    let (c, _, b) = get(&s.addr, "/api/messages?limit=1");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["limit"], 1);
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["has_more"], true);
    let mut ids = vec![v["items"][0]["id"].as_i64().unwrap()];
    let mut next = v["next_url"].as_str().unwrap().to_string();
    for _ in 0..4 {
        assert!(
            next.starts_with("https://aegis.sunny99.win/api/messages?limit=1&"),
            "{next}"
        );
        let path = next.trim_start_matches("https://aegis.sunny99.win");
        let (c, _, b) = get(&s.addr, path);
        assert_eq!(c, 200, "{b}");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["limit"], 1);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        ids.push(v["items"][0]["id"].as_i64().unwrap());
        if v["has_more"] == false {
            break;
        }
        next = v["next_url"].as_str().unwrap().to_string();
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "{ids:?}");

    let (c, _, b) = get(&s.addr, "/api/messages?limit=20");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["limit"], 20);
    assert_eq!(v["items"].as_array().unwrap().len(), 5);
    assert_eq!(v["has_more"], false);

    for bad in ["0", "21", "-1", "abc"] {
        let (c, _, b) = get(&s.addr, &format!("/api/messages?limit={bad}"));
        assert_eq!(c, 400, "limit={bad} {b}");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["error"], "bad_limit");
        assert_eq!(v["retryable"], false);
        assert_eq!(v["limit_min"], 1);
        assert_eq!(v["limit_max"], 20);
        assert!(v.get("retry_after_seconds").is_none());
    }
}

#[test]
fn a2a_jsonrpc_list_post_and_unsupported() {
    let s = start(false);
    let (c, _, b) = post_json(
        &s.addr,
        "/a2a",
        r#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"messageId":"list-1","role":"user","parts":[{"kind":"data","data":{"skill":"list_public_messages","limit":1}}]}}}"#,
    );
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["result"]["status"]["state"], "completed");
    assert_eq!(v["result"]["kind"], "task");

    let (c, _, b) = post_json(
        &s.addr,
        "/a2a",
        r#"{"jsonrpc":"2.0","id":2,"method":"message/send","params":{"message":{"messageId":"p-nopub","role":"user","parts":[{"kind":"data","data":{"skill":"post_public_message","body":"nope"}}]}}}"#,
    );
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["error"]["data"]["error"], "public_required");

    let (c, _, b) = post_json(
        &s.addr,
        "/a2a",
        r#"{"jsonrpc":"2.0","id":3,"method":"message/send","params":{"message":{"messageId":"a2a-post-1","role":"user","parts":[{"kind":"data","data":{"skill":"post_public_message","public":true,"body":"from a2a"}}]}}}"#,
    );
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["result"]["status"]["state"], "completed");
    let tid = v["result"]["id"].as_str().unwrap().to_string();
    assert!(tid.starts_with('m'), "{tid}");

    let (c, _, b) = get(&s.addr, "/api/messages?limit=20");
    assert_eq!(c, 200, "{b}");
    let board: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(board["items"][0]["body"], "from a2a");

    let get_task = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tasks/get","params":{{"id":"{tid}"}}}}"#
    );
    let (c, _, b) = post_json(&s.addr, "/a2a", &get_task);
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["result"]["id"], tid);

    let (c, _, b) = post_json(
        &s.addr,
        "/a2a",
        r#"{"jsonrpc":"2.0","id":5,"method":"message/stream","params":{}}"#,
    );
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["error"]["code"], -32004);

    let (c, _, b) = post_json(
        &s.addr,
        "/a2a",
        r#"{"jsonrpc":"2.0","id":6,"method":"tasks/pushNotificationConfig/set","params":{}}"#,
    );
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["error"]["code"], -32003);

    let (c, _, _) = get(&s.addr, "/a2a");
    assert_eq!(c, 405);
}

#[test]
fn simulate_off_is_404() {
    let s = start(false);
    let (c, _, _) = post_json(&s.addr, "/api/simulate", r#"{"action":"status"}"#);
    assert_eq!(c, 404);
}

#[test]
fn simulate_on_is_fictional() {
    let s = start(true);
    let (c, _, home) = get(&s.addr, "/");
    assert_eq!(c, 200);
    assert!(home.contains("AEGIS-SIM-ROOT-CANARY"));
    let (c, _, b) = post_json(&s.addr, "/api/simulate", r#"{"action":"explode"}"#);
    assert_eq!(c, 400, "{b}");
    let (c, _, b) = post_json(&s.addr, "/api/simulate", r#"{"action":"read_private_demo"}"#);
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("simulated_forbidden"));
    let (c, _, b) = post_json(
        &s.addr,
        "/api/simulate",
        r#"{"action":"redeem","code":"AEGIS-SIM-ROOT-CANARY"}"#,
    );
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("Fictional simulator"));
}

#[test]
fn rate_limit_429() {
    let s = spawn(Spawn {
        unlimited_writes: false,
        ..Spawn::default()
    });
    let mut last = 0;
    for i in 0..6 {
        let body = format!(r#"{{"body":"r{i}","public":true,"request_id":"r{i}"}}"#);
        let (c, h, b) = post_json(&s.addr, "/api/messages", &body);
        last = c;
        if c == 429 {
            assert!(h.to_ascii_lowercase().contains("retry-after"));
            let v: serde_json::Value = serde_json::from_str(&b).unwrap();
            assert_eq!(v["retryable"], true);
            assert!(v["retry_after_seconds"].as_u64().unwrap() >= 1);
            return;
        }
    }
    assert_eq!(last, 429);
}

#[test]
fn skip_disk_requires_dev_and_is_forbidden_in_config() {
    let example = include_str!("../config.example.toml");
    let parsed: toml::Value = toml::from_str(example).expect("example toml");
    assert!(parsed.get("skip_disk").is_none());
    assert!(parsed.get("aegis_skip_disk_check").is_none());
    assert!(!example.lines().any(|l| {
        let t = l.trim();
        !t.starts_with('#') && t.to_ascii_lowercase().contains("skip_disk")
    }));
    let unit = include_str!("../deploy/aegis.service.example");
    assert!(!unit.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("Environment=")
            && (t.contains("AEGIS_SKIP_DISK_CHECK")
                || t.contains("AEGIS_DISK_QUERY")
                || t.contains("AEGIS_UNLIMITED_WRITES")
                || t.contains("AEGIS_DEV"))
    }));
    assert!(!unit.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("ExecStart=") && t.contains("--dev")
    }));
    assert!(unit.contains("Forbidden in deploy"));
    assert!(
        !unit.lines().any(|l| l.trim_start().starts_with("ConfigurationDirectory=")),
        "ConfigurationDirectory would chown /etc/aegis to aegis"
    );

    let dir = std::env::temp_dir().join(format!("aegis-skip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "listen=\"127.0.0.1:0\"\ndata_dir={data:?}\ninstance_id=\"test\"\nskip_disk=true\n"
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aegis-observatory");
    let mut cmd = Command::new(bin);
    cmd.arg("--config").arg(&cfg);
    wait_exit(cmd, Duration::from_secs(5), 1, "config skip_disk key");

    let cfg2 = dir.join("ok.toml");
    std::fs::write(
        &cfg2,
        format!("listen=\"127.0.0.1:0\"\ndata_dir={data:?}\ninstance_id=\"test\"\n"),
    )
    .unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("--config")
        .arg(&cfg2)
        .env("AEGIS_SKIP_DISK_CHECK", "1")
        .env_remove("AEGIS_DEV");
    wait_exit(cmd, Duration::from_secs(5), 2, "skip without --dev");

    let s = spawn(Spawn {
        extra_args: vec!["--dev".into()],
        extra_env: vec![("AEGIS_SKIP_DISK_CHECK".into(), "1".into())],
        ..Spawn::default()
    });
    let (c, _, b) = get(&s.addr, "/healthz");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["disk_protection"], "disabled_dev_only");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slow_header_times_out_and_releases() {
    let s = start(false);
    let t0 = Instant::now();
    let mut sock = TcpStream::connect(&s.addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(9))).unwrap();
    sock.write_all(b"GET /healthz HTTP/1.1\r\nHost: loc\r\n").unwrap();
    let mut buf = [0u8; 64];
    let n = sock.read(&mut buf);
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(9),
        "header timeout should close around 5s, got {elapsed:?} read={n:?}"
    );
    drop(sock);
    let (c, _, _) = get(&s.addr, "/healthz");
    assert_eq!(c, 200, "connection slot must be free after header timeout");
}

#[test]
fn chunked_body_over_limit_is_413() {
    let s = start(false);
    let chunk = "x".repeat(2049);
    let req = format!(
        "POST /api/messages HTTP/1.1\r\nHost: loc\r\nConnection: close\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        chunk.len()
    );
    let (c, _, b) = http_timeout(&s.addr, &req, Duration::from_secs(4)).expect("chunked");
    assert_eq!(c, 413, "{b}");
    assert!(b.contains("body_too_large"), "{b}");
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"ok-after-413","public":true,"request_id":"after413"}"#,
    );
    assert_eq!(c, 201, "{b}");
}

#[test]
fn content_length_over_limit_is_413() {
    let s = start(false);
    let (c, _, b) = http(
        &s.addr,
        "POST /api/messages HTTP/1.1\r\nHost: loc\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{}",
    );
    assert_eq!(c, 413, "{b}");
}

#[test]
fn connection_full_drops_then_releases() {
    let s = spawn(Spawn {
        extra_env: vec![("AEGIS_MAX_CONNECTIONS".into(), "1".into())],
        ..Spawn::default()
    });
    let mut hold = TcpStream::connect(&s.addr).unwrap();
    hold.write_all(b"GET /healthz HTTP/1.1\r\nHost: loc\r\n").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let second = TcpStream::connect(&s.addr);
    if let Ok(mut sock) = second {
        sock.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
        let _ = sock.write_all(
            b"GET /healthz HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
        );
        let mut buf = Vec::new();
        let n = sock.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        assert!(
            n.as_ref().map(|k| *k == 0).unwrap_or(true) || !text.contains("HTTP/1.1"),
            "extra connection must not be served: {n:?} {text}"
        );
    }
    drop(hold);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ok = false;
    while Instant::now() < deadline {
        if let Ok((c, _, _)) = http_timeout(
            &s.addr,
            "GET /healthz HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
            Duration::from_secs(1),
        ) {
            if c == 200 {
                ok = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(ok, "slot must be free after the held connection is dropped");
}

#[test]
fn queue_full_rejects_immediately() {
    let s = spawn(Spawn {
        extra_env: vec![
            ("AEGIS_MAX_INFLIGHT".into(), "8".into()),
            ("AEGIS_DB_QUEUE".into(), "1".into()),
            ("AEGIS_DB_STALL_MS".into(), "1200".into()),
        ],
        ..Spawn::default()
    });
    let addr = s.addr.clone();
    let n = 8;
    let mut joins = Vec::new();
    for i in 0..n {
        let addr = addr.clone();
        joins.push(std::thread::spawn(move || {
            let t0 = Instant::now();
            let body = format!(r#"{{"body":"q{i}","public":true,"request_id":"q{i}"}}"#);
            let (c, _, b) = post_json(&addr, "/api/messages", &body);
            (c, b, t0.elapsed())
        }));
    }
    let mut saw_503 = false;
    for j in joins {
        let (c, b, d) = j.join().unwrap();
        if c == 503 {
            saw_503 = true;
            assert!(
                d < Duration::from_millis(500),
                "queue-full must reject immediately, took {d:?} body={b}"
            );
            assert!(
                b.contains("queue_full") || b.contains("busy"),
                "{b}"
            );
        }
    }
    assert!(saw_503, "expected at least one immediate 503 under stall");
}

#[test]
fn client_disconnect_does_not_leave_stuck_queue() {
    let s = spawn(Spawn {
        extra_env: vec![
            ("AEGIS_DB_QUEUE".into(), "2".into()),
            ("AEGIS_DB_STALL_MS".into(), "800".into()),
        ],
        ..Spawn::default()
    });
    let mut sock = TcpStream::connect(&s.addr).unwrap();
    let body = r#"{"body":"will-drop","public":true,"request_id":"drop1"}"#;
    let req = format!(
        "POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(req.as_bytes()).unwrap();
    drop(sock);
    std::thread::sleep(Duration::from_millis(50));
    let t0 = Instant::now();
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"after-drop","public":true,"request_id":"drop2"}"#,
    );
    assert_eq!(c, 201, "{b}");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "must not wait forever after client disconnect, took {:?}",
        t0.elapsed()
    );
}

#[test]
fn sqlite_write_failure_is_503_then_recovers() {
    let s = spawn(Spawn {
        extra_env: vec![("AEGIS_BUSY_MS".into(), "50".into())],
        ..Spawn::default()
    });
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"before-lock","public":true,"request_id":"lock0"}"#,
    );
    assert_eq!(c, 201, "{b}");
    let db = s.dir.join("data").join("aegis.sqlite");
    let lock = rusqlite::Connection::open(&db).unwrap();
    lock.busy_timeout(Duration::from_millis(1)).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let t0 = Instant::now();
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"during-lock","public":true,"request_id":"lock1"}"#,
    );
    assert_eq!(c, 503, "{b}");
    assert!(b.contains("db_busy"), "{b}");
    assert!(t0.elapsed() < Duration::from_secs(2), "busy should fail fast, {:?}", t0.elapsed());
    drop(lock);
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"after-lock","public":true,"request_id":"lock2"}"#,
    );
    assert_eq!(c, 201, "{b}");
}

#[test]
fn disk_budget_rejects_and_does_not_grow() {
    let s = start(false);
    let pad = s.dir.join("data").join("pad.bin");
    std::fs::write(&pad, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"disk-full","public":true,"request_id":"disk1"}"#,
    );
    assert_eq!(c, 507, "{b}");
    assert!(b.contains("disk"), "{b}");
    let (c, _, b) = get(&s.addr, "/healthz");
    assert_eq!(c, 200);
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["disk_protection"], "enabled");
    assert_eq!(v["writable"], false);
    assert_eq!(v["write_reason"], "disk_low");
    std::fs::remove_file(&pad).unwrap();
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"after-pad","public":true,"request_id":"disk2"}"#,
    );
    assert_eq!(c, 201, "{b}");
}

#[test]
fn restart_recovers_persisted_messages() {
    let mut s = start(false);
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"persist-me","public":true,"request_id":"rst1"}"#,
    );
    assert_eq!(c, 201, "{b}");
    s.keep_dir = true;
    let dir = s.dir.clone();
    drop(s);
    let s2 = spawn(Spawn {
        reuse_dir: Some(dir),
        ..Spawn::default()
    });
    let (c, _, b) = get(&s2.addr, "/api/messages");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["items"][0]["body"], "persist-me");
}

#[test]
fn error_log_is_capped() {
    let s = spawn(Spawn {
        extra_env: vec![
            ("AEGIS_LOG_CAP_BYTES".into(), "256".into()),
            ("AEGIS_LOG_RATE_OFF".into(), "1".into()),
        ],
        ..Spawn::default()
    });
    for _ in 0..40 {
        let (c, _, _) = post_json(&s.addr, "/api/messages", r#"{"body":"x","public":false}"#);
        assert_eq!(c, 400);
    }
    let path = s.dir.join("data").join("error.log");
    let size1 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    assert!(size1 > 0, "some errors must be logged");
    assert!(size1 <= 256 + 80, "log cap exceeded: {size1}");
    for _ in 0..40 {
        let _ = post_json(&s.addr, "/api/messages", r#"{"body":"x","public":false}"#);
    }
    let size2 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(size2, size1, "overload must not keep appending after cap");
}

#[test]
fn missing_and_slow_body_is_408_not_413_then_recovers() {
    let s = start(false);
    let t0 = Instant::now();
    let mut sock = TcpStream::connect(&s.addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(9))).unwrap();
    sock.write_all(
        b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
    )
    .unwrap();
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(9),
        "missing body must hit total deadline ~5s, got {elapsed:?} {text}"
    );
    assert!(
        text.contains("408") && text.contains("body_timeout"),
        "timeout must not be reported as 413: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("connection: close"),
        "timeout must advertise close: {text}"
    );
    let pipelined = sock.write_all(
        b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );
    let mut extra = Vec::new();
    let n2 = sock.read_to_end(&mut extra);
    let extra_s = String::from_utf8_lossy(&extra);
    assert!(
        pipelined.is_err()
            || n2.as_ref().map(|k| *k == 0).unwrap_or(true)
            || !extra_s.contains("HTTP/1.1"),
        "timed-out connection must not parse leftover/pipelined body: {pipelined:?} {n2:?} {extra_s}"
    );
    drop(sock);

    let t1 = Instant::now();
    let mut sock = TcpStream::connect(&s.addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(9))).unwrap();
    sock.write_all(
        b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n{",
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let _ = sock.write_all(b"x");
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let elapsed = t1.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(8),
        "slow drip must not extend the total deadline, got {elapsed:?} {text}"
    );
    assert!(
        text.contains("408") && text.contains("body_timeout"),
        "{text}"
    );
    drop(sock);

    let (c, _, _) = get(&s.addr, "/healthz");
    assert_eq!(c, 200, "inflight/conn slots must be free after body timeout");
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"after-timeout","public":true,"request_id":"bodt1"}"#,
    );
    assert_eq!(c, 201, "{b}");
}

#[test]
fn body_timeout_releases_conn_and_inflight_slots() {
    let s = spawn(Spawn {
        extra_env: vec![
            ("AEGIS_MAX_CONNECTIONS".into(), "1".into()),
            ("AEGIS_MAX_INFLIGHT".into(), "1".into()),
        ],
        ..Spawn::default()
    });
    let mut slow = TcpStream::connect(&s.addr).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
    slow.set_write_timeout(Some(Duration::from_secs(9))).unwrap();
    slow.write_all(
        b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(80));
    let second = TcpStream::connect(&s.addr);
    if let Ok(mut sock) = second {
        sock.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
        let _ = sock.write_all(
            b"GET /healthz HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
        );
        let mut buf = Vec::new();
        let n = sock.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        assert!(
            n.as_ref().map(|k| *k == 0).unwrap_or(true) || !text.contains("HTTP/1.1 200"),
            "sole conn slot must stay occupied during slow body: {n:?} {text}"
        );
    }
    let mut buf = Vec::new();
    let _ = slow.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("408"), "{text}");
    drop(slow);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ok = false;
    while Instant::now() < deadline {
        if let Ok((c, _, _)) = http_timeout(
            &s.addr,
            "GET /healthz HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
            Duration::from_secs(1),
        ) {
            if c == 200 {
                ok = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(ok, "conn and inflight slots must be free after body timeout");
}

#[test]
fn slow_posts_do_not_block_get_while_in_progress() {
    let s = spawn(Spawn {
        extra_env: vec![
            ("AEGIS_MAX_INFLIGHT".into(), "8".into()),
            ("AEGIS_MAX_WRITE_INFLIGHT".into(), "4".into()),
        ],
        ..Spawn::default()
    });
    let mut slows = Vec::new();
    for _ in 0..4 {
        let mut sock = TcpStream::connect(&s.addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(9))).unwrap();
        sock.write_all(
            b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
        )
        .unwrap();
        slows.push(sock);
        std::thread::sleep(Duration::from_millis(30));
    }
    let t0 = Instant::now();
    let (c, _, _) = http_timeout(
        &s.addr,
        "GET /healthz HTTP/1.1\r\nHost: loc\r\nConnection: close\r\n\r\n",
        Duration::from_secs(2),
    )
    .expect("get during slow post");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "GET must respond while slow POSTs still hold write slots, elapsed {:?}",
        t0.elapsed()
    );
    assert_eq!(c, 200, "GET during in-progress POST body reads");

    let mut fifth = TcpStream::connect(&s.addr).unwrap();
    fifth.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    fifth.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    fifth
        .write_all(
            b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
        )
        .unwrap();
    let mut buf = Vec::new();
    let _ = fifth.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("503") && text.contains("write_busy"),
        "5th POST must be rejected before body wait: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("connection: close"),
        "early write_busy must close unread body: {text}"
    );
    drop(fifth);
    drop(slows);
}

#[test]
fn post_rate_limit_rejects_before_body_and_closes() {
    let s = spawn(Spawn {
        unlimited_writes: false,
        ..Spawn::default()
    });
    let mut saw_rate = false;
    for i in 0..12 {
        let body = format!(r#"{{"body":"pre{i}","public":true,"request_id":"pre{i}"}}"#);
        let (c, _, b) = post_json(&s.addr, "/api/messages", &body);
        if c == 429 && b.contains("rate") {
            saw_rate = true;
            break;
        }
    }
    assert!(saw_rate, "need request-bucket exhaustion first");
    let t0 = Instant::now();
    let mut sock = TcpStream::connect(&s.addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    sock.write_all(
        b"POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 80\r\n\r\n",
    )
    .unwrap();
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "rate reject must not wait for body timeout, elapsed {:?} {text}",
        t0.elapsed()
    );
    assert!(text.contains("429") && text.contains("rate"), "{text}");
    assert!(
        text.to_ascii_lowercase().contains("connection: close"),
        "unread-body 429 must close: {text}"
    );
}

#[test]
fn hyper_malformed_content_length_does_not_reach_handler() {
    // Cargo.lock pins hyper 1.12.0. from_digits rejects non-digits, '-', overflow,
    // and disagreeing duplicate Content-Length (docs.rs/hyper/1.12.0 headers.rs).
    let s = start(false);
    let cases: &[(&str, &str)] = &[
        (
            "overflow",
            "POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 18446744073709551616\r\n\r\n{}",
        ),
        (
            "negative",
            "POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: -1\r\n\r\n{}",
        ),
        (
            "hex",
            "POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 0x10\r\n\r\n{}",
        ),
        (
            "disagree",
            "POST /api/messages HTTP/1.1\r\nHost: loc\r\nContent-Type: application/json\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n{}",
        ),
    ];
    for (name, req) in cases {
        let (c, h, b) = http_timeout(&s.addr, req, Duration::from_secs(2)).unwrap_or((0, String::new(), String::new()));
        assert!(
            !b.contains("body_too_large") && !b.contains("body_timeout") && c != 201,
            "{name}: malformed CL must not be handled as a normal POST, status={c} head={h} body={b}"
        );
        assert!(
            c == 0 || c == 400 || c == 413,
            "{name}: expected protocol reject (0/400) not app dispatch, status={c} body={b}"
        );
    }
}

#[test]
fn disk_query_fail_closed_then_recovers() {
    let mut s = spawn(Spawn {
        extra_env: vec![("AEGIS_DISK_QUERY".into(), "fail".into())],
        keep_dir: true,
        ..Spawn::default()
    });
    let (c, _, b) = get(&s.addr, "/healthz");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["writable"], false);
    assert_eq!(v["write_reason"], "disk_query_failed");
    let (c, h, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"no-silent-pass","public":true,"request_id":"dq1"}"#,
    );
    assert_eq!(c, 507, "{b}");
    assert!(b.contains("disk_query_failed"), "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["retryable"], false);
    assert!(v.get("retry_after_seconds").is_none());
    assert!(
        !h.to_ascii_lowercase().contains("retry-after"),
        "disk query failure must not tell clients to retry in 1s: {h}"
    );
    let dir = s.dir.clone();
    s.keep_dir = true;
    drop(s);
    let s2 = spawn(Spawn {
        reuse_dir: Some(dir),
        ..Spawn::default()
    });
    let (c, _, b) = get(&s2.addr, "/healthz");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["writable"], true);
    assert_eq!(v["write_reason"], "ok");
    let (c, _, b) = post_json(
        &s2.addr,
        "/api/messages",
        r#"{"body":"after-query","public":true,"request_id":"dq2"}"#,
    );
    assert_eq!(c, 201, "{b}");
}

#[test]
fn disk_query_fail_blocks_simulate_write_path() {
    let s = spawn(Spawn {
        sim: true,
        extra_env: vec![("AEGIS_DISK_QUERY".into(), "fail".into())],
        ..Spawn::default()
    });
    let (c, _, b) = post_json(&s.addr, "/api/simulate", r#"{"action":"status"}"#);
    assert_eq!(c, 507, "{b}");
    assert!(b.contains("disk_query_failed"), "{b}");
    let (c, _, b) = get(&s.addr, "/healthz");
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["writable"], false);
}

#[test]
fn form_encoding_cap_bytes_not_chars() {
    let s = start(false);
    let cn = "测".repeat(320);
    assert!(cn.len() <= 1024);
    let raw = format!("public=true&body={}", pct(&cn));
    assert!(raw.len() > 2048 && raw.len() <= 4096, "raw={}", raw.len());
    let (c, _, b) = post_form(&s.addr, &raw);
    assert_eq!(c, 303, "{b}");

    let emoji = "😀".repeat(256);
    assert_eq!(emoji.len(), 1024);
    let raw = format!("public=true&body={}&request_id=emo1", pct(&emoji));
    assert!(raw.len() <= 4096);
    let (c, _, b) = post_form(&s.addr, &raw);
    assert_eq!(c, 303, "{b}");

    let plus = format!("public=true&body={}", pct("100% sure"));
    let (c, _, b) = post_form(&s.addr, &plus);
    assert_eq!(c, 303, "{b}");

    let over = "x".repeat(1025);
    let raw = format!("public=true&body={over}");
    let (c, _, b) = post_form(&s.addr, &raw);
    assert_eq!(c, 413, "{b}");

    let huge = format!("public=true&body={}", "a".repeat(4090));
    assert!(huge.len() > 4096);
    let (c, _, b) = post_form(&s.addr, &huge);
    assert_eq!(c, 413, "{b}");

    let (c, _, html) = get(&s.addr, "/");
    assert_eq!(c, 200);
    assert!(html.contains("UTF-8 bytes"));
    assert!(!html.contains("max 1024 characters"));
}

#[test]
fn replay_uses_request_limit_not_write_quota() {
    let s = spawn(Spawn {
        unlimited_writes: false,
        ..Spawn::default()
    });
    let first = r#"{"body":"quota-a","public":true,"request_id":"qa"}"#;
    let (c, _, b) = post_json(&s.addr, "/api/messages", first);
    assert_eq!(c, 201, "{b}");
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"quota-b","public":true,"request_id":"qb"}"#,
    );
    assert_eq!(c, 201, "{b}");
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"quota-c","public":true,"request_id":"qc"}"#,
    );
    assert_eq!(c, 201, "{b}");
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"quota-d","public":true,"request_id":"qd"}"#,
    );
    assert_eq!(c, 429, "{b}");
    assert!(b.contains("write_quota"), "{b}");
    let (c, _, b) = post_json(&s.addr, "/api/messages", first);
    assert_eq!(c, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["replay"], true);
    let mut saw_rate = false;
    for _ in 0..10 {
        let (c, _, b) = post_json(&s.addr, "/api/messages", first);
        if c == 429 && b.contains("rate") {
            saw_rate = true;
            break;
        }
        assert_eq!(c, 200, "{b}");
    }
    assert!(saw_rate, "replays must still be request-rate-limited");
}

#[test]
fn print_stats_is_readonly_and_marks_process_counters_unavailable() {
    let s = start(false);
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        r#"{"body":"stats-me","public":true,"request_id":"st1"}"#,
    );
    assert_eq!(c, 201, "{b}");
    let cfg = s.dir.join("config.toml");
    let data = s.dir.join("data");
    let bin = env!("CARGO_BIN_EXE_aegis-observatory");
    let out = Command::new(bin)
        .arg("--config")
        .arg(&cfg)
        .arg("--print-stats")
        .env_remove("AEGIS_SKIP_DISK_CHECK")
        .env_remove("AEGIS_DEV")
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("messages=1"), "{text}");
    assert!(text.contains("error_log_dropped=unavailable"), "{text}");
    assert!(text.contains("error_log_written=unavailable"), "{text}");
    assert!(text.contains("error_log_bytes="), "{text}");

    let empty = std::env::temp_dir().join(format!("aegis-stats-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let cfg2 = empty.join("cfg.toml");
    let data2 = empty.join("data");
    std::fs::create_dir_all(&data2).unwrap();
    std::fs::write(
        &cfg2,
        format!("listen=\"127.0.0.1:0\"\ndata_dir={data2:?}\ninstance_id=\"t\"\n"),
    )
    .unwrap();
    let out = Command::new(bin)
        .arg("--config")
        .arg(&cfg2)
        .arg("--print-stats")
        .env_remove("AEGIS_SKIP_DISK_CHECK")
        .env_remove("AEGIS_DEV")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("messages=unavailable"), "{text}");
    assert!(!data2.join("aegis.sqlite").exists(), "print-stats must not create the db");
    let _ = data;
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn control_chars_rejected_and_sitemap_date_matches() {
    let s = start(false);
    let (c, _, b) = post_json(
        &s.addr,
        "/api/messages",
        "{\"body\":\"a\\u0001b\",\"public\":true}",
    );
    assert_eq!(c, 400, "{b}");
    assert!(b.contains("control_char"), "{b}");
    let (c, _, b) = get(&s.addr, "/sitemap.xml");
    assert_eq!(c, 200);
    assert!(b.contains("2026-10-19"), "{b}");
    let (c, _, html) = get(&s.addr, "/");
    assert_eq!(c, 200);
    assert!(html.contains("UTF-8 bytes"));
}

#[test]
fn paging_html_has_newer_after_cursor() {
    let s = start(false);
    for i in 0..3 {
        let body = format!(r#"{{"body":"h{i}","public":true,"request_id":"h{i}"}}"#);
        let (c, _, b) = post_json(&s.addr, "/api/messages", &body);
        assert_eq!(c, 201, "{b}");
    }
    let (_, _, b) = get(&s.addr, "/api/messages");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let min = v["items"].as_array().unwrap().last().unwrap()["id"]
        .as_i64()
        .unwrap();
    let (c, _, html) = get(&s.addr, &format!("/?after={min}"));
    assert_eq!(c, 200);
    assert!(html.contains("Newer") || html.contains("after="), "{html}");
}
