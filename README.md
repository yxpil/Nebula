<div align="center">

<img src="assets/icon.png" alt="Nebula 图标" width="150">

# Nebula

本地优先的个人记忆检索引擎 · 单文件加密存储 · SQL 接口

[![CI](https://github.com/yxpil/Nebula/actions/workflows/ci.yml/badge.svg)](https://github.com/yxpil/Nebula/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yxpil/Nebula?include_prereleases&label=release)](https://github.com/yxpil/Nebula/releases)
[![License](https://img.shields.io/github/license/yxpil/Nebula)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)](https://www.rust-lang.org/)
[![Tests](https://img.shields.io/badge/tests-146%20passing-success)](https://github.com/yxpil/Nebula/actions/workflows/ci.yml)

</div>

---

Nebula 是一个**完全本地运行**的个人记忆 / 笔记检索引擎:所有数据保存在加密的单文件数据库中,通过类 SQL 语言完成写入、BM25 全文检索、联想查询与多库管理;也可启动 TCP 服务,供远程客户端或你自己的程序以加密协议访问。

> **当前版本为 ES(Engineering Sample,工程测试版)**,核心功能与测试已完整,但数据格式与接口可能继续演进,暂不建议作为唯一数据副本存放不可重建的资料。

## ✨ 特性

- **单文件加密存储**:整个数据库是一个 `.ndb` 文件,Argon2id 密钥派生 + ChaCha20-Poly1305 页加密
- **SQL 接口**:INSERT / SELECT / UPDATE / DELETE,支持 `WHERE` 按 id / 关键词 / 标签 / 重要度过滤
- **BM25 全文检索**:中文分词 + 关键词提取,返回相关度分数;停用词表可自定义
- **联想查询(RELATED)**:基于共现图的跳数扩展,发现语义邻近记忆
- **多库分库**:单文件内多个逻辑库;目录集群(`--dir`)把多个 `.ndb` 挂接进统一命名空间,跨库聚合检索
- **用户与授权**:应用层用户,按库授予 `READ / WRITE / ADMIN`
- **事务**:`BEGIN / COMMIT / ROLLBACK`,内存 undo log,未提交不落盘;断连/退出自动回滚
- **诊断日志**:ERROR/WARN/INFO/DEBUG 分级写文件,按大小自动滚动
- **查询缓存**:重复检索自动复用,写操作后自动失效

## 📦 安装

### 方式一:下载预编译版本(推荐)

**Windows 安装版**:从 [Releases](https://github.com/yxpil/Nebula/releases) 下载 `nebula-setup-*.exe` 运行即可——可自定义安装目录与开始菜单文件夹、创建卸载项(写入"添加/删除程序"),安装在用户目录,**无需管理员权限**;卸载时完整清理程序文件,你的 `.ndb` 数据不受影响。

需要免安装/U盘携带时使用 zip 便携版;macOS、Linux 使用 tar.gz:

| 平台 | 资产文件名包含 |
| --- | --- |
| Windows x64 | `x86_64-pc-windows-msvc.zip` |
| Linux x64 | `x86_64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `aarch64-apple-darwin.tar.gz` |

```bash
# Linux / macOS
tar -xzf nebula-*.tar.gz
cd nebula-*/
./nebula --help

# 可选:校验完整性
sha256sum -c nebula-*.tar.gz.sha256

# Windows (PowerShell)
Expand-Archive .\nebula-*.zip
.\nebula-*\nebula.exe --help
```

把可执行文件放入 `PATH`(Linux/macOS:`sudo mv nebula /usr/local/bin/`)即可全局使用。Windows 版 `nebula.exe` 已嵌入应用图标。

### 方式二:用 Cargo 安装

需要 [Rust 稳定版](https://www.rust-lang.org/tools/install)(Windows 还需 [Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)):

```bash
cargo install --git https://github.com/yxpil/Nebula --bin nebula
```

### 方式三:从源码构建

```bash
git clone https://github.com/yxpil/Nebula.git
cd Nebula
cargo build --release
# 产物: target/release/nebula (Windows 为 nebula.exe)
```

## 🚀 快速开始

```bash
# 1. 创建一个新数据库(交互式设置密码),创建后直接进入本地 SQL 会话
nebula create --db mynotes.ndb

# 2. 以后打开它(本地模式)
nebula open --db mynotes.ndb

# 同一目录的"多库集群"模式:
nebula create --dir ./myspace      # 目录内可 CREATE DATABASE 多个库
nebula open   --dir ./myspace
```

进入会话后:

```sql
INSERT INTO memories (content, tags, importance)
VALUES ('Rust 的所有权规则:一个值同一时刻只有一个所有者', 'rust, lang', 0.9);

SELECT id, content FROM memories WHERE keyword = 'rust';

SEARCH 'rust 所有权' LIMIT 5;

UPDATE memories SET importance = 0.3 WHERE id = 1;
DELETE FROM memories WHERE id = 1;

exit;
```

### 远程服务模式

```bash
# 服务端(默认监听 127.0.0.1:7878,可用 --addr 更改)
nebula serve --db mynotes.ndb --addr 0.0.0.0:7878

# 客户端
nebula connect --addr 127.0.0.1:7878
```

非交互式脚本可使用环境变量跳过密码输入:`NEBULA_PASSWORD`、`NEBULA_USER`。

## 📖 SQL 用法速查

### 写入与查询

```sql
-- 全列顺序: content, keywords, source, importance
INSERT INTO memories VALUES ('内容文本', '词1, 词2', '来源标记', 0.8);
-- 推荐显式列(其余字段自动提取/默认)
INSERT INTO memories (content, tags) VALUES ('内容', '标签1, 标签2');

SELECT * FROM memories;
SELECT id, content, importance FROM memories WHERE id = 3;

UPDATE memories SET importance = 0.9, source = 'book' WHERE keyword = 'rust';
DELETE FROM memories WHERE tag = '临时';
```

记录字段:`id`、`content`、`keywords`(自动提取)、`key_points`(自动提取)、`tags`、`source`、`importance`(0~1)、`created_at`、`updated_at`。

### 检索与联想

```sql
SEARCH '查询语句' LIMIT 10;                 -- BM25,返回 id/score/content/...
SEARCH 'rust 内存' IN main, work LIMIT 5;    -- 显式跨库聚合
RELATED TO 3 LIMIT 5;                        -- 与 id=3 联想(当前库)
RELATED TO work.12 IN main, work LIMIT 5;    -- 限定种子库 + 跨库
```

### 多库管理(目录集群)

```sql
CREATE DATABASE kb;
USE kb;
ATTACH FILE 'notes.ndb' AS notes;   -- 把外部文件挂为逻辑库
DETACH notes;
SHOW DATABASES;
```

### 用户与授权

```sql
CREATE USER bob IDENTIFIED BY '强密码';
GRANT READ, WRITE ON kb TO bob;
GRANT ADMIN ON * TO alice;
REVOKE WRITE ON kb FROM bob;
SHOW GRANTS FOR bob;
DROP USER bob;
```

### 事务

```sql
BEGIN;                        -- 或 START TRANSACTION / BEGIN WORK
INSERT INTO memories (content) VALUES ('草稿');
UPDATE memories SET importance = 0.1 WHERE id = 3;
ROLLBACK;                     -- 全部撤销:事务内的 INSERT/UPDATE/DELETE 恢复原状

BEGIN;
DELETE FROM memories WHERE id = 8;
COMMIT;                       -- 一次 checkpoint 落盘
```

事务语义说明:

- 事务期间写入只改内存,**不提交目录、不写快照**;进程崩溃重开即为事务前状态;
- DDL(`CREATE/DROP DATABASE`、`ATTACH/DETACH`、用户与授权语句)会**隐式提交**当前事务(类 MySQL,因其物理操作不可撤销);
- 事务中执行 `CHECKPOINT` 会被拒绝;REPL 退出或客户端断连时,未提交事务自动回滚;
- **隔离级别:ES 阶段为读未提交(Read Uncommitted)**——同一服务上的其他连接可瞬时观察到未提交数据,但 `ROLLBACK` 后其视图同步恢复;`COMMIT` 保证原子落盘。快照隔离是后续版本目标。

### 缓存与状态

```sql
SHOW CACHE; CLEAR CACHE;
SHOW HOT LIMIT 5; SET CACHE doc 256;
SHOW STATUS; CHECKPOINT;
```

## 🔌 服务请求格式(面向开发者)

如果你要自己写客户端对接 Nebula 服务,通信分为**握手认证**与**加密帧**两阶段(参考实现:[client.rs](crates/nebula-server/src/client.rs))。

### 握手(明文,防重放)

```text
Client → Server  hello:    字节串 b"NEBULA2"(7 字节魔数)
Server → Client  ready:    0x02(1 字节)
Client → Server  identity: varint 长度 || UTF-8 用户名
Server → Client  challenge: 用户盐 salt(16) || 随机挑战 challenge(32)
Client → Server  proof:    HMAC-SHA256(session_key, challenge)(32 字节)
Server → Client  status:   0x01 通过 / 0x00 失败(随后断开)
```

密钥派生(均在客户端本地完成,密码不上网):

```text
master      = Argon2id(password, salt)
session_key = HKDF-SHA256(master, info = "nebula/session/v2" || challenge)
proof       = HMAC-SHA256(session_key, challenge)
```

### 加密帧

```text
[u32 BE 帧长度] || ChaCha20-Poly1305(nonce || 密文 || tag)
AAD = "nebula/frame/up"(或 down) || u64 BE 序号
```

收发序号各自递增,重放、重排或反向注入的帧都无法通过认证。单帧上限 16 MiB。

### JSON 请求 / 响应

帧载荷为 UTF-8 JSON:

```jsonc
// 请求 (type: sql | ping | close)
{ "type": "sql", "sql": "SELECT id FROM memories" }
{ "type": "ping" }
{ "type": "close" }

// 响应 —— 结果集
{
  "ok": true,
  "columns": ["id", "content"],
  "rows": [["1", "示例内容"]],
  "affected": 0
}

// 响应 —— 写操作
{ "ok": true, "message": "OK, inserted memory main.1", "affected": 1 }

// 响应 —— SQL 逻辑错误(连接不断开,可继续发请求)
{ "ok": false, "error": "unknown database 'xx'" }
```

空结果集返回 `ok: true` 且 `message` 为 empty 提示(不带 `rows`)。多语句脚本时,额外的 `script` 字段携带每条语句的响应。

## ⚙️ 配置

首次 `create` 时会在工作目录生成 `nebula.conf.d/`:

- `nebula.toml` —— 全部参数(页大小、检查点阈值、密码策略、BM25 k1/b、联想跳数与衰减、缓存容量、服务默认地址等),分段中文注释,手工编辑后下次启动生效;
- `stopwords.txt` —— 停用词表;
- `nebula.log` —— 诊断日志(由 `[logging]` 段控制开关、级别、文件与大小上限,默认 10MB 滚动)。

日志文件限定为配置目录内的普通文件名,防止路径穿越。

## 🧪 测试

```bash
cargo test --workspace                 # 全部单元 + 集成 + E2E
cargo test -p nebula-server --test e2e # 仅 E2E(真实 TCP + 加密客户端)
```

- **CI 矩阵**:每次提交在 Windows / Linux / macOS (Intel & Apple Silicon) 四个目标上构建并运行全量测试;
- **测试报告**:每个 CI 任务产出 HTML 报告页面(在 workflow 运行页的 Artifacts 中下载);
- E2E 覆盖:完整 CRUD + 检索、事务回滚/提交/断连清理、用户授权与越权拒绝、DDL 隐式提交、错误密码与坏 SQL 不拖垮服务、并发连接一致性。

## 🗂 项目结构

```text
crates/
├── nebula-core        核心类型、错误、编解码、诊断日志
├── nebula-crypto      Argon2id / HKDF / HMAC / ChaCha20-Poly1305
├── nebula-storage     分页文件、目录、快照与加密页
├── nebula-tokenizer   中英文分词、关键词/关键点提取、停用词
├── nebula-sql         SQL 词法/语法/AST
├── nebula-engine      执行器、检索、索引、缓存、授权会话
├── nebula-config      nebula.toml 配置模型与校验
├── nebula-cluster     多文件目录集群
├── nebula-server      TCP 服务端、协议、加密客户端
└── nebula-cli         nebula 命令(本地/服务/远程)
assets/                应用图标(含生成脚本)
scripts/               测试报告生成器
```

## 🛣 Roadmap

- [ ] 快照隔离 / MVCC,消除读未提交
- [ ] 增量快照与 WAL
- [ ] 更丰富的 SQL(ORDER BY / LIMIT on SELECT / 聚合)
- [ ] 语言绑定与 Web 客户端

## License

[MIT](LICENSE) © 2026 yxpil
