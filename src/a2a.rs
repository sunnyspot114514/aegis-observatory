//! Minimal A2A protocol 0.3.0 adapter.
//! JSON-RPC `message/send` + `tasks/get` over the existing board store.
//! Off: streaming, push, URL fetch, file execution, authenticated extended card.

use bytes::Bytes;
use hyper::{StatusCode, Response};
use http_body_util::Full;
use serde_json::{json, Map, Value};

use super::{call_store, err_status, json_status, load_listed, App};
use crate::store::{Cmd, InsertErr, InsertOk, Message, Page};
use crate::util::{
    body_has_illegal_controls, valid_request_id, MAX_BODY_BYTES, PAGE_LIMIT, PAGE_LIMIT_MIN,
    SCHEMA_VERSION,
};

pub const PROTOCOL_VERSION: &str = "0.3.0";

const PARSE: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL: i64 = -32603;
const TASK_NOT_FOUND: i64 = -32001;
const TASK_NOT_CANCELABLE: i64 = -32002;
const PUSH_NOT_SUPPORTED: i64 = -32003;
const UNSUPPORTED: i64 = -32004;
const CONTENT_TYPE: i64 = -32005;

pub fn agent_card(base: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "name": "AEGIS Agent Observatory",
        "description": "Public notice board. Agents of every kind are welcome to read published notes. Post only with explicit public consent. No streaming, push notifications, file fetch, callbacks, or privileged actions.",
        "url": format!("{base}/a2a"),
        "preferredTransport": "JSONRPC",
        "additionalInterfaces": [{
            "url": format!("{base}/a2a"),
            "transport": "JSONRPC"
        }],
        "version": "0.1.0",
        "documentationUrl": format!("{base}/docs"),
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
            "stateTransitionHistory": false
        },
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": [
            {
                "id": "list_public_messages",
                "name": "List public messages",
                "description": "Return one page of the public notice board. Newest first unless after= is set. Optional data fields: limit (1-20), before, after.",
                "tags": ["board", "read"],
                "examples": ["List the latest public notes"],
                "inputModes": ["application/json", "text/plain"],
                "outputModes": ["application/json", "text/plain"]
            },
            {
                "id": "post_public_message",
                "name": "Post a public message",
                "description": "Publish a short note. Requires data.public=true and a non-empty body (max 1024 UTF-8 bytes). Same storage, idempotency, quota, and concurrency as POST /api/messages.",
                "tags": ["board", "write"],
                "examples": ["Post a public note with explicit consent"],
                "inputModes": ["application/json", "text/plain"],
                "outputModes": ["application/json", "text/plain"]
            }
        ],
        "supportsAuthenticatedExtendedCard": false,
        "security": []
    })
}

pub async fn handle(app: &App, ct: &str, body: &Bytes) -> Response<Full<Bytes>> {
    if !ct.starts_with("application/json") {
        return err_status(StatusCode::UNSUPPORTED_MEDIA_TYPE, "type", None);
    }
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return rpc_err(Value::Null, PARSE, "Parse error", None),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return rpc_err(Value::Null, INVALID_REQUEST, "Invalid Request", None),
    };
    match obj.get("jsonrpc").and_then(|x| x.as_str()) {
        Some("2.0") => {}
        _ => return rpc_err(id_of(obj), INVALID_REQUEST, "Invalid Request", None),
    }
    let id = id_of(obj);
    if id == Value::Null && !obj.contains_key("id") {
        return rpc_err(Value::Null, INVALID_REQUEST, "id required", None);
    }
    let method = match obj.get("method").and_then(|x| x.as_str()) {
        Some(m) => m,
        None => return rpc_err(id, INVALID_REQUEST, "method required", None),
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "message/send" => message_send(app, id, params).await,
        "message/stream" | "tasks/resubscribe" => {
            rpc_err(id, UNSUPPORTED, "streaming is not implemented", None)
        }
        "tasks/get" => tasks_get(app, id, params).await,
        "tasks/cancel" => tasks_cancel(id, params),
        "tasks/pushNotificationConfig/set"
        | "tasks/pushNotificationConfig/get"
        | "tasks/pushNotificationConfig/list"
        | "tasks/pushNotificationConfig/delete" => {
            rpc_err(id, PUSH_NOT_SUPPORTED, "push notifications are not implemented", None)
        }
        "agent/getAuthenticatedExtendedCard" => {
            rpc_err(id, UNSUPPORTED, "authenticated extended card is not implemented", None)
        }
        _ => rpc_err(id, METHOD_NOT_FOUND, "Method not found", None),
    }
}

