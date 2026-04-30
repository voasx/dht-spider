# dht-spider

Rust 实现的 BitTorrent DHT 爬虫，支持元数据下载、种子信息存储与 Web 管理面板。

## 功能特性

- **DHT 爬虫（BEP-5）**：支持 Standard 与 Crawl 两种模式，自动发现 infohash
- **元数据下载（BEP-9/10）**：通过 Wire 协议从 peer 下载 .torrent 元数据
- **对等交换（BEP-11）**：解析 ut_pex 扩展，发现更多 peer
- **SQLite 持久化**：自动存储成功下载的种子信息（名称、文件列表、大小）
- **Web 管理面板**：内置中文界面，支持分页浏览、搜索、按行查询、磁力链接复制
- **密钥认证**：启动时生成随机访问密钥，保护 Web 面板和 API

## 快速开始

```bash
cargo build --release
cargo run
```

启动后控制台输出示例：

```json
{"level":"info","event":"access_key","key":"a1b2c3d4e5f6...","hint":"在浏览器中输入此密钥以访问 Web 面板"}
{"level":"info","event":"web_server","listen":"http://0.0.0.0:3000"}
{"type":"metadata","infohash":"abc123...","name":"Example Torrent","files":[{"path":["file.mkv"],"length":123456789}]}
```

浏览器访问 `http://localhost:3000`，输入控制台显示的密钥即可进入面板。

## Web 面板功能

| 功能 | 说明 |
|------|------|
| 种子列表 | 分页展示所有爬取到的种子，显示名称、哈希、文件数、大小、时间 |
| 搜索 | 按种子名称模糊搜索 |
| 按行查询 | 每行"查询"按钮触发 DHT 搜索，实时轮询结果并弹窗展示 |
| 磁力链接 | 查询结果中生成完整磁力链接（含 dn + 多 tracker），一键复制 |
| 自动刷新 | 30 秒自动刷新列表，可手动关闭 |
| 密钥认证 | 未认证时全屏遮罩，认证后密钥存入 localStorage |

## API 接口

所有 `/api/*` 接口（除 `/api/auth`）需在请求头中携带 `X-Access-Key`。

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/auth` | 验证密钥，body: `{"key":"..."}` |
| GET | `/api/torrents?page=1&page_size=50&search=关键词` | 分页查询种子列表 |
| POST | `/api/query` | 按行查询，body: `{"query":"infohash或磁力链接"}` |
| GET | `/api/query/:infohash` | 轮询查询结果 |

## 配置与默认值

开箱即用，无需额外配置。主要默认参数：

- **监听**：UDP `0.0.0.0:6881`（DHT）、TCP `0.0.0.0:3000`（Web）
- **模式**：Crawl
- **数据库**：`torrents.db`（SQLite WAL 模式）
- **路由**：k=8，最大节点 5000
- **事务**：上限 10000 条，指数退避重试
- **Wire 并发**：256 个 worker，请求队列 TTL 600 秒
- **Peer 表**：上限 50000 个 infohash
- **黑名单**：上限 65536 条目

## 项目结构

```
src/
├── main.rs         # 入口：DHT + Wire + Storage + Web 启动与事件订阅
├── dht.rs          # DHT 节点核心（BEP-5）
├── wire.rs         # Wire 协议元数据下载（BEP-9/10）
├── storage.rs      # SQLite 存储层
├── web.rs          # Axum Web 服务器 + 内嵌前端
├── routing.rs      # K-Bucket 路由表
├── transaction.rs  # 事务管理（超时、重试、容量限制）
├── peers.rs        # Peer 管理器
├── blacklist.rs    # IP 黑名单
├── krpc.rs         # KRPC 消息编码/解码
├── bencode.rs      # Bencode 编解码
├── types.rs        # 公共类型定义
└── util.rs         # 工具函数
```

## 输出格式

运行时仅输出成功下载的元数据（JSONL 格式）：

```json
{"type":"metadata","infohash":"a1b2c3...","name":"Example","files":[{"path":["dir","file.mkv"],"length":123456789}]}
```

## License

MIT
