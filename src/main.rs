use dht_spider::{Config, Dht, Mode};
use dht_spider::wire::WireRunner;
use dht_spider::storage::Storage;
use dht_spider::logger::HourlyLogger;
use dht_spider::web;
use serde_json::json;
use std::sync::Arc;

fn parse_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if (args[i] == "-m" || args[i] == "--key") && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

#[tokio::main]
async fn main() {
    let secret_key = match parse_args() {
        Some(k) => k,
        None => {
            eprintln!("用法: dht-spider -m <密钥>");
            eprintln!("示例: cargo run -- -m my_secret_key");
            std::process::exit(1);
        }
    };

    let logger = Arc::new(HourlyLogger::new("logs"));

    let mut cfg = Config::default();
    cfg.mode = Mode::Crawl;
    cfg.address = "0.0.0.0:6881".into();

    let mut d = match Dht::new(cfg.clone()).await {
        Ok(d) => d,
        Err(e) => {
            let line = json!({"level":"error","event":"startup","error": e.to_string()});
            println!("{}", line.to_string());
            return;
        }
    };

    d.callbacks.on_get_peers = None;

    let storage = Arc::new(Storage::open("torrents.db").expect("failed to open torrents.db"));

    let (runner, handle) = WireRunner::new(65536, 4096, 256);
    let wire_for_announce = handle.clone_handle();
    let wire_for_getpeers = handle.clone_handle();
    {
        let mut sub = handle.subscribe();
        let storage_meta = storage.clone();
        let logger_meta = logger.clone();
        tokio::spawn(async move { runner.run().await; });
        // metadata 订阅
        tokio::spawn(async move {
            while let Ok(resp) = sub.recv().await {
                let infohash_hex = hex::encode(resp.request.info_hash);
                match dht_spider::bencode::decode(&resp.metadata_info) {
                    Ok(dht_spider::bencode::BVal::Dict(m)) => {
                        let name = m.get("name").and_then(|v| match v { dht_spider::bencode::BVal::Bytes(b) => std::str::from_utf8(b).ok().map(|s| s.to_string()), _ => None });
                        if name.is_none() { continue; }
                        let name = name.unwrap();

                        let mut out_files = Vec::new();
                        if let Some(dht_spider::bencode::BVal::List(files)) = m.get("files") {
                            for item in files {
                                if let dht_spider::bencode::BVal::Dict(fm) = item {
                                    let length = fm.get("length").and_then(|v| match v { dht_spider::bencode::BVal::Int(n) => Some(*n as i64), _ => None });
                                    let paths = fm.get("path").and_then(|v| match v { dht_spider::bencode::BVal::List(ps) => {
                                        let mut vec = Vec::new();
                                        for p in ps { if let dht_spider::bencode::BVal::Bytes(pb) = p { if let Ok(s)=std::str::from_utf8(pb) { vec.push(s.to_string()); } } }
                                        Some(vec)
                                    }, _ => None });
                                    if let (Some(length), Some(paths)) = (length, paths) {
                                        out_files.push(json!({"path": paths, "length": length}));
                                    }
                                }
                            }
                        } else if let Some(dht_spider::bencode::BVal::Int(len)) = m.get("length") {
                            out_files.push(json!({"path": [name.clone()], "length": *len as i64}));
                        }

                        if !out_files.is_empty() {
                            let line = json!({
                                "type": "metadata",
                                "infohash": infohash_hex,
                                "name": name,
                                "files": out_files
                            });
                            let line_str = line.to_string();
                            println!("{}", line_str);
                            logger_meta.log(&line_str);

                            let files_json = serde_json::to_string(&out_files).unwrap_or_default();
                            let db = storage_meta.clone();
                            let ih = infohash_hex.clone();
                            let n = name.clone();
                            let fj = files_json.clone();
                            tokio::task::spawn_blocking(move || {
                                let _ = db.insert(&ih, &n, &fj);
                            });
                        }
                    }
                    _ => {}
                }
            }
        });

        // Peer Exchange (PeX) 订阅
        let logger_pex = logger.clone();
        let mut psub = handle.subscribe_peers();
        tokio::spawn(async move {
            while let Ok(evt) = psub.recv().await {
                let line = json!({
                    "type": "peer",
                    "ip": evt.ip,
                    "port": evt.port,
                    "info_hash": hex::encode(evt.info_hash)
                });
                let line_str = line.to_string();
                println!("{}", line_str);
                logger_pex.log(&line_str);
            }
        });
    }

    let logger_ann = logger.clone();
    d.callbacks.on_announce_peer = Some(Arc::new(move |ih, ip, port| {
            let line = json!({
                "type": "peer",
                "ip": ip,
                "port": port,
                "info_hash": ih
            });
            let line_str = line.to_string();
            println!("{}", line_str);
            logger_ann.log(&line_str);
            if let Ok(bytes) = hex::decode(&ih) {
                let h = wire_for_announce.clone_handle();
                tokio::spawn(async move {
                    h.request(&bytes, &ip, port).await;
                });
            }
        }));

    let logger_gp = logger.clone();
    d.callbacks.on_get_peers_response = Some(Arc::new(move |ih, peer| {
            let line = json!({
                "type": "peer",
                "ip": peer.ip.to_string(),
                "port": peer.port,
                "info_hash": ih
            });
            let line_str = line.to_string();
            println!("{}", line_str);
            logger_gp.log(&line_str);
            if let Ok(bytes) = hex::decode(&ih) {
                let h = wire_for_getpeers.clone_handle();
                let ip = peer.ip.to_string();
                let port = peer.port;
                tokio::spawn(async move {
                    h.request(&bytes, &ip, port).await;
                });
            }
        }));

    let logger_node = logger.clone();
    d.callbacks.on_node = Some(Arc::new(move |id_hex, ip, port| {
            let line = json!({
                "type": "node",
                "id": id_hex,
                "ip": ip,
                "port": port
            });
            let line_str = line.to_string();
            println!("{}", line_str);
            logger_node.log(&line_str);
        }));

    let dht_handle = d.start();

    println!("{}", json!({"level":"info","event":"access_key","key":secret_key,"hint":"在浏览器中输入此密钥以访问 Web 面板"}).to_string());

    {
        let web_storage = storage.clone();
        tokio::spawn(async move {
            web::start_server(web_storage, dht_handle, secret_key, 3000).await;
        });
    }

    loop { tokio::time::sleep(std::time::Duration::from_secs(60)).await; }
}
