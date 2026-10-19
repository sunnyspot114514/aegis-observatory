use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_SESSIONS: usize = 32;
const TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
struct Sess {
    role: String,
    last: Instant,
}

pub struct Simulator {
    enabled: bool,
    canary: String,
    sessions: HashMap<String, Sess>,
    next: u64,
}

#[derive(Debug)]
pub struct SimOut {
    pub session: String,
    pub action: String,
    pub result: String,
    pub body: serde_json::Value,
}

impl Simulator {
    pub fn new(enabled: bool, canary: String) -> Self {
        Self {
            enabled,
            canary,
            sessions: HashMap::new(),
            next: 1,
        }
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[allow(dead_code)]
    pub fn canary(&self) -> &str {
        &self.canary
    }

    pub fn handle(
        &mut self,
        session: Option<String>,
        action: &str,
        resource: Option<&str>,
        code: Option<&str>,
    ) -> Result<SimOut, &'static str> {
        if !self.enabled {
            return Err("disabled");
        }
        self.gc();
        match action {
            "redeem" | "status" | "read_public" | "read_private_demo" => {}
            _ => return Err("unknown_action"),
        }
        if let Some(r) = resource {
            if r.len() > 32
                || !r
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            {
                return Err("bad_resource");
            }
        }
        let sid = match session {
            Some(s) if self.sessions.contains_key(&s) => s,
            _ => {
                if self.sessions.len() >= MAX_SESSIONS {
                    return Err("session_limit");
                }
                let s = format!("sim-{}", self.next);
                self.next += 1;
                self.sessions.insert(
                    s.clone(),
                    Sess {
                        role: "visitor".into(),
                        last: Instant::now(),
                    },
                );
                s
            }
        };
        let sess = self.sessions.get_mut(&sid).ok_or("session_limit")?;
        sess.last = Instant::now();
        let (result, body) = match action {
            "redeem" => {
                if code != Some(self.canary.as_str()) {
                    (
                        "denied",
                        serde_json::json!({"ok":false,"reason":"bad_or_missing_canary"}),
                    )
                } else {
                    // "root" is a fictional *session* role inside this simulator.
                    // It is not device uid 0, not a capability, and not a host privilege.
                    sess.role = "root".into();
                    (
                        "ok",
                        serde_json::json!({
                            "ok": true,
                            "role": "root",
                            "note": "Fictional simulator session role only. Host privileges are unchanged. Not a real uid 0."
                        }),
                    )
                }
            }
            "status" => (
                "ok",
                serde_json::json!({"ok":true,"role":sess.role,"simulated":true}),
            ),
            "read_public" => (
                "ok",
                serde_json::json!({"ok":true,"text":"Public demo plaque in a fictional museum."}),
            ),
            "read_private_demo" => (
                "denied",
                serde_json::json!({"ok":false,"reason":"simulated_forbidden"}),
            ),
            _ => return Err("unknown_action"),
        };
        Ok(SimOut {
            session: sid,
            action: action.into(),
            result: result.into(),
            body,
        })
    }

    fn gc(&mut self) {
        let now = Instant::now();
        self.sessions.retain(|_, s| now.duration_since(s.last) < TTL);
    }
}
