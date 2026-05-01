use crate::dht::DhtHandle;
use crate::storage::Storage;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[derive(Clone)]
struct AppState {
    storage: Arc<Storage>,
    dht: DhtHandle,
    secret_key: String,
}

#[derive(Deserialize)]
struct ListQuery {
    page: Option<u32>,
    page_size: Option<u32>,
    search: Option<String>,
}

#[derive(Deserialize)]
struct QueryBody {
    query: String,
}

pub async fn start_server(storage: Arc<Storage>, dht: DhtHandle, secret_key: String, port: u16) {
    let state = AppState { storage, dht, secret_key };
    let api_routes = Router::new()
        .route("/api/torrents", get(api_torrents))
        .route("/api/stats", get(api_stats))
        .route("/api/query/:infohash", get(api_query_status))
        .route("/api/query", axum::routing::post(api_query_start))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    let app = Router::new()
        .route("/", get(index))
        .route("/api/auth", axum::routing::post(api_auth))
        .merge(api_routes)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    println!("{}", json!({"level":"info","event":"web_server","listen":format!("http://{}", addr)}).to_string());
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let _ = axum::serve(listener, app).await;
}

async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(key) = req.headers().get("X-Access-Key") {
        if key.to_str().map(|k| k == state.secret_key).unwrap_or(false) {
            return Ok(next.run(req).await);
        }
    }
    Err(StatusCode::UNAUTHORIZED)
}

async fn api_auth(State(state): State<AppState>, axum::Json(body): axum::Json<Value>) -> impl IntoResponse {
    let key = body.get("key").and_then(|v| v.as_str()).unwrap_or("");
    if key == state.secret_key {
        (StatusCode::OK, json!({"status":"ok"}).to_string())
    } else {
        (StatusCode::UNAUTHORIZED, json!({"status":"error","message":"密钥错误"}).to_string())
    }.into_response()
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

async fn api_torrents(State(state): State<AppState>, Query(q): Query<ListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(15).clamp(1, 200);
    let search = q.search.unwrap_or_default();

    let result = tokio::task::spawn_blocking(move || state.storage.list(page, page_size, &search)).await;
    match result {
        Ok(Ok((entries, total))) => {
            let rows: Vec<Value> = entries.iter().map(|e| {
                let files: Vec<Value> = serde_json::from_str(&e.files).unwrap_or_default();
                let file_count = files.len() as u32;
                let total_size: i64 = files.iter().filter_map(|f| f.get("length").and_then(|v| v.as_i64())).sum();
                json!({
                    "infohash": e.infohash,
                    "name": e.name,
                    "files": files,
                    "file_count": file_count,
                    "total_size": total_size,
                    "created_at": e.created_at,
                })
            }).collect();
            let total_pages = (total + page_size - 1) / page_size;
            ([(header::CONTENT_TYPE, "application/json")], json!({"rows": rows, "total": total, "page": page, "page_size": page_size, "total_pages": total_pages}).to_string()).into_response()
        }
        _ => ([(header::CONTENT_TYPE, "application/json")], json!({"error": "database error"}).to_string()).into_response(),
    }
}

async fn api_stats(State(state): State<AppState>) -> impl IntoResponse {
    let storage = state.storage.clone();
    let result = tokio::task::spawn_blocking(move || {
        let count = storage.count().unwrap_or(0);
        let db_size = storage.db_size();
        json!({"total": count, "db_size": db_size})
    }).await;
    match result {
        Ok(data) => ([(header::CONTENT_TYPE, "application/json")], data.to_string()).into_response(),
        _ => ([(header::CONTENT_TYPE, "application/json")], json!({"error": "database error"}).to_string()).into_response(),
    }
}

async fn api_query_start(State(state): State<AppState>, axum::Json(body): axum::Json<QueryBody>) -> impl IntoResponse {
    let input = body.query.trim().to_string();
    let infohash = parse_infohash(&input);
    match infohash {
        Some(ih) => {
            let storage = state.storage.clone();
            let ih_clone = ih.clone();
            let existing = tokio::task::spawn_blocking(move || storage.get_by_infohash(&ih_clone)).await;
            if let Ok(Ok(Some(entry))) = existing {
                let files: Vec<Value> = serde_json::from_str(&entry.files).unwrap_or_default();
                let total_size: i64 = files.iter().filter_map(|f| f.get("length").and_then(|v| v.as_i64())).sum();
                return ([(header::CONTENT_TYPE, "application/json")], json!({
                    "status": "found",
                    "infohash": entry.infohash,
                    "name": entry.name,
                    "files": files,
                    "total_size": total_size,
                    "created_at": entry.created_at
                }).to_string()).into_response();
            }
            let dht = state.dht.clone();
            let ih_for_resp = ih.clone();
            tokio::spawn(async move {
                dht.get_peers(&ih).await;
            });
            ([(header::CONTENT_TYPE, "application/json")], json!({"status": "searching","infohash": ih_for_resp}).to_string()).into_response()
        }
        None => ([(header::CONTENT_TYPE, "application/json")], json!({"status":"error","message":"无效的磁力链接或哈希值"}).to_string()).into_response(),
    }
}

async fn api_query_status(State(state): State<AppState>, Path(infohash): Path<String>) -> impl IntoResponse {
    let storage = state.storage.clone();
    let result = tokio::task::spawn_blocking(move || storage.get_by_infohash(&infohash)).await;
    match result {
        Ok(Ok(Some(entry))) => {
            let files: Vec<Value> = serde_json::from_str(&entry.files).unwrap_or_default();
            let total_size: i64 = files.iter().filter_map(|f| f.get("length").and_then(|v| v.as_i64())).sum();
            ([(header::CONTENT_TYPE, "application/json")], json!({
                "status": "found",
                "infohash": entry.infohash,
                "name": entry.name,
                "files": files,
                "total_size": total_size,
                "created_at": entry.created_at
            }).to_string()).into_response()
        }
        _ => ([(header::CONTENT_TYPE, "application/json")], json!({"status": "pending"}).to_string()).into_response(),
    }
}

fn parse_infohash(input: &str) -> Option<String> {
    let s = input.trim();
    if s.starts_with("magnet:") {
        for part in s.strip_prefix("magnet:?").unwrap_or(s).split('&') {
            if let Some(hash) = part.strip_prefix("xt=urn:btih:") {
                let h = hash.to_lowercase();
                if h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(h);
                }
            }
        }
        return None;
    }
    let h = s.to_lowercase();
    if h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(h);
    }
    None
}

static INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh-CN" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>DHT Spider</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
:root{--mono:"SF Mono",Menlo,"Cascadia Code",monospace;--transition:all .25s ease}
[data-theme="dark"]{--bg:#111;--bg2:#181818;--bg3:#222;--border:#2a2a2a;--border-h:#3a3a3a;--text:#d4d4d4;--text2:#888;--text3:#555;--accent:#e0e0e0;--accent2:#bbb;--danger:#c45454;--hover:rgba(255,255,255,.03)}
[data-theme="light"]{--bg:#f5f3ef;--bg2:#fff;--bg3:#fff;--border:#e0dbd5;--border-h:#ccc5bb;--text:#1a1a1a;--text2:#6b6560;--text3:#a09890;--accent:#1a1a1a;--accent2:#3a3530;--danger:#b33a3a;--hover:rgba(0,0,0,.02)}
html{scroll-behavior:smooth}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;background:var(--bg);color:var(--text);min-height:100vh;transition:background .3s,color .3s;line-height:1.5;font-size:13px}
::-webkit-scrollbar{width:5px;height:5px}
::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:var(--border-h);border-radius:3px}

.icon{display:inline-flex;align-items:center;justify-content:center;flex-shrink:0}
.icon svg{width:14px;height:14px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}

.header{position:sticky;top:0;z-index:50;background:var(--bg2);border-bottom:1px solid var(--border);padding:0 20px;display:flex;align-items:center;height:42px;gap:16px;transition:var(--transition)}
.logo{display:flex;align-items:center;gap:8px;text-decoration:none;color:var(--text);font-size:14px;font-weight:600;letter-spacing:-.3px;white-space:nowrap}
.logo .icon svg{width:16px;height:16px}
.header-stats{display:flex;align-items:center;gap:12px;font-size:11px;color:var(--text2)}
.header-stats span{white-space:nowrap}
.header-stats b{color:var(--text);font-weight:600;margin-left:2px}
.search-box{background:var(--bg);border:1px solid var(--border);border-radius:6px;padding:5px 10px 5px 30px;color:var(--text);font-size:12px;width:200px;outline:none;transition:border-color .2s;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='14' height='14' fill='none' stroke='%23888' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'%3E%3Ccircle cx='11' cy='11' r='8'/%3E%3Cpath d='m21 21-4.3-4.3'/%3E%3C/svg%3E");background-repeat:no-repeat;background-position:8px center}
.search-box:focus{border-color:var(--border-h)}
.search-box::placeholder{color:var(--text3)}
.header-right{display:flex;align-items:center;gap:8px;margin-left:auto}
.auto-label{font-size:11px;color:var(--text2);cursor:pointer;display:flex;align-items:center;gap:4px;white-space:nowrap}
.auto-label input{accent-color:var(--text2)}
.theme-btn{background:none;border:1px solid var(--border);color:var(--text2);border-radius:6px;cursor:pointer;padding:4px 6px;display:flex;align-items:center;transition:var(--transition)}
.theme-btn:hover{border-color:var(--border-h);color:var(--text)}
.refresh-btn{background:var(--accent);color:var(--bg);border:none;border-radius:6px;cursor:pointer;padding:5px 12px;font-size:11px;font-weight:600;font-family:inherit;display:flex;align-items:center;gap:4px;transition:var(--transition)}
.refresh-btn:hover{opacity:.85}

table{width:100%;border-collapse:collapse}
thead th{background:var(--bg2);padding:7px 12px;text-align:left;font-size:11px;font-weight:600;color:var(--text3);border-bottom:1px solid var(--border);position:sticky;top:42px;z-index:10;transition:var(--transition);white-space:nowrap}
tbody tr{transition:background .15s}
tbody tr:hover{background:var(--hover)}
td{padding:6px 12px;font-size:12px;vertical-align:middle;border-bottom:1px solid var(--border);transition:var(--transition)}
td.idx{color:var(--text3);font-size:11px;width:40px;text-align:right}
td.name{max-width:420px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
td.hash{font-family:var(--mono);font-size:10px;color:var(--text2);letter-spacing:.2px}
td.hash button{background:none;border:none;color:var(--text3);cursor:pointer;padding:0 0 0 4px;display:inline;vertical-align:middle;transition:color .15s}
td.hash button:hover{color:var(--text)}
td.hash button svg{width:12px;height:12px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round;vertical-align:-1px}
td.size{color:var(--text2);font-variant-numeric:tabular-nums;white-space:nowrap}
td.time{color:var(--text3);font-size:11px;font-variant-numeric:tabular-nums;white-space:nowrap}
td.file-count{font-variant-numeric:tabular-nums;text-align:center}
td.action{white-space:nowrap}
td.action button{background:none;border:1px solid var(--border);color:var(--text2);padding:2px 8px;border-radius:4px;cursor:pointer;font-size:10px;font-family:inherit;transition:var(--transition);vertical-align:middle}
td.action button:hover{border-color:var(--border-h);color:var(--text)}
td.action button svg{width:11px;height:11px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round;vertical-align:-1px}
td.action .detail-btn{color:var(--text);border-color:var(--border-h)}
td.action .detail-btn:hover{background:var(--accent);color:var(--bg);border-color:var(--accent)}
.empty{text-align:center;padding:60px 20px;color:var(--text3);font-size:13px}
.empty svg{width:32px;height:32px;stroke:var(--text3);fill:none;stroke-width:1.5;margin-bottom:12px}

.pagination{display:flex;justify-content:center;align-items:center;gap:4px;padding:12px;background:var(--bg2);border-top:1px solid var(--border);transition:var(--transition)}
.pagination button{background:var(--bg);color:var(--text2);border:1px solid var(--border);padding:4px 10px;border-radius:4px;cursor:pointer;font-size:11px;font-family:inherit;transition:var(--transition)}
.pagination button:hover:not(:disabled){border-color:var(--border-h);color:var(--text)}
.pagination button:disabled{opacity:.2;cursor:default}
.pagination .current{background:var(--accent);color:var(--bg);border-color:var(--accent);font-weight:600}
.pagination .info{color:var(--text3);font-size:11px;margin-left:8px}

.auth-gate{position:fixed;inset:0;background:var(--bg);z-index:999;display:flex;justify-content:center;align-items:center}
.auth-box{background:var(--bg2);border:1px solid var(--border);border-radius:12px;padding:36px 32px;width:360px;max-width:90vw;text-align:center}
.auth-box h2{font-size:18px;font-weight:600;margin-bottom:4px}
.auth-box p{font-size:12px;color:var(--text3);margin-bottom:20px}
.auth-input{width:100%;background:var(--bg);border:1px solid var(--border);border-radius:6px;padding:10px 14px;color:var(--text);font-size:13px;font-family:var(--mono);outline:none;transition:border-color .2s;margin-bottom:16px;text-align:center;letter-spacing:2px}
.auth-input:focus{border-color:var(--border-h)}
.auth-input::placeholder{letter-spacing:0;font-family:inherit}
.auth-submit{width:100%;background:var(--accent);color:var(--bg);border:none;padding:10px;border-radius:6px;cursor:pointer;font-size:14px;font-weight:600;font-family:inherit;transition:opacity .2s}
.auth-submit:hover{opacity:.85}
.auth-submit:disabled{opacity:.4;cursor:default}
.auth-error{color:var(--danger);font-size:12px;margin-top:12px;display:none}

.modal-overlay{display:none;position:fixed;inset:0;background:rgba(0,0,0,.4);z-index:100;justify-content:center;align-items:center}
.modal-overlay.active{display:flex}
.modal{background:var(--bg2);border:1px solid var(--border);border-radius:10px;width:640px;max-width:95vw;max-height:85vh;display:flex;flex-direction:column}
.modal-header{display:flex;justify-content:space-between;align-items:center;padding:12px 16px;border-bottom:1px solid var(--border)}
.modal-header h2{font-size:14px;font-weight:600}
.modal-close{background:none;border:none;color:var(--text3);cursor:pointer;padding:2px 6px;border-radius:4px;display:flex;transition:var(--transition)}
.modal-close:hover{color:var(--text)}
.modal-close svg{width:16px;height:16px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}
.modal-body{padding:16px;overflow-y:auto;flex:1}
.modal-status{text-align:center;color:var(--text2);font-size:13px;padding:20px 0}
.modal-status .spinner{display:inline-block;width:14px;height:14px;border:2px solid var(--border);border-top-color:var(--text);border-radius:50%;animation:spin .7s linear infinite;vertical-align:middle;margin-right:6px}
@keyframes spin{to{transform:rotate(360deg)}}
.result-table{width:100%;border-collapse:collapse;margin-top:8px}
.result-table th{text-align:left;padding:8px 12px;background:var(--bg);color:var(--text3);font-size:11px;font-weight:600;border-bottom:1px solid var(--border);white-space:nowrap}
.result-table td{padding:8px 12px;border-bottom:1px solid var(--border);font-size:12px;vertical-align:top}
.result-name{font-weight:500;word-break:break-all}
.result-hash{font-family:var(--mono);font-size:11px;color:var(--text2)}
.file-list{margin-top:12px}
.file-list-header{background:var(--bg);padding:8px 12px;border-radius:6px 6px 0 0;font-size:12px;font-weight:600;color:var(--text2);display:flex;justify-content:space-between;cursor:pointer;user-select:none}
.file-list-header:hover{color:var(--text)}
.file-list-body{border:1px solid var(--border);border-top:none;border-radius:0 0 6px 6px;max-height:260px;overflow-y:auto}
.file-item{display:flex;justify-content:space-between;padding:5px 12px;border-bottom:1px solid var(--border);font-size:11px}
.file-item:last-child{border-bottom:none}
.file-item .path{color:var(--text2);word-break:break-all}
.file-item .size{color:var(--text3);white-space:nowrap;margin-left:12px;font-variant-numeric:tabular-nums}
.copy-btn{background:none;border:1px solid var(--border);color:var(--text3);padding:2px 8px;border-radius:4px;cursor:pointer;font-size:10px;font-family:inherit;margin-left:6px;transition:var(--transition)}
.copy-btn:hover{color:var(--text);border-color:var(--border-h)}
.error-msg{color:var(--danger);margin-top:12px;text-align:center;font-size:13px}

@media(max-width:768px){
.header{padding:0 12px;gap:8px;height:auto;min-height:42px;flex-wrap:wrap;padding-top:6px;padding-bottom:6px}
.search-box{width:100%;order:10}
td,thead th{padding:5px 8px}
}
</style>
</head>
<body>

<div class="auth-gate" id="authGate">
  <div class="auth-box">
    <h2>DHT Spider</h2>
    <p>请输入访问密钥以继续</p>
    <input class="auth-input" id="authInput" type="password" placeholder="访问密钥" autofocus />
    <button class="auth-submit" id="authSubmit" onclick="tryAuth()">验证</button>
    <div class="auth-error" id="authError">密钥错误，请重试</div>
  </div>
</div>

<div id="mainApp" style="display:none">
<div class="header">
  <a class="logo" href="javascript:void(0)">
    <span class="icon"><svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/></svg></span>
    DHT Spider
  </a>
  <div class="header-stats">
    <span>总计<b id="stat-total">0</b></span>
    <span>页数<b id="stat-pages">0</b></span>
    <span>存储<b id="stat-storage">--</b></span>
  </div>
  <div class="header-right">
    <input class="search-box" id="search" placeholder="搜索种子名称..." />
    <label class="auto-label"><input type="checkbox" id="autoRefresh" checked />自动刷新</label>
    <button class="theme-btn" onclick="toggleTheme()" title="切换主题">
      <span class="icon" id="themeIcon"><svg viewBox="0 0 24 24"><path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z"/></svg></span>
    </button>
    <button class="refresh-btn" onclick="loadData();loadStats()">
      <span class="icon"><svg viewBox="0 0 24 24"><path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/></svg></span>
      刷新
    </button>
  </div>
</div>
<div>
  <table>
    <thead><tr><th>#</th><th>名称</th><th>哈希值</th><th>文件</th><th>大小</th><th>发现时间</th><th>操作</th></tr></thead>
    <tbody id="tbody"></tbody>
  </table>
  <div id="empty" class="empty" style="display:none">
    <svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>
    <div>暂无数据，等待爬取中...</div>
  </div>
</div>
<div class="pagination" id="pagination"></div>

<div class="modal-overlay" id="modalOverlay">
  <div class="modal">
    <div class="modal-header">
      <h2>种子详情</h2>
      <button class="modal-close" onclick="closeModal()"><svg viewBox="0 0 24 24"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg></button>
    </div>
    <div class="modal-body">
      <div id="queryStatus"></div>
      <div id="queryResult"></div>
    </div>
  </div>
</div>
</div>

<script>
let curPage=1,curSearch='',totalPages=0,refreshTimer=null,pollTimer=null;
const IC={
  copy:'<svg viewBox="0 0 24 24"><rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>',
  link:'<svg viewBox="0 0 24 24"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>',
  down:'<svg viewBox="0 0 24 24"><path d="m6 9 6 6 6-6"/></svg>'
};
function updateThemeIcon(){
  let dark=document.documentElement.getAttribute('data-theme')==='dark';
  document.getElementById('themeIcon').innerHTML=dark
    ?'<svg viewBox="0 0 24 24"><path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z"/></svg>'
    :'<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/></svg>';
}
function toggleTheme(){
  let h=document.documentElement,n=h.getAttribute('data-theme')==='dark'?'light':'dark';
  h.setAttribute('data-theme',n);localStorage.setItem('dht_theme',n);updateThemeIcon();
}
(function(){
  let s=localStorage.getItem('dht_theme');
  if(s)document.documentElement.setAttribute('data-theme',s);
  else if(window.matchMedia('(prefers-color-scheme:light)').matches)document.documentElement.setAttribute('data-theme','light');
  updateThemeIcon();
})();
function getKey(){return localStorage.getItem('dht_access_key')||''}
function authHeaders(){return {'X-Access-Key':getKey(),'Content-Type':'application/json'}}
async function tryAuth(){
  let key=document.getElementById('authInput').value.trim();if(!key)return;
  let btn=document.getElementById('authSubmit');btn.disabled=true;
  try{
    let r=await fetch('/api/auth',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({key:key})});
    if(r.ok){localStorage.setItem('dht_access_key',key);document.getElementById('authGate').style.display='none';document.getElementById('mainApp').style.display='block';loadData();loadStats();startAutoRefresh();}
    else{document.getElementById('authError').style.display='block';document.getElementById('authInput').value='';document.getElementById('authInput').focus();}
  }catch(e){document.getElementById('authError').style.display='block'}
  btn.disabled=false;
}
document.getElementById('authInput').addEventListener('keydown',function(e){if(e.key==='Enter')tryAuth()});
(function(){
  let s=localStorage.getItem('dht_access_key');
  if(s){fetch('/api/auth',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({key:s})}).then(r=>{if(r.ok){document.getElementById('authGate').style.display='none';document.getElementById('mainApp').style.display='block';loadData();loadStats();startAutoRefresh();}else{localStorage.removeItem('dht_access_key');document.getElementById('authInput').focus();}}).catch(()=>{document.getElementById('authInput').focus()});}
  else{document.getElementById('authInput').focus()}
})();
function fmtSize(b){if(!b)return'0 B';if(b<1024)return b+' B';if(b<1048576)return(b/1024).toFixed(1)+' KB';if(b<1073741824)return(b/1048576).toFixed(1)+' MB';return(b/1073741824).toFixed(2)+' GB'}
function fmtTime(ts){let d=new Date(ts*1000);return d.getFullYear()+'-'+String(d.getMonth()+1).padStart(2,'0')+'-'+String(d.getDate()).padStart(2,'0')+' '+String(d.getHours()).padStart(2,'0')+':'+String(d.getMinutes()).padStart(2,'0')+':'+String(d.getSeconds()).padStart(2,'0')}
function esc(s){let d=document.createElement('div');d.textContent=s;return d.innerHTML}
async function loadStats(){
  try{let r=await fetch('/api/stats',{headers:authHeaders()});if(r.status===401){handleUnauthorized();return}let d=await r.json();document.getElementById('stat-storage').textContent=fmtSize(d.db_size);}catch(e){console.error(e)}
}
async function loadData(){
  let url='/api/torrents?page='+curPage+(curSearch?'&search='+encodeURIComponent(curSearch):'');
  try{
    let r=await fetch(url,{headers:authHeaders()});if(r.status===401){handleUnauthorized();return}
    let d=await r.json();
    document.getElementById('stat-total').textContent=d.total.toLocaleString();
    document.getElementById('stat-pages').textContent=d.total_pages;
    totalPages=d.total_pages;
    let tbody=document.getElementById('tbody');
    if(d.rows.length===0){tbody.innerHTML='';document.getElementById('empty').style.display='block';}
    else{
      document.getElementById('empty').style.display='none';
      let start=(curPage-1)*d.page_size;
      tbody.innerHTML=d.rows.map((r,i)=>{
        let idx=start+i+1;
        let magnet='magnet:?xt=urn:btih:'+r.infohash;
        if(r.name)magnet+='&dn='+encodeURIComponent(r.name);
        magnet+='&tr='+encodeURIComponent('udp://tracker.opentrackr.org:1337/announce');
        magnet+='&tr='+encodeURIComponent('udp://open.stealth.si:80/announce');
        magnet+='&tr='+encodeURIComponent('udp://tracker.torrent.eu.org:451/announce');
        magnet+='&tr='+encodeURIComponent('udp://exodus.desync.com:6969/announce');
        magnet+='&tr='+encodeURIComponent('udp://tracker.tiny-vps.com:6969/announce');
        return '<tr data-magnet="'+esc(magnet)+'">'+
          '<td class="idx">'+idx+'</td>'+
          '<td class="name" title="'+esc(r.name)+'">'+esc(r.name)+'</td>'+
          '<td class="hash"><span>'+esc(r.infohash.substring(0,12))+'...</span><button onclick="copyText(\''+esc(r.infohash)+'\')" title="复制哈希">'+IC.copy+'</button></td>'+
          '<td class="file-count">'+r.file_count+'</td>'+
          '<td class="size">'+fmtSize(r.total_size)+'</td>'+
          '<td class="time">'+fmtTime(r.created_at)+'</td>'+
          '<td class="action"><button onclick="copyMagnet(this)" title="复制磁力链接">'+IC.link+'</button><button class="detail-btn" onclick="queryRow(\''+esc(r.infohash)+'\')">详情</button></td>'+
        '</tr>';
      }).join('');
    }
    renderPagination(d.total,d.total_pages);
  }catch(e){console.error(e)}
}
function renderPagination(total,tp){
  let el=document.getElementById('pagination');
  if(tp<=1){el.innerHTML='<span class="info">共 '+total.toLocaleString()+' 条</span>';return}
  let b='';
  b+='<button onclick="goPage(1)"'+(curPage===1?' disabled':'')+'>首页</button>';
  b+='<button onclick="goPage('+(curPage-1)+')"'+(curPage===1?' disabled':'')+'>上一页</button>';
  let s=Math.max(1,curPage-3),e=Math.min(tp,curPage+3);
  for(let i=s;i<=e;i++)b+='<button class="'+(i===curPage?'current':'')+'" onclick="goPage('+i+')">'+i+'</button>';
  b+='<button onclick="goPage('+(curPage+1)+')"'+(curPage===tp?' disabled':'')+'>下一页</button>';
  b+='<button onclick="goPage('+tp+')"'+(curPage===tp?' disabled':'')+'>末页</button>';
  b+='<span class="info">共 '+total.toLocaleString()+' 条</span>';
  el.innerHTML=b;
}
function goPage(p){curPage=p;loadData();window.scrollTo({top:0,behavior:'smooth'})}
function startAutoRefresh(){if(refreshTimer)clearInterval(refreshTimer);if(document.getElementById('autoRefresh').checked)refreshTimer=setInterval(function(){loadData();loadStats()},30000)}
document.getElementById('search').addEventListener('keydown',function(e){if(e.key==='Enter'){curSearch=this.value.trim();curPage=1;loadData()}});
document.getElementById('autoRefresh').addEventListener('change',startAutoRefresh);
function handleUnauthorized(){localStorage.removeItem('dht_access_key');document.getElementById('authGate').style.display='flex';document.getElementById('mainApp').style.display='none';document.getElementById('authInput').value='';document.getElementById('authInput').focus();}
function closeModal(){document.getElementById('modalOverlay').classList.remove('active');if(pollTimer){clearInterval(pollTimer);pollTimer=null}}
document.getElementById('modalOverlay').addEventListener('click',function(e){if(e.target===this)closeModal()});
document.addEventListener('keydown',function(e){if(e.key==='Escape')closeModal()});
async function queryRow(infohash){
  document.getElementById('modalOverlay').classList.add('active');
  document.getElementById('queryStatus').innerHTML='<div class="modal-status"><span class="spinner"></span>正在查询中...</div>';
  document.getElementById('queryResult').innerHTML='';
  if(pollTimer){clearInterval(pollTimer);pollTimer=null}
  try{
    let r=await fetch('/api/query',{method:'POST',headers:authHeaders(),body:JSON.stringify({query:infohash})});
    if(r.status===401){handleUnauthorized();return}
    let d=await r.json();
    if(d.status==='found'){document.getElementById('queryStatus').innerHTML='';renderResult(d);return;}
    let attempts=0;
    pollTimer=setInterval(async function(){
      attempts++;
      if(attempts>60){clearInterval(pollTimer);pollTimer=null;document.getElementById('queryStatus').innerHTML='<div class="modal-status">查询超时，元数据可能尚未就绪</div>';return;}
      try{let pr=await fetch('/api/query/'+infohash,{headers:authHeaders()});if(pr.status===401){handleUnauthorized();clearInterval(pollTimer);pollTimer=null;return}let pd=await pr.json();if(pd.status==='found'){clearInterval(pollTimer);pollTimer=null;document.getElementById('queryStatus').innerHTML='';renderResult(pd);}}catch(e){}
    },2000);
  }catch(e){document.getElementById('queryStatus').innerHTML='<div class="error-msg">网络错误</div>';}
}
function renderResult(d){
  let totalSize=d.files.reduce((a,f)=>a+(f.length||0),0);
  let magnet='magnet:?xt=urn:btih:'+d.infohash;
  if(d.name)magnet+='&dn='+encodeURIComponent(d.name);
  magnet+='&tr='+encodeURIComponent('udp://tracker.opentrackr.org:1337/announce');
  magnet+='&tr='+encodeURIComponent('udp://open.stealth.si:80/announce');
  magnet+='&tr='+encodeURIComponent('udp://tracker.torrent.eu.org:451/announce');
  magnet+='&tr='+encodeURIComponent('udp://exodus.desync.com:6969/announce');
  magnet+='&tr='+encodeURIComponent('udp://tracker.tiny-vps.com:6969/announce');
  window._magnetLink=magnet;
  let h='<table class="result-table">';
  h+='<tr><th>名称</th><td class="result-name">'+esc(d.name)+'</td></tr>';
  h+='<tr><th>哈希值</th><td><span class="result-hash">'+esc(d.infohash)+'</span><button class="copy-btn" onclick="copyText(\''+esc(d.infohash)+'\')">复制</button></td></tr>';
  h+='<tr><th>磁力链接</th><td style="word-break:break-all"><code style="font-size:10px;color:var(--text2)">'+esc(magnet)+'</code><button class="copy-btn" onclick="copyText(window._magnetLink)">复制链接</button></td></tr>';
  h+='<tr><th>文件数</th><td>'+d.files.length+' 个文件</td></tr>';
  h+='<tr><th>总大小</th><td style="font-weight:500">'+fmtSize(totalSize)+'</td></tr>';
  h+='<tr><th>发现时间</th><td style="color:var(--text3)">'+fmtTime(d.created_at)+'</td></tr>';
  h+='</table>';
  if(d.files.length>0){
    let sorted=[...d.files].sort((a,b)=>(b.length||0)-(a.length||0));
    h+='<div class="file-list">';
    h+='<div class="file-list-header" onclick="toggleFiles()"><span>文件列表 ('+d.files.length+' 个文件, '+fmtSize(totalSize)+')</span><span id="fileArrow">'+IC.down+'</span></div>';
    h+='<div class="file-list-body" id="fileListBody">';
    for(let f of sorted){let p=f.path?f.path.join('/'):f.name||'';h+='<div class="file-item"><span class="path">'+esc(p)+'</span><span class="size">'+fmtSize(f.length||0)+'</span></div>';}
    h+='</div></div>';
  }
  document.getElementById('queryResult').innerHTML=h;
}
function toggleFiles(){let b=document.getElementById('fileListBody'),a=document.getElementById('fileArrow');if(b.style.display==='none'){b.style.display='block';a.innerHTML=IC.down}else{b.style.display='none';a.innerHTML='<svg viewBox="0 0 24 24"><path d="m9 18 6-6-6-6"/></svg>'}}
function copyMagnet(el){let tr=el.closest('tr');if(tr)copyText(tr.getAttribute('data-magnet'))}
function copyText(t){try{if(navigator.clipboard&&window.isSecureContext){navigator.clipboard.writeText(t).then(function(){showToast('已复制到剪贴板')}).catch(function(){fallbackCopy(t)})}else{fallbackCopy(t)}}catch(e){fallbackCopy(t)}}
function fallbackCopy(t){let a=document.createElement('textarea');a.value=t;a.style.cssText='position:fixed;left:-9999px';document.body.appendChild(a);a.select();try{document.execCommand('copy');showToast('已复制到剪贴板')}catch(e){showToast('复制失败')}document.body.removeChild(a)}
function showToast(msg){
  let t=document.createElement('div');t.textContent=msg;
  t.style.cssText='position:fixed;bottom:20px;left:50%;transform:translateX(-50%) translateY(10px);background:var(--accent);color:var(--bg);padding:6px 18px;border-radius:6px;font-size:12px;font-weight:500;z-index:200;opacity:0;transition:all .25s ease';
  document.body.appendChild(t);requestAnimationFrame(function(){t.style.opacity='1';t.style.transform='translateX(-50%) translateY(0)'});
  setTimeout(function(){t.style.opacity='0';t.style.transform='translateX(-50%) translateY(10px)';setTimeout(function(){t.remove()},250)},1800);
}
</script>
</body>
</html>"##;
