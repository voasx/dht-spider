use crate::dht::DhtHandle;
use crate::storage::Storage;
use axum::{
    extract::{Path, Query, State},
    http::header,
    response::IntoResponse,
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

pub async fn start_server(storage: Arc<Storage>, dht: DhtHandle, port: u16) {
    let state = AppState { storage, dht };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/torrents", get(api_torrents))
        .route("/api/query/:infohash", get(api_query_status))
        .route("/api/query", axum::routing::post(api_query_start))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    println!("{}", json!({"level":"info","event":"web_server","listen":format!("http://{}", addr)}).to_string());
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let _ = axum::serve(listener, app).await;
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

async fn api_torrents(State(state): State<AppState>, Query(q): Query<ListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
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

async fn api_query_start(State(state): State<AppState>, axum::Json(body): axum::Json<QueryBody>) -> impl IntoResponse {
    let input = body.query.trim().to_string();
    let infohash = parse_infohash(&input);
    match infohash {
        Some(ih) => {
            // check if already in db
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
            // trigger DHT query
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
    // magnet:?xt=urn:btih:HEX...
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
    // plain 40-char hex
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
<title>DHT Spider - 种子爬虫</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;background:#0f172a;color:#e2e8f0;min-height:100vh}
.header{background:linear-gradient(135deg,#1e293b 0%,#0f172a 100%);border-bottom:1px solid #1e293b;padding:20px 24px;display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:12px}
.header h1{font-size:20px;font-weight:600;color:#38bdf8;letter-spacing:-0.5px}
.header .stats{display:flex;gap:16px;font-size:13px;color:#94a3b8}
.header .stats span{background:#1e293b;padding:4px 12px;border-radius:6px}
.search-box{background:#1e293b;border:1px solid #334155;border-radius:8px;padding:8px 16px;color:#e2e8f0;font-size:14px;width:240px;outline:none;transition:border-color .2s}
.search-box:focus{border-color:#38bdf8}
.toolbar{padding:12px 24px;display:flex;justify-content:space-between;align-items:center;background:#1e293b;border-bottom:1px solid #334155;gap:12px}
.toolbar-left{display:flex;gap:12px;align-items:center}
.refresh-btn{background:#38bdf8;color:#0f172a;border:none;padding:6px 16px;border-radius:6px;cursor:pointer;font-size:13px;font-weight:500;transition:background .2s}
.refresh-btn:hover{background:#7dd3fc}
.query-btn{background:#a78bfa;color:#0f172a;border:none;padding:6px 16px;border-radius:6px;cursor:pointer;font-size:13px;font-weight:500;transition:background .2s}
.query-btn:hover{background:#c4b5fd}
.auto-refresh{display:flex;align-items:center;gap:6px;font-size:13px;color:#94a3b8}
.auto-refresh input{accent-color:#38bdf8}
table{width:100%;border-collapse:collapse}
thead th{background:#1e293b;padding:10px 12px;text-align:left;font-size:12px;font-weight:600;color:#94a3b8;text-transform:uppercase;letter-spacing:.5px;border-bottom:1px solid #334155;position:sticky;top:0}
tbody tr{border-bottom:1px solid #1e293b;transition:background .15s}
tbody tr:hover{background:#1e293b}
td{padding:10px 12px;font-size:13px;vertical-align:top;max-width:400px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
td.name{white-space:normal;word-break:break-all}
td.hash{font-family:"SF Mono",Menlo,monospace;font-size:11px;color:#38bdf8}
td.size{color:#a78bfa}
td.time{color:#64748b;font-size:12px}
.pagination{display:flex;justify-content:center;align-items:center;gap:8px;padding:16px;background:#1e293b;border-top:1px solid #334155}
.pagination button{background:#334155;color:#e2e8f0;border:none;padding:6px 14px;border-radius:6px;cursor:pointer;font-size:13px;transition:background .2s}
.pagination button:hover:not(:disabled){background:#475569}
.pagination button:disabled{opacity:.3;cursor:default}
.pagination .current{background:#38bdf8;color:#0f172a;font-weight:600}
.pagination .info{color:#94a3b8;font-size:13px}
.empty{text-align:center;padding:60px 20px;color:#64748b;font-size:15px}
.container{max-width:1400px;margin:0 auto}

/* modal */
.modal-overlay{display:none;position:fixed;inset:0;background:rgba(0,0,0,.6);z-index:100;justify-content:center;align-items:center}
.modal-overlay.active{display:flex}
.modal{background:#1e293b;border:1px solid #334155;border-radius:12px;width:680px;max-width:95vw;max-height:85vh;display:flex;flex-direction:column}
.modal-header{display:flex;justify-content:space-between;align-items:center;padding:16px 20px;border-bottom:1px solid #334155}
.modal-header h2{font-size:16px;font-weight:600;color:#e2e8f0}
.modal-close{background:none;border:none;color:#94a3b8;font-size:20px;cursor:pointer;padding:4px 8px;border-radius:4px}
.modal-close:hover{color:#e2e8f0;background:#334155}
.modal-body{padding:20px;overflow-y:auto;flex:1}
.modal-input-row{display:flex;gap:8px}
.modal-input{flex:1;background:#0f172a;border:1px solid #334155;border-radius:8px;padding:10px 14px;color:#e2e8f0;font-size:14px;outline:none;transition:border-color .2s}
.modal-input:focus{border-color:#a78bfa}
.modal-submit{background:#a78bfa;color:#0f172a;border:none;padding:10px 20px;border-radius:8px;cursor:pointer;font-size:14px;font-weight:500;white-space:nowrap;transition:background .2s}
.modal-submit:hover{background:#c4b5fd}
.modal-submit:disabled{opacity:.5;cursor:default}
.modal-status{margin-top:16px;text-align:center;color:#94a3b8;font-size:14px}
.modal-status .spinner{display:inline-block;width:18px;height:18px;border:2px solid #334155;border-top-color:#38bdf8;border-radius:50%;animation:spin .8s linear infinite;vertical-align:middle;margin-right:6px}
@keyframes spin{to{transform:rotate(360deg)}}
.result-table{width:100%;border-collapse:collapse;margin-top:16px}
.result-table th{text-align:left;padding:8px 12px;background:#0f172a;color:#94a3b8;font-size:12px;font-weight:600;border-bottom:1px solid #334155;white-space:nowrap}
.result-table td{padding:8px 12px;border-bottom:1px solid #1e293b;font-size:13px;vertical-align:top}
.result-table td code{font-family:"SF Mono",Menlo,monospace;font-size:12px}
.result-name{color:#e2e8f0;font-size:15px;font-weight:500;word-break:break-all}
.result-hash{font-family:"SF Mono",Menlo,monospace;font-size:12px;color:#38bdf8;cursor:pointer}
.result-hash:hover{text-decoration:underline}
.file-list{margin-top:16px}
.file-list-header{background:#0f172a;padding:10px 12px;border-radius:8px 8px 0 0;font-size:13px;font-weight:600;color:#94a3b8;display:flex;justify-content:space-between;cursor:pointer;user-select:none}
.file-list-header:hover{color:#e2e8f0}
.file-list-body{border:1px solid #334155;border-top:none;border-radius:0 0 8px 8px;max-height:300px;overflow-y:auto}
.file-item{display:flex;justify-content:space-between;padding:6px 12px;border-bottom:1px solid #1e293b;font-size:12px}
.file-item:last-child{border-bottom:none}
.file-item .path{color:#cbd5e1;word-break:break-all}
.file-item .size{color:#64748b;white-space:nowrap;margin-left:12px}
.copy-btn{background:none;border:1px solid #334155;color:#94a3b8;padding:2px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-left:6px;transition:all .2s}
.copy-btn:hover{color:#e2e8f0;border-color:#94a3b8}
.error-msg{color:#f87171;margin-top:16px;text-align:center;font-size:14px}
</style>
</head>
<body>
<div class="header">
  <h1>DHT Spider - 种子爬虫</h1>
  <div class="stats">
    <span id="stat-total">总计: 0</span>
    <span id="stat-pages">页数: 0</span>
  </div>
  <input class="search-box" id="search" placeholder="搜索种子名称..." />
</div>
<div class="toolbar">
  <div class="toolbar-left">
    <div class="auto-refresh">
      <input type="checkbox" id="autoRefresh" checked />
      <label for="autoRefresh">自动刷新 (30秒)</label>
    </div>
  </div>
  <div style="display:flex;gap:8px">
    <button class="query-btn" onclick="openModal()">查询</button>
    <button class="refresh-btn" onclick="loadData()">刷新</button>
  </div>
</div>
<div class="container">
  <table>
    <thead><tr><th>#</th><th>名称</th><th>哈希值</th><th>文件数</th><th>大小</th><th>发现时间</th></tr></thead>
    <tbody id="tbody"></tbody>
  </table>
  <div id="empty" class="empty" style="display:none">暂无数据，等待爬取中...</div>
</div>
<div class="pagination" id="pagination"></div>

<!-- 查询弹窗 -->
<div class="modal-overlay" id="modalOverlay">
  <div class="modal">
    <div class="modal-header">
      <h2>查询种子信息</h2>
      <button class="modal-close" onclick="closeModal()">&times;</button>
    </div>
    <div class="modal-body">
      <div class="modal-input-row">
        <input class="modal-input" id="queryInput" placeholder="输入磁力链接或 InfoHash (40位十六进制)" />
        <button class="modal-submit" id="querySubmit" onclick="startQuery()">查询</button>
      </div>
      <div id="queryStatus"></div>
      <div id="queryResult"></div>
    </div>
  </div>
</div>

<script>
let curPage=1,curSearch='',totalPages=0,refreshTimer=null,pollTimer=null;
function fmtSize(b){if(b<1024)return b+'B';if(b<1048576)return(b/1024).toFixed(1)+'KB';if(b<1073741824)return(b/1048576).toFixed(1)+'MB';return(b/1073741824).toFixed(2)+'GB'}
function fmtTime(ts){let d=new Date(ts*1000);return d.getFullYear()+'-'+String(d.getMonth()+1).padStart(2,'0')+'-'+String(d.getDate()).padStart(2,'0')+' '+String(d.getHours()).padStart(2,'0')+':'+String(d.getMinutes()).padStart(2,'0')+':'+String(d.getSeconds()).padStart(2,'0')}
function esc(s){let d=document.createElement('div');d.textContent=s;return d.innerHTML}
async function loadData(){
  let url='/api/torrents?page='+curPage+(curSearch?'&search='+encodeURIComponent(curSearch):'');
  try{
    let r=await fetch(url);let d=await r.json();
    document.getElementById('stat-total').textContent='总计: '+d.total;
    document.getElementById('stat-pages').textContent='页数: '+d.total_pages;
    totalPages=d.total_pages;
    let tbody=document.getElementById('tbody');
    if(d.rows.length===0){tbody.innerHTML='';document.getElementById('empty').style.display='block';}
    else{
      document.getElementById('empty').style.display='none';
      let start=(curPage-1)*d.page_size;
      tbody.innerHTML=d.rows.map((r,i)=>'<tr>'+
        '<td>'+(start+i+1)+'</td>'+
        '<td class="name">'+esc(r.name)+'</td>'+
        '<td class="hash">'+esc(r.infohash)+'</td>'+
        '<td>'+r.file_count+'</td>'+
        '<td class="size">'+fmtSize(r.total_size)+'</td>'+
        '<td class="time">'+fmtTime(r.created_at)+'</td>'+
      '</tr>').join('');
    }
    renderPagination(d.total,d.total_pages);
  }catch(e){console.error(e)}
}
function renderPagination(total,tp){
  let el=document.getElementById('pagination');
  if(tp<=1){el.innerHTML='<span class="info">总计 '+total+' 条</span>';return}
  let btns='';
  btns+='<button onclick="goPage(1)"'+(curPage===1?' disabled':'')+'>首页</button>';
  btns+='<button onclick="goPage('+(curPage-1)+')"'+(curPage===1?' disabled':'')+'>上一页</button>';
  let s=Math.max(1,curPage-3),e=Math.min(tp,curPage+3);
  for(let i=s;i<=e;i++)btns+='<button class="'+(i===curPage?'current':'')+'" onclick="goPage('+i+')">'+i+'</button>';
  btns+='<button onclick="goPage('+(curPage+1)+')"'+(curPage===tp?' disabled':'')+'>下一页</button>';
  btns+='<button onclick="goPage('+tp+')"'+(curPage===tp?' disabled':'')+'>末页</button>';
  btns+='<span class="info">总计 '+total+' 条</span>';
  el.innerHTML=btns;
}
function goPage(p){curPage=p;loadData();window.scrollTo(0,0)}
function startAutoRefresh(){if(refreshTimer)clearInterval(refreshTimer);if(document.getElementById('autoRefresh').checked)refreshTimer=setInterval(loadData,30000)}
document.getElementById('search').addEventListener('keydown',function(e){if(e.key==='Enter'){curSearch=this.value.trim();curPage=1;loadData()}});
document.getElementById('autoRefresh').addEventListener('change',startAutoRefresh);

// modal
function openModal(){
  document.getElementById('modalOverlay').classList.add('active');
  document.getElementById('queryInput').value='';
  document.getElementById('queryStatus').innerHTML='';
  document.getElementById('queryResult').innerHTML='';
  document.getElementById('queryInput').focus();
}
function closeModal(){
  document.getElementById('modalOverlay').classList.remove('active');
  if(pollTimer){clearInterval(pollTimer);pollTimer=null}
}
document.getElementById('modalOverlay').addEventListener('click',function(e){if(e.target===this)closeModal()});
document.getElementById('queryInput').addEventListener('keydown',function(e){if(e.key==='Enter')startQuery()});

async function startQuery(){
  let input=document.getElementById('queryInput').value.trim();
  if(!input){return}
  let btn=document.getElementById('querySubmit');
  btn.disabled=true;
  document.getElementById('queryStatus').innerHTML='<div class="modal-status"><span class="spinner"></span>正在查询中...</div>';
  document.getElementById('queryResult').innerHTML='';
  if(pollTimer){clearInterval(pollTimer);pollTimer=null}
  try{
    let r=await fetch('/api/query',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({query:input})});
    let d=await r.json();
    if(d.status==='error'){
      document.getElementById('queryStatus').innerHTML='<div class="error-msg">'+esc(d.message)+'</div>';
      btn.disabled=false;return;
    }
    if(d.status==='found'){
      document.getElementById('queryStatus').innerHTML='';
      renderResult(d);btn.disabled=false;return;
    }
    // searching -> poll
    let ih=d.infohash;
    let attempts=0;
    pollTimer=setInterval(async function(){
      attempts++;
      if(attempts>60){clearInterval(pollTimer);pollTimer=null;
        document.getElementById('queryStatus').innerHTML='<div class="modal-status">查询超时，元数据可能尚未就绪，请稍后在列表中查看</div>';
        btn.disabled=false;return;
      }
      try{
        let pr=await fetch('/api/query/'+ih);let pd=await pr.json();
        if(pd.status==='found'){
          clearInterval(pollTimer);pollTimer=null;
          document.getElementById('queryStatus').innerHTML='';
          renderResult(pd);btn.disabled=false;
        }
      }catch(e){}
    },2000);
  }catch(e){
    document.getElementById('queryStatus').innerHTML='<div class="error-msg">网络错误</div>';
    btn.disabled=false;
  }
}

function renderResult(d){
  let totalSize=d.files.reduce((a,f)=>a+(f.length||0),0);
  let html='<table class="result-table">';
  html+='<tr><th>名称</th><td class="result-name">'+esc(d.name)+'</td></tr>';
  html+='<tr><th>哈希值</th><td><span class="result-hash" onclick="copyText(\''+esc(d.infohash)+'\')">'+esc(d.infohash)+'</span><button class="copy-btn" onclick="copyText(\''+esc(d.infohash)+'\')">复制</button></td></tr>';
  html+='<tr><th>磁力链接</th><td><button class="copy-btn" onclick="copyText(\'magnet:?xt=urn:btih:'+esc(d.infohash)+'\')">复制磁力链接</button></td></tr>';
  html+='<tr><th>文件数</th><td>'+d.files.length+' 个文件</td></tr>';
  html+='<tr><th>总大小</th><td style="color:#a78bfa">'+fmtSize(totalSize)+'</td></tr>';
  html+='<tr><th>发现时间</th><td style="color:#64748b">'+fmtTime(d.created_at)+'</td></tr>';
  html+='</table>';

  // file list
  if(d.files.length>0){
    let sorted=[...d.files].sort((a,b)=>(b.length||0)-(a.length||0));
    html+='<div class="file-list">';
    html+='<div class="file-list-header" onclick="toggleFiles()"><span>文件列表 ('+d.files.length+' 个文件, '+fmtSize(totalSize)+')</span><span id="fileArrow">&#9660;</span></div>';
    html+='<div class="file-list-body" id="fileListBody">';
    for(let f of sorted){
      let p=f.path?f.path.join('/'):f.name||'';
      html+='<div class="file-item"><span class="path">'+esc(p)+'</span><span class="size">'+fmtSize(f.length||0)+'</span></div>';
    }
    html+='</div></div>';
  }
  document.getElementById('queryResult').innerHTML=html;
}
function toggleFiles(){
  let body=document.getElementById('fileListBody');
  let arrow=document.getElementById('fileArrow');
  if(body.style.display==='none'){body.style.display='block';arrow.innerHTML='&#9660;'}
  else{body.style.display='none';arrow.innerHTML='&#9654;'}
}
function copyText(text){
  navigator.clipboard.writeText(text).then(function(){
    showToast('已复制到剪贴板');
  });
}
function showToast(msg){
  let t=document.createElement('div');
  t.textContent=msg;
  t.style.cssText='position:fixed;bottom:24px;left:50%;transform:translateX(-50%);background:#38bdf8;color:#0f172a;padding:8px 20px;border-radius:8px;font-size:14px;font-weight:500;z-index:200;transition:opacity .3s';
  document.body.appendChild(t);
  setTimeout(function(){t.style.opacity='0';setTimeout(function(){t.remove()},300)},1500);
}

loadData();startAutoRefresh();
</script>
</body>
</html>"##;
