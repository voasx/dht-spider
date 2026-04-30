use crate::storage::Storage;
use axum::{
    extract::{Query, State},
    http::header,
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[derive(Deserialize)]
struct ListQuery {
    page: Option<u32>,
    page_size: Option<u32>,
    search: Option<String>,
}

pub async fn start_server(storage: Arc<Storage>, port: u16) {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/torrents", get(api_torrents))
        .layer(CorsLayer::permissive())
        .with_state(storage);

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

async fn api_torrents(State(storage): State<Arc<Storage>>, Query(q): Query<ListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let search = q.search.unwrap_or_default();

    let result = tokio::task::spawn_blocking(move || storage.list(page, page_size, &search)).await;
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

static INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>DHT Spider</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;background:#0f172a;color:#e2e8f0;min-height:100vh}
.header{background:linear-gradient(135deg,#1e293b 0%,#0f172a 100%);border-bottom:1px solid #1e293b;padding:20px 24px;display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:12px}
.header h1{font-size:20px;font-weight:600;color:#38bdf8;letter-spacing:-0.5px}
.header .stats{display:flex;gap:16px;font-size:13px;color:#94a3b8}
.header .stats span{background:#1e293b;padding:4px 12px;border-radius:6px}
.search-box{background:#1e293b;border:1px solid #334155;border-radius:8px;padding:8px 16px;color:#e2e8f0;font-size:14px;width:240px;outline:none;transition:border-color .2s}
.search-box:focus{border-color:#38bdf8}
.toolbar{padding:12px 24px;display:flex;justify-content:space-between;align-items:center;background:#1e293b;border-bottom:1px solid #334155}
.refresh-btn{background:#38bdf8;color:#0f172a;border:none;padding:6px 16px;border-radius:6px;cursor:pointer;font-size:13px;font-weight:500;transition:background .2s}
.refresh-btn:hover{background:#7dd3fc}
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
</style>
</head>
<body>
<div class="header">
  <h1>DHT Spider</h1>
  <div class="stats">
    <span id="stat-total">Total: 0</span>
    <span id="stat-pages">Pages: 0</span>
  </div>
  <input class="search-box" id="search" placeholder="Search torrent name..." />
</div>
<div class="toolbar">
  <div class="auto-refresh">
    <input type="checkbox" id="autoRefresh" checked />
    <label for="autoRefresh">Auto refresh (30s)</label>
  </div>
  <button class="refresh-btn" onclick="loadData()">Refresh</button>
</div>
<div class="container">
  <table>
    <thead><tr><th>#</th><th>Name</th><th>InfoHash</th><th>Files</th><th>Size</th><th>Time</th></tr></thead>
    <tbody id="tbody"></tbody>
  </table>
  <div id="empty" class="empty" style="display:none">No data yet, waiting for crawl...</div>
</div>
<div class="pagination" id="pagination"></div>
<script>
let curPage=1,curSearch='',totalPages=0,refreshTimer=null;
function fmtSize(b){if(b<1024)return b+'B';if(b<1048576)return(b/1024).toFixed(1)+'KB';if(b<1073741824)return(b/1048576).toFixed(1)+'MB';return(b/1073741824).toFixed(2)+'GB'}
function fmtTime(ts){let d=new Date(ts*1000);return d.getFullYear()+'-'+String(d.getMonth()+1).padStart(2,'0')+'-'+String(d.getDate()).padStart(2,'0')+' '+String(d.getHours()).padStart(2,'0')+':'+String(d.getMinutes()).padStart(2,'0')+':'+String(d.getSeconds()).padStart(2,'0')}
function esc(s){let d=document.createElement('div');d.textContent=s;return d.innerHTML}
async function loadData(){
  let url='/api/torrents?page='+curPage+(curSearch?'&search='+encodeURIComponent(curSearch):'');
  try{
    let r=await fetch(url);let d=await r.json();
    document.getElementById('stat-total').textContent='Total: '+d.total;
    document.getElementById('stat-pages').textContent='Pages: '+d.total_pages;
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
  if(tp<=1){el.innerHTML='<span class="info">Total: '+total+'</span>';return}
  let btns='';
  btns+='<button onclick="goPage(1)"'+(curPage===1?' disabled':'')+'>First</button>';
  btns+='<button onclick="goPage('+(curPage-1)+')"'+(curPage===1?' disabled':'')+'>Prev</button>';
  let s=Math.max(1,curPage-3),e=Math.min(tp,curPage+3);
  for(let i=s;i<=e;i++)btns+='<button class="'+(i===curPage?'current':'')+'" onclick="goPage('+i+')">'+i+'</button>';
  btns+='<button onclick="goPage('+(curPage+1)+')"'+(curPage===tp?' disabled':'')+'>Next</button>';
  btns+='<button onclick="goPage('+tp+')"'+(curPage===tp?' disabled':'')+'>Last</button>';
  btns+='<span class="info">Total: '+total+'</span>';
  el.innerHTML=btns;
}
function goPage(p){curPage=p;loadData();window.scrollTo(0,0)}
function startAutoRefresh(){if(refreshTimer)clearInterval(refreshTimer);if(document.getElementById('autoRefresh').checked)refreshTimer=setInterval(loadData,30000)}
document.getElementById('search').addEventListener('keydown',function(e){if(e.key==='Enter'){curSearch=this.value.trim();curPage=1;loadData()}});
document.getElementById('autoRefresh').addEventListener('change',startAutoRefresh);
loadData();startAutoRefresh();
</script>
</body>
</html>"##;