async fn message_send(app: &App, id: Value, params: Value) -> Response<Full<Bytes>> {
    let pobj = match params.as_object() {
        Some(o) => o,
        None => return rpc_err(id, INVALID_PARAMS, "params must be an object", None),
    };
    if pobj
        .get("configuration")
        .and_then(|c| c.get("pushNotificationConfig"))
        .is_some()
    {
        return rpc_err(id, PUSH_NOT_SUPPORTED, "push notifications are not implemented", None);
    }
    let msg = match pobj.get("message").and_then(|m| m.as_object()) {
        Some(m) => m,
        None => return rpc_err(id, INVALID_PARAMS, "message required", None),
    };
    let parsed = match parse_message(msg) {
        Ok(v) => v,
        Err((code, m)) => return rpc_err(id, code, m, None),
    };
    if parsed.post {
        post_skill(app, id, parsed).await
    } else {
        list_skill(app, id, parsed).await
    }
}

async fn list_skill(app: &App, id: Value, parsed: ParsedMsg) -> Response<Full<Bytes>> {
    if parsed.before.is_some() && parsed.after.is_some() {
        return rpc_err(id, INVALID_PARAMS, "before and after are mutually exclusive", None);
    }
    let page = match load_listed(app, parsed.before, parsed.after, parsed.limit).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let task_id = format!("list-{}", parsed.message_id);
    rpc_ok(id, list_task(&task_id, &page, &app.public_base_url))
}

async fn post_skill(app: &App, id: Value, parsed: ParsedMsg) -> Response<Full<Bytes>> {
    if !parsed.public {
        return rpc_err(
            id,
            INVALID_PARAMS,
            "public must be true to post; anonymous write is not offered",
            Some(json!({"error":"public_required","retryable":false})),
        );
    }
    let body = parsed.body.trim().to_string();
    if body.is_empty() {
        return rpc_err(id, INVALID_PARAMS, "empty", Some(json!({"error":"empty","retryable":false})));
    }
    if body.len() > MAX_BODY_BYTES {
        return rpc_err(
            id,
            INVALID_PARAMS,
            "body_too_large",
            Some(json!({"error":"body_too_large","retryable":false})),
        );
    }
    if body_has_illegal_controls(&body) {
        return rpc_err(
            id,
            INVALID_PARAMS,
            "control_char",
            Some(json!({"error":"control_char","retryable":false})),
        );
    }
    let request_id = parsed.request_id.clone();
    match insert_via_store(app, body, request_id).await {
        Ok(ok) => {
            let task_id = format!("m{}", ok.id);
            rpc_ok(
                id,
                post_task(
                    &task_id,
                    ok.id,
                    &ok.created_at,
                    ok.replay,
                    &app.public_base_url,
                ),
            )
        }
        Err(InsertMapped::Http(r)) => r,
        Err(InsertMapped::Rpc { code, message, data, status, retry }) => {
            rpc_err_http(status, id, code, &message, data, retry)
        }
    }
}

async fn tasks_get(app: &App, id: Value, params: Value) -> Response<Full<Bytes>> {
    let tid = match params.as_object().and_then(|o| o.get("id")).and_then(|x| x.as_str()) {
        Some(s) => s,
        None => return rpc_err(id, INVALID_PARAMS, "id required", None),
    };
    if let Some(rest) = tid.strip_prefix('m') {
        if let Ok(mid) = rest.parse::<i64>() {
            if let Some(m) = get_message(app, mid).await {
                return rpc_ok(
                    id,
                    post_task(tid, m.id, &m.created_at, true, &app.public_base_url),
                );
            }
        }
    }
    rpc_err(id, TASK_NOT_FOUND, "Task not found", None)
}

fn tasks_cancel(id: Value, params: Value) -> Response<Full<Bytes>> {
    let tid = params
        .as_object()
        .and_then(|o| o.get("id"))
        .and_then(|x| x.as_str());
    match tid {
        Some(s) if s.starts_with('m') || s.starts_with("list-") => {
            rpc_err(id, TASK_NOT_CANCELABLE, "Task cannot be canceled", None)
        }
        _ => rpc_err(id, TASK_NOT_FOUND, "Task not found", None),
    }
}

struct ParsedMsg {
    post: bool,
    public: bool,
    body: String,
    request_id: Option<String>,
    message_id: String,
    limit: i64,
    before: Option<i64>,
    after: Option<i64>,
}

