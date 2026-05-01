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
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>DHT Spider</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif;background:#f5f5f5;color:#333;font-size:13px;line-height:1.45}
::-webkit-scrollbar{width:6px;height:6px}
::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:#ccc;border-radius:3px}

.navbar{background:#313131;padding:0 20px;display:flex;align-items:center;height:42px;position:sticky;top:0;z-index:50;gap:14px}
.navbar-brand{color:#fff;font-size:15px;font-weight:700;text-decoration:none;letter-spacing:-.3px;white-space:nowrap;display:flex;align-items:center;gap:6px}
.navbar-brand svg{width:16px;height:16px;fill:none;stroke:#fff;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}
.navbar-stats{display:flex;gap:14px;font-size:11px;color:#888}
.navbar-stats b{color:#ccc;font-weight:600;margin-left:2px}
.navbar-right{display:flex;align-items:center;gap:8px;margin-left:auto}
.search-input{background:#484848;border:1px solid #555;border-radius:4px;padding:5px 10px 5px 28px;color:#eee;font-size:12px;width:240px;outline:none;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='13' height='13' fill='none' stroke='%23999' stroke-width='2'%3E%3Ccircle cx='11' cy='11' r='8'/%3E%3Cpath d='m21 21-4.3-4.3'/%3E%3C/svg%3E");background-repeat:no-repeat;background-position:8px center;transition:border-color .2s}
.search-input:focus{border-color:#777}
.search-input::placeholder{color:#777}
.nav-check{font-size:11px;color:#888;cursor:pointer;display:flex;align-items:center;gap:3px;white-space:nowrap}
.nav-check input{accent-color:#6a6}
.nav-btn{background:#484848;border:1px solid #555;color:#ccc;border-radius:4px;cursor:pointer;padding:4px 10px;font-size:11px;font-family:inherit;display:flex;align-items:center;gap:4px;transition:background .15s}
.nav-btn:hover{background:#555;color:#fff}
.nav-btn svg{width:13px;height:13px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}

.container{max-width:1100px;margin:0 auto;padding:8px 0}
table{width:100%;border-collapse:collapse;background:#fff}
thead th{background:#e8e8e8;padding:6px 10px;text-align:left;font-size:11px;font-weight:700;color:#555;border-bottom:2px solid #d5d5d5;white-space:nowrap;user-select:none}
tbody tr:nth-child(even){background:#f7f7f7}
tbody tr:hover{background:#e8f0fe}
td{padding:5px 10px;font-size:12px;vertical-align:middle;border-bottom:1px solid #e8e8e8}
td.c1{color:#999;width:38px;text-align:center;font-size:11px}
td.c2{max-width:420px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
td.c2 a{color:#337ab7;text-decoration:none;font-weight:500}
td.c2 a:hover{text-decoration:underline;color:#23527c}
td.c3{font-family:"SF Mono",Menlo,Consolas,monospace;font-size:10px;color:#888;white-space:nowrap}
td.c3 button{background:none;border:none;color:#bbb;cursor:pointer;padding:0 0 0 4px;vertical-align:middle;transition:color .15s}
td.c3 button:hover{color:#333}
td.c3 button svg{width:11px;height:11px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round;vertical-align:-1px}
td.c4{text-align:center;font-size:11px;color:#5a5;font-weight:600}
td.c5{color:#555;font-variant-numeric:tabular-nums;white-space:nowrap;font-size:11px}
td.c6{color:#999;font-size:11px;white-space:nowrap;font-variant-numeric:tabular-nums}
td.c7{white-space:nowrap}
td.c7 button{background:none;border:1px solid #d5d5d5;color:#888;padding:2px 7px;border-radius:3px;cursor:pointer;font-size:10px;font-family:inherit;vertical-align:middle;transition:all .15s}
td.c7 button:hover{border-color:#999;color:#333}
td.c7 button svg{width:11px;height:11px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round;vertical-align:-1px}
td.c7 .btn-d{color:#313131;border-color:#aaa;font-weight:600}
td.c7 .btn-d:hover{background:#313131;color:#fff;border-color:#313131}
.empty{text-align:center;padding:60px 20px;color:#bbb;font-size:13px;background:#fff}

.pagination{display:flex;justify-content:center;align-items:center;gap:3px;padding:10px}
.pagination button{background:#fff;color:#555;border:1px solid #d5d5d5;padding:4px 10px;border-radius:3px;cursor:pointer;font-size:11px;font-family:inherit;transition:all .15s}
.pagination button:hover:not(:disabled){background:#313131;color:#fff;border-color:#313131}
.pagination button:disabled{opacity:.3;cursor:default}
.pagination .pg-active{background:#313131;color:#fff;border-color:#313131;font-weight:700}
.pagination .pg-info{color:#999;font-size:11px;margin-left:8px}

.auth-gate{position:fixed;inset:0;background:#f5f5f5;z-index:999;display:flex;justify-content:center;align-items:center}
.auth-box{background:#fff;border:1px solid #ddd;border-radius:8px;padding:36px 32px;width:340px;max-width:90vw;text-align:center;box-shadow:0 2px 8px rgba(0,0,0,.06)}
.auth-box h2{font-size:18px;font-weight:700;color:#313131;margin-bottom:4px}
.auth-box p{font-size:12px;color:#999;margin-bottom:20px}
.auth-input{width:100%;background:#f9f9f9;border:1px solid #ddd;border-radius:4px;padding:10px 14px;color:#333;font-size:13px;font-family:"SF Mono",Menlo,monospace;outline:none;margin-bottom:14px;text-align:center;letter-spacing:2px;transition:border-color .2s}
.auth-input:focus{border-color:#313131}
.auth-input::placeholder{letter-spacing:0;font-family:inherit}
.auth-submit{width:100%;background:#313131;color:#fff;border:none;padding:10px;border-radius:4px;cursor:pointer;font-size:14px;font-weight:700;font-family:inherit;transition:background .15s}
.auth-submit:hover{background:#444}
.auth-submit:disabled{opacity:.4;cursor:default}
.auth-error{color:#c44;font-size:12px;margin-top:10px;display:none}

.modal-overlay{display:none;position:fixed;inset:0;background:rgba(0,0,0,.35);z-index:100;justify-content:center;align-items:center}
.modal-overlay.active{display:flex}
.modal{background:#fff;border:1px solid #ddd;border-radius:6px;width:640px;max-width:95vw;max-height:85vh;display:flex;flex-direction:column;box-shadow:0 4px 16px rgba(0,0,0,.1)}
.modal-header{display:flex;justify-content:space-between;align-items:center;padding:10px 14px;border-bottom:1px solid #eee;background:#fafafa;border-radius:6px 6px 0 0}
.modal-header h2{font-size:14px;font-weight:700;color:#313131}
.modal-close{background:none;border:none;color:#999;cursor:pointer;padding:2px 6px;border-radius:3px;display:flex}
.modal-close:hover{color:#333}
.modal-close svg{width:15px;height:15px;stroke:currentColor;fill:none;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}
.modal-body{padding:14px;overflow-y:auto;flex:1}
.modal-status{text-align:center;color:#999;font-size:13px;padding:20px 0}
.spinner{display:inline-block;width:14px;height:14px;border:2px solid #ddd;border-top-color:#313131;border-radius:50%;animation:spin .7s linear infinite;vertical-align:middle;margin-right:6px}
@keyframes spin{to{transform:rotate(360deg)}}

.rtable{width:100%;border-collapse:collapse;margin-top:6px}
.rtable th{text-align:left;padding:7px 10px;background:#f7f7f7;color:#666;font-size:11px;font-weight:700;border-bottom:1px solid #eee;white-space:nowrap}
.rtable td{padding:7px 10px;border-bottom:1px solid #f0f0f0;font-size:12px;vertical-align:top}
.rname{font-weight:600;word-break:break-all}
.rhash{font-family:"SF Mono",Menlo,monospace;font-size:10px;color:#888}
.flist{margin-top:10px}
.flist-h{background:#f7f7f7;padding:7px 10px;border-radius:4px 4px 0 0;font-size:12px;font-weight:600;color:#666;display:flex;justify-content:space-between;cursor:pointer;user-select:none;border:1px solid #eee;border-bottom:none}
.flist-h:hover{color:#333}
.flist-b{border:1px solid #eee;border-top:none;border-radius:0 0 4px 4px;max-height:260px;overflow-y:auto}
.fitem{display:flex;justify-content:space-between;padding:4px 10px;border-bottom:1px solid #f5f5f5;font-size:11px}
.fitem:last-child{border-bottom:none}
.fitem .fp{color:#888;word-break:break-all}
.fitem .fs{color:#aaa;white-space:nowrap;margin-left:12px;font-variant-numeric:tabular-nums}
.copy-btn{background:none;border:1px solid #ddd;color:#888;padding:2px 7px;border-radius:3px;cursor:pointer;font-size:10px;font-family:inherit;margin-left:6px;transition:all .15s}
.copy-btn:hover{color:#333;border-color:#999}
.error-msg{color:#c44;margin-top:10px;text-align:center;font-size:13px}

.toast{position:fixed;bottom:20px;left:50%;transform:translateX(-50%) translateY(10px);background:#313131;color:#fff;padding:7px 20px;border-radius:4px;font-size:12px;font-weight:500;z-index:200;opacity:0;transition:all .25s ease}
.toast.show{opacity:1;transform:translateX(-50%) translateY(0)}

@media(max-width:768px){
.navbar{flex-wrap:wrap;height:auto;padding:6px 12px;gap:6px}
.search-input{width:100%;order:10}
.container{padding:4px}
td,thead th{padding:4px 6px}
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
<div class="navbar">
  <a class="navbar-brand" href="javascript:void(0)">
    <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/></svg>
    DHT Spider
  </a>
  <div class="navbar-stats">
    <span>总计<b id="stat-total">0</b></span>
    <span>页数<b id="stat-pages">0</b></span>
    <span>存储<b id="stat-storage">--</b></span>
  </div>
  <div class="navbar-right">
    <input class="search-input" id="search" placeholder="搜索种子名称..." />
    <label class="nav-check"><input type="checkbox" id="autoRefresh" checked />自动刷新</label>
    <button class="nav-btn" onclick="loadData();loadStats()">
      <svg viewBox="0 0 24 24"><path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/></svg>
      刷新
    </button>
  </div>
</div>
<div class="container">
  <table>
    <thead><tr><th style="width:38px">#</th><th>名称</th><th>哈希值</th><th style="width:48px">文件</th><th>大小</th><th>发现时间</th><th>操作</th></tr></thead>
    <tbody id="tbody"></tbody>
  </table>
  <div id="empty" class="empty" style="display:none">暂无数据，等待爬取中...</div>
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
var curPage=1,curSearch='',totalPages=0,refreshTimer=null,pollTimer=null;
var IC={
  copy:'<svg viewBox="0 0 24 24"><rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>',
  link:'<svg viewBox="0 0 24 24"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>',
  down:'<svg viewBox="0 0 24 24"><path d="m6 9 6 6 6-6"/></svg>',
  right:'<svg viewBox="0 0 24 24"><path d="m9 18 6-6-6-6"/></svg>'
};
function getKey(){return localStorage.getItem('dht_access_key')||''}
function authHeaders(){return {'X-Access-Key':getKey(),'Content-Type':'application/json'}}
async function tryAuth(){
  var key=document.getElementById('authInput').value.trim();if(!key)return;
  var btn=document.getElementById('authSubmit');btn.disabled=true;
  try{
    var r=await fetch('/api/auth',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({key:key})});
    if(r.ok){localStorage.setItem('dht_access_key',key);document.getElementById('authGate').style.display='none';document.getElementById('mainApp').style.display='block';loadData();loadStats();startAutoRefresh();}
    else{document.getElementById('authError').style.display='block';document.getElementById('authInput').value='';document.getElementById('authInput').focus();}
  }catch(e){document.getElementById('authError').style.display='block'}
  btn.disabled=false;
}
document.getElementById('authInput').addEventListener('keydown',function(e){if(e.key==='Enter')tryAuth()});
(function(){
  var s=localStorage.getItem('dht_access_key');
  if(s){fetch('/api/auth',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({key:s})}).then(function(r){if(r.ok){document.getElementById('authGate').style.display='none';document.getElementById('mainApp').style.display='block';loadData();loadStats();startAutoRefresh();}else{localStorage.removeItem('dht_access_key');document.getElementById('authInput').focus();}}).catch(function(){document.getElementById('authInput').focus()});}
  else{document.getElementById('authInput').focus()}
})();
function fmtSize(b){if(!b)return'0 B';if(b<1024)return b+' B';if(b<1048576)return(b/1024).toFixed(1)+' KB';if(b<1073741824)return(b/1048576).toFixed(1)+' MB';return(b/1073741824).toFixed(2)+' GB'}
function fmtTime(ts){var d=new Date(ts*1000);return d.getFullYear()+'-'+String(d.getMonth()+1).padStart(2,'0')+'-'+String(d.getDate()).padStart(2,'0')+' '+String(d.getHours()).padStart(2,'0')+':'+String(d.getMinutes()).padStart(2,'0')+':'+String(d.getSeconds()).padStart(2,'0')}
function esc(s){var d=document.createElement('div');d.textContent=s;return d.innerHTML}
async function loadStats(){
  try{var r=await fetch('/api/stats',{headers:authHeaders()});if(r.status===401){handleUnauthorized();return}var d=await r.json();document.getElementById('stat-storage').textContent=fmtSize(d.db_size);}catch(e){console.error(e)}
}
async function loadData(){
  var url='/api/torrents?page='+curPage+(curSearch?'&search='+encodeURIComponent(curSearch):'');
  try{
    var r=await fetch(url,{headers:authHeaders()});if(r.status===401){handleUnauthorized();return}
    var d=await r.json();
    document.getElementById('stat-total').textContent=d.total.toLocaleString();
    document.getElementById('stat-pages').textContent=d.total_pages;
    totalPages=d.total_pages;
    var tbody=document.getElementById('tbody');
    if(d.rows.length===0){tbody.innerHTML='';document.getElementById('empty').style.display='block';}
    else{
      document.getElementById('empty').style.display='none';
      var start=(curPage-1)*d.page_size;
      tbody.innerHTML=d.rows.map(function(r,i){
        var idx=start+i+1;
        var magnet='magnet:?xt=urn:btih:'+r.infohash;
        if(r.name)magnet+='&dn='+encodeURIComponent(r.name);
        magnet+='&tr='+encodeURIComponent('udp://tracker.opentrackr.org:1337/announce');
        magnet+='&tr='+encodeURIComponent('udp://open.stealth.si:80/announce');
        magnet+='&tr='+encodeURIComponent('udp://tracker.torrent.eu.org:451/announce');
        magnet+='&tr='+encodeURIComponent('udp://exodus.desync.com:6969/announce');
        magnet+='&tr='+encodeURIComponent('udp://tracker.tiny-vps.com:6969/announce');
        return '<tr data-magnet="'+esc(magnet)+'">'+
          '<td class="c1">'+idx+'</td>'+
          '<td class="c2"><a href="javascript:void(0)" onclick="queryRow(\''+esc(r.infohash)+'\')" title="'+esc(r.name)+'">'+esc(r.name)+'</a></td>'+
          '<td class="c3"><span>'+esc(r.infohash.substring(0,12))+'...</span><button onclick="copyText(\''+esc(r.infohash)+'\')" title="复制哈希">'+IC.copy+'</button></td>'+
          '<td class="c4">'+r.file_count+'</td>'+
          '<td class="c5">'+fmtSize(r.total_size)+'</td>'+
          '<td class="c6">'+fmtTime(r.created_at)+'</td>'+
          '<td class="c7"><button onclick="copyMagnet(this)" title="复制磁力链接">'+IC.link+'</button><button class="btn-d" onclick="queryRow(\''+esc(r.infohash)+'\')">详情</button></td>'+
        '</tr>';
      }).join('');
    }
    renderPagination(d.total,d.total_pages);
  }catch(e){console.error(e)}
}
function renderPagination(total,tp){
  var el=document.getElementById('pagination');
  if(tp<=1){el.innerHTML='<span class="pg-info">共 '+total.toLocaleString()+' 条</span>';return}
  var b='';
  b+='<button onclick="goPage(1)"'+(curPage===1?' disabled':'')+'>首页</button>';
  b+='<button onclick="goPage('+(curPage-1)+')"'+(curPage===1?' disabled':'')+'>上一页</button>';
  var s=Math.max(1,curPage-3),e=Math.min(tp,curPage+3);
  for(var i=s;i<=e;i++)b+='<button class="'+(i===curPage?'pg-active':'')+'" onclick="goPage('+i+')">'+i+'</button>';
  b+='<button onclick="goPage('+(curPage+1)+')"'+(curPage===tp?' disabled':'')+'>下一页</button>';
  b+='<button onclick="goPage('+tp+')"'+(curPage===tp?' disabled':'')+'>末页</button>';
  b+='<span class="pg-info">共 '+total.toLocaleString()+' 条</span>';
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
    var r=await fetch('/api/query',{method:'POST',headers:authHeaders(),body:JSON.stringify({query:infohash})});
    if(r.status===401){handleUnauthorized();return}
    var d=await r.json();
    if(d.status==='found'){document.getElementById('queryStatus').innerHTML='';renderResult(d);return;}
    var attempts=0;
    pollTimer=setInterval(async function(){
      attempts++;
      if(attempts>60){clearInterval(pollTimer);pollTimer=null;document.getElementById('queryStatus').innerHTML='<div class="modal-status">查询超时，元数据可能尚未就绪</div>';return;}
      try{var pr=await fetch('/api/query/'+infohash,{headers:authHeaders()});if(pr.status===401){handleUnauthorized();clearInterval(pollTimer);pollTimer=null;return}var pd=await pr.json();if(pd.status==='found'){clearInterval(pollTimer);pollTimer=null;document.getElementById('queryStatus').innerHTML='';renderResult(pd);}}catch(e){}
    },2000);
  }catch(e){document.getElementById('queryStatus').innerHTML='<div class="error-msg">网络错误</div>';}
}
function renderResult(d){
  var totalSize=d.files.reduce(function(a,f){return a+(f.length||0)},0);
  var magnet='magnet:?xt=urn:btih:'+d.infohash;
  if(d.name)magnet+='&dn='+encodeURIComponent(d.name);
  magnet+='&tr='+encodeURIComponent('udp://tracker.opentrackr.org:1337/announce');
  magnet+='&tr='+encodeURIComponent('udp://open.stealth.si:80/announce');
  magnet+='&tr='+encodeURIComponent('udp://tracker.torrent.eu.org:451/announce');
  magnet+='&tr='+encodeURIComponent('udp://exodus.desync.com:6969/announce');
  magnet+='&tr='+encodeURIComponent('udp://tracker.tiny-vps.com:6969/announce');
  window._magnetLink=magnet;
  var h='<table class="rtable">';
  h+='<tr><th>名称</th><td class="rname">'+esc(d.name)+'</td></tr>';
  h+='<tr><th>哈希值</th><td><span class="rhash">'+esc(d.infohash)+'</span><button class="copy-btn" onclick="copyText(\''+esc(d.infohash)+'\')">复制</button></td></tr>';
  h+='<tr><th>磁力链接</th><td style="word-break:break-all"><code style="font-size:10px;color:#888">'+esc(magnet)+'</code><button class="copy-btn" onclick="copyText(window._magnetLink)">复制链接</button></td></tr>';
  h+='<tr><th>文件数</th><td>'+d.files.length+' 个文件</td></tr>';
  h+='<tr><th>总大小</th><td style="font-weight:600">'+fmtSize(totalSize)+'</td></tr>';
  h+='<tr><th>发现时间</th><td style="color:#999">'+fmtTime(d.created_at)+'</td></tr>';
  h+='</table>';
  if(d.files.length>0){
    var sorted=d.files.slice().sort(function(a,b){return (b.length||0)-(a.length||0)});
    h+='<div class="flist">';
    h+='<div class="flist-h" onclick="toggleFiles()"><span>文件列表 ('+d.files.length+' 个文件, '+fmtSize(totalSize)+')</span><span id="fileArrow">'+IC.down+'</span></div>';
    h+='<div class="flist-b" id="fileListBody">';
    for(var i=0;i<sorted.length;i++){var f=sorted[i];var p=f.path?f.path.join('/'):'';h+='<div class="fitem"><span class="fp">'+esc(p)+'</span><span class="fs">'+fmtSize(f.length||0)+'</span></div>';}
    h+='</div></div>';
  }
  document.getElementById('queryResult').innerHTML=h;
}
function toggleFiles(){var b=document.getElementById('fileListBody'),a=document.getElementById('fileArrow');if(b.style.display==='none'){b.style.display='block';a.innerHTML=IC.down}else{b.style.display='none';a.innerHTML=IC.right}}
function copyMagnet(el){var tr=el.closest('tr');if(tr)copyText(tr.getAttribute('data-magnet'))}
function copyText(t){try{if(navigator.clipboard&&window.isSecureContext){navigator.clipboard.writeText(t).then(function(){showToast('已复制到剪贴板')}).catch(function(){fallbackCopy(t)})}else{fallbackCopy(t)}}catch(e){fallbackCopy(t)}}
function fallbackCopy(t){var a=document.createElement('textarea');a.value=t;a.style.cssText='position:fixed;left:-9999px';document.body.appendChild(a);a.select();try{document.execCommand('copy');showToast('已复制到剪贴板')}catch(e){showToast('复制失败')}document.body.removeChild(a)}
function showToast(msg){
  var t=document.createElement('div');t.textContent=msg;t.className='toast';
  document.body.appendChild(t);requestAnimationFrame(function(){t.classList.add('show')});
  setTimeout(function(){t.classList.remove('show');setTimeout(function(){t.remove()},250)},1800);
}
</script>
</body>
</html>"##;