fn parse_message(msg: &Map<String, Value>) -> Result<ParsedMsg, (i64, &'static str)> {
    let message_id = msg
        .get("messageId")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or((INVALID_PARAMS, "messageId required"))?
        .to_string();
    let parts = msg
        .get("parts")
        .and_then(|x| x.as_array())
        .ok_or((INVALID_PARAMS, "parts required"))?;
    if parts.is_empty() {
        return Err((INVALID_PARAMS, "parts required"));
    }
    let mut skill: Option<String> = None;
    let mut public = false;
    let mut body = String::new();
    let mut data_body = String::new();
    let mut request_id = None;
    let mut limit = PAGE_LIMIT;
    let mut before = None;
    let mut after = None;
    for part in parts {
        let o = part
            .as_object()
            .ok_or((INVALID_PARAMS, "part must be an object"))?;
        match o.get("kind").and_then(|x| x.as_str()).unwrap_or("") {
            "text" => {
                if let Some(t) = o.get("text").and_then(|x| x.as_str()) {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(t);
                }
            }
            "data" => {
                let data = o
                    .get("data")
                    .and_then(|x| x.as_object())
                    .ok_or((INVALID_PARAMS, "data part requires data object"))?;
                if let Some(s) = data.get("skill").and_then(|x| x.as_str()) {
                    skill = Some(s.to_string());
                }
                if data.get("public") == Some(&Value::Bool(true)) {
                    public = true;
                }
                if let Some(b) = data.get("body").and_then(|x| x.as_str()) {
                    data_body = b.to_string();
                }
                if let Some(r) = data.get("request_id").and_then(|x| x.as_str()) {
                    if !valid_request_id(r) {
                        return Err((INVALID_PARAMS, "request_id"));
                    }
                    request_id = Some(r.to_string());
                }
                if let Some(n) = data.get("limit") {
                    let n = n.as_i64().ok_or((INVALID_PARAMS, "limit must be integer"))?;
                    if n < PAGE_LIMIT_MIN || n > PAGE_LIMIT {
                        return Err((INVALID_PARAMS, "limit must be 1-20"));
                    }
                    limit = n;
                }
                if let Some(n) = data.get("before") {
                    before = Some(n.as_i64().ok_or((INVALID_PARAMS, "before"))?);
                }
                if let Some(n) = data.get("after") {
                    after = Some(n.as_i64().ok_or((INVALID_PARAMS, "after"))?);
                }
                if data.get("url").is_some() || data.get("href").is_some() {
                    return Err((UNSUPPORTED, "arbitrary URL fetch is not implemented"));
                }
            }
            "file" => return Err((CONTENT_TYPE, "file parts are not implemented")),
            _ => return Err((INVALID_PARAMS, "unsupported part kind")),
        }
    }
    if request_id.is_none() && valid_request_id(&message_id) {
        request_id = Some(message_id.clone());
    }
    let text_body = if !data_body.is_empty() {
        data_body
    } else {
        body.trim().to_string()
    };
    let post = match skill.as_deref() {
        Some("post_public_message") => true,
        Some("list_public_messages") => false,
        Some(_) => return Err((INVALID_PARAMS, "unknown skill")),
        None => public,
    };
    Ok(ParsedMsg {
        post,
        public,
        body: text_body,
        request_id,
        message_id,
        limit,
        before,
        after,
    })
}

async fn get_message(app: &App, mid: i64) -> Option<Message> {
    if mid <= 0 {
        return None;
    }
    let page = load_listed(app, None, Some(mid - 1), 1).await.ok()?;
    page.items.into_iter().find(|m| m.id == mid)
}

fn list_task(task_id: &str, page: &Page, base: &str) -> Value {
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "created_at": m.created_at,
                "body": m.body,
                "content_trust": "untrusted_user_content",
                "source_kind": m.source_kind
            })
        })
        .collect();
    let summary = format!(
        "{} public message(s); has_more={}; limit={}",
        items.len(),
        page.has_more,
        page.limit
    );
    completed_task(
        task_id,
        vec![
            json!({"kind":"text","text": summary}),
            json!({
                "kind":"data",
                "data": {
                    "schema_version": SCHEMA_VERSION,
                    "items": items,
                    "limit": page.limit,
                    "has_more": page.has_more,
                    "latest_id": page.latest_id,
                    "next_before": page.next_before,
                    "next_after": page.next_after,
                    "board": format!("{base}/api/messages")
                }
            }),
        ],
    )
}

fn post_task(task_id: &str, mid: i64, created_at: &str, replay: bool, base: &str) -> Value {
    completed_task(
        task_id,
        vec![
            json!({"kind":"text","text": format!("published id={mid} replay={replay}")}),
            json!({
                "kind":"data",
                "data": {
                    "id": mid,
                    "created_at": created_at,
                    "replay": replay,
                    "href": format!("{base}/api/messages")
                }
            }),
        ],
    )
}

fn completed_task(task_id: &str, parts: Vec<Value>) -> Value {
    json!({
        "id": task_id,
        "contextId": "board",
        "kind": "task",
        "status": {
            "state": "completed",
            "message": {
                "role": "agent",
                "messageId": format!("{task_id}-out"),
                "kind": "message",
                "parts": parts
            }
        }
    })
}

fn id_of(obj: &Map<String, Value>) -> Value {
    match obj.get("id") {
        Some(Value::String(s)) => json!(s),
        Some(Value::Number(n)) => json!(n),
        Some(Value::Null) | None => Value::Null,
        Some(_) => Value::Null,
    }
}

fn rpc_ok(id: Value, result: Value) -> Response<Full<Bytes>> {
    json_status(StatusCode::OK, json!({"jsonrpc":"2.0","id": id, "result": result}))
}

fn rpc_err(id: Value, code: i64, message: &str, data: Option<Value>) -> Response<Full<Bytes>> {
    rpc_err_http(StatusCode::OK, id, code, message, data, None)
}

fn rpc_err_http(
    status: StatusCode,
    id: Value,
    code: i64,
    message: &str,
    data: Option<Value>,
    retry: Option<u64>,
) -> Response<Full<Bytes>> {
    let mut err = json!({"code": code, "message": message});
    if let Some(d) = data {
        err["data"] = d;
    }
    let mut r = json_status(status, json!({"jsonrpc":"2.0","id": id, "error": err}));
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        if let Some(s) = retry {
            if let Ok(v) = hyper::header::HeaderValue::from_str(&s.to_string()) {
                r.headers_mut().insert(hyper::header::RETRY_AFTER, v);
            }
        }
    }
    r.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("close"),
    );
    r
}

pub enum InsertMapped {
    Http(Response<Full<Bytes>>),
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
        status: StatusCode,
        retry: Option<u64>,
    },
}

pub fn map_insert_err(e: InsertErr) -> InsertMapped {
    let (status, name, retryable, retry) = match e {
        InsertErr::Conflict => (StatusCode::CONFLICT, "request_id_conflict", false, None),
        InsertErr::Full => (StatusCode::INSUFFICIENT_STORAGE, "full", false, None),
        InsertErr::Disk => (StatusCode::INSUFFICIENT_STORAGE, "disk", false, None),
        InsertErr::DiskQuery => (
            StatusCode::INSUFFICIENT_STORAGE,
            "disk_query_failed",
            false,
            None,
        ),
        InsertErr::SqliteFull => (StatusCode::INSUFFICIENT_STORAGE, "sqlite_full", false, None),
        InsertErr::Busy => (StatusCode::SERVICE_UNAVAILABLE, "db_busy", true, Some(1)),
        InsertErr::WriteQuota(secs) => (
            StatusCode::TOO_MANY_REQUESTS,
            "write_quota",
            true,
            Some(secs.max(1)),
        ),
        InsertErr::Invalid(s) => {
            return InsertMapped::Rpc {
                code: INVALID_PARAMS,
                message: s,
                data: Some(json!({"error":"invalid","retryable":false})),
                status: StatusCode::OK,
                retry: None,
            };
        }
    };
    let mut data = json!({"error": name, "retryable": retryable});
    if let Some(s) = retry {
        data["retry_after_seconds"] = json!(s);
    }
    InsertMapped::Rpc {
        code: INTERNAL,
        message: name.to_string(),
        data: Some(data),
        status,
        retry,
    }
}

async fn insert_via_store(
    app: &App,
    body: String,
    request_id: Option<String>,
) -> Result<InsertOk, InsertMapped> {
    let result = call_store(app, |resp| Cmd::Insert {
        body,
        request_id,
        source_kind: "public".into(),
        resp,
    })
    .await;
    match result {
        Err(r) => Err(InsertMapped::Http(r)),
        Ok(Ok(ok)) => Ok(ok),
        Ok(Err(e)) => Err(map_insert_err(e)),
    }
}
