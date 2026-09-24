//! TCP 服务端:监听、v2 两步挑战应答认证、thread per connection、命令分发。
//!
//! 两种后端(由启动方式决定):
//! - 单文件模式:打开一个 .ndb,只接受内置 admin,文件密码认证;
//! - 目录模式(--dir):打开集群目录,用户与授权来自 _admin.ndb。
//!
//! 每条连接持有独立的 [`nebula_engine::Session`](跨语句保持 USE 的当前库),
//! 数据访问在共享后端的 Mutex 上串行。

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nebula_core::Result;
use nebula_crypto::{
    ct_eq, derive_master_key, random_bytes, random_salt, SALT_LEN,
};
use nebula_cluster::Cluster;
use nebula_engine::{
    executor, Database, EngineConfig, Session, SessionBackend,
};
use nebula_storage::page::{HEADER_PREFIX_LEN, MAGIC, SALT_OFFSET};
use nebula_tokenizer::ExtractorConfig;

use crate::protocol::{
    auth_proof, read_frame, read_identity, session_key, write_frame,
    CHALLENGE_FRAME_LEN, CHALLENGE_LEN, Direction, PROTOCOL_MAGIC, READY_BYTE, Request, Response,
    STATUS_FAIL, STATUS_OK,
};

/// 服务端句柄。
pub struct Server {
    addr: SocketAddr,
    backend: ConnBackend,
}

/// 连接可用的后端(可廉价 Clone:内部均为 Arc)。
#[derive(Clone)]
pub enum ConnBackend {
    /// 单文件:数据库 + 文件头盐 + 主密钥。
    Single {
        db: Arc<Mutex<Database>>,
        salt: [u8; SALT_LEN],
        master: [u8; 32],
    },
    /// 目录集群。
    Cluster {
        cluster: Arc<Mutex<Cluster>>,
    },
}

impl Server {
    /// 打开已有单文件数据库并准备服务。
    pub fn open(addr: SocketAddr, path: &Path, password: &str) -> Result<Self> {
        Self::open_configured(
            addr,
            path,
            password,
            &EngineConfig::default(),
            &ExtractorConfig::default(),
            &nebula_tokenizer::default_stopword_set(),
        )
    }

    /// 打开已有单文件数据库,注入引擎/提取配置与停用词。
    pub fn open_configured(
        addr: SocketAddr,
        path: &Path,
        password: &str,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let db = Database::open_configured(path, password, cfg, extractor_cfg, stopwords)?;
        let salt = load_salt(path)?;
        let master = derive_master_key(password, &salt);
        Ok(Server {
            addr,
            backend: ConnBackend::Single {
                db: Arc::new(Mutex::new(db)),
                salt,
                master,
            },
        })
    }

    /// 创建新单文件数据库并准备服务(文件必须不存在)。
    pub fn create(addr: SocketAddr, path: &Path, password: &str, page_size: u32) -> Result<Self> {
        Self::create_configured(
            addr,
            path,
            password,
            page_size,
            &EngineConfig::default(),
            &ExtractorConfig::default(),
            &nebula_tokenizer::default_stopword_set(),
        )
    }

    /// 创建新单文件数据库,注入引擎/提取配置与停用词。
    pub fn create_configured(
        addr: SocketAddr,
        path: &Path,
        password: &str,
        page_size: u32,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let db =
            Database::create_configured(path, password, page_size, cfg, extractor_cfg, stopwords)?;
        let salt = load_salt(path)?;
        let master = derive_master_key(password, &salt);
        Ok(Server {
            addr,
            backend: ConnBackend::Single {
                db: Arc::new(Mutex::new(db)),
                salt,
                master,
            },
        })
    }

    /// 打开或初始化目录集群并准备服务。
    pub fn open_directory(addr: SocketAddr, dir: &Path, password: &str) -> Result<Self> {
        let cluster = Cluster::open_or_create(dir, password)?;
        Ok(Server {
            addr,
            backend: ConnBackend::Cluster {
                cluster: Arc::new(Mutex::new(cluster)),
            },
        })
    }

    /// 以目录集群方式打开,注入引擎/分词配置(等价 open_directory 但走配置)。
    pub fn open_directory_configured(
        addr: SocketAddr,
        dir: &Path,
        password: &str,
        engine_cfg: &nebula_engine::EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &std::collections::HashSet<String>,
    ) -> Result<Self> {
        let cluster = Cluster::configured(dir, password, engine_cfg, extractor_cfg, stopwords)?;
        Ok(Server {
            addr,
            backend: ConnBackend::Cluster {
                cluster: Arc::new(Mutex::new(cluster)),
            },
        })
    }

    /// 绑定地址并进入 accept 循环(阻塞,直到进程终止)。
    pub fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(self.addr)?;
        let label = match &self.backend {
            ConnBackend::Single { db, .. } => db
                .lock()
                .map(|d| d.path().display().to_string())
                .unwrap_or_else(|_| "<locked>".into()),
            ConnBackend::Cluster { cluster } => cluster
                .lock()
                .map(|c| format!("{}", c.dir_display().display()))
                .unwrap_or_else(|_| "<locked>".into()),
        };
        println!(
            "nebula-server listening on {} ({})",
            listener.local_addr()?,
            label
        );
        serve_until(listener, self.backend.clone(), Arc::new(AtomicBool::new(false)))
    }

    /// 后端标识(展示用)。
    pub fn backend_label(&self) -> String {
        match &self.backend {
            ConnBackend::Single { db, .. } => db
                .lock()
                .map(|d| d.path().display().to_string())
                .unwrap_or_else(|_| "<locked>".into()),
            ConnBackend::Cluster { cluster } => cluster
                .lock()
                .map(|c| format!("{}", c.dir_display().display()))
                .unwrap_or_else(|_| "<locked>".into()),
        }
    }
}

/// 读取数据库文件头中的盐(明文前缀)。
pub fn load_salt(path: &Path) -> Result<[u8; SALT_LEN]> {
    let mut file = File::open(path)?;
    let mut head = vec![0u8; HEADER_PREFIX_LEN];
    file.read_exact(&mut head)?;
    if &head[0..MAGIC.len()] != MAGIC {
        return Err(nebula_core::Error::Storage(
            "not a nebula database file".into(),
        ));
    }
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&head[SALT_OFFSET..SALT_OFFSET + SALT_LEN]);
    Ok(salt)
}

/// 从数据库文件头读取盐并重新派生主密钥。
pub fn load_master_key(path: &Path, password: &str) -> Result<[u8; 32]> {
    let salt = load_salt(path)?;
    Ok(derive_master_key(password, &salt))
}

/// 带停止开关的服务循环(测试用:置位 `stop` 后 Accept 循环在 ~50ms 内退出)。
pub fn serve_until(
    listener: TcpListener,
    backend: ConnBackend,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // Windows 上 accept 出的流会继承监听口的 nonblocking 标志,必须复位。
                if let Err(e) = stream.set_nonblocking(false) {
                    eprintln!("failed to reset stream blocking mode: {e}");
                    continue;
                }
                let backend = backend.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, backend) {
                        if !matches!(&e, nebula_core::Error::Protocol(m) if m == "connection closed")
                        {
                            eprintln!("connection error: {e}");
                        }
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// 单连接生命周期:握手 → 帧循环 → 断开。
fn handle_conn(mut stream: TcpStream, backend: ConnBackend) -> Result<()> {
    // 1. 读客户端 hello(魔数),回 ready
    let mut hello = [0u8; PROTOCOL_MAGIC.len()];
    stream
        .read_exact(&mut hello)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                nebula_core::Error::Protocol("connection closed".into())
            }
            _ => e.into(),
        })?;
    if &hello != PROTOCOL_MAGIC {
        return Err(nebula_core::Error::Protocol(
            "client is not a nebula v2 client (bad magic)".into(),
        ));
    }
    stream.write_all(&[READY_BYTE])?;
    stream.flush()?;

    // 2. 读用户名
    let user = read_identity(&mut stream)?;

    // 3. 解析 (盐, 验证器):未知用户用一次性随机盐/验证器,proof 必然失配,
    //    但仍走完挑战流程(避免按"是否发挑战"枚举用户)。
    let mut known = true;
    let (salt, verifier) = match &backend {
        ConnBackend::Single { salt, master, .. } => {
            if user == nebula_engine::auth::DEFAULT_USER {
                (*salt, *master)
            } else {
                known = false;
                (random_salt(), random_bytes())
            }
        }
        ConnBackend::Cluster { cluster } => {
            let guard = cluster
                .lock()
                .map_err(|_| nebula_core::Error::Engine("cluster mutex poisoned".into()))?;
            match guard.verifier_of(&user) {
                Some((s, v)) => {
                    let mut salt = [0u8; SALT_LEN];
                    if s.len() != SALT_LEN || v.len() != 32 {
                        return Err(nebula_core::Error::Codec(
                            "corrupt stored verifier".into(),
                        ));
                    }
                    salt.copy_from_slice(&s);
                    let mut vv = [0u8; 32];
                    vv.copy_from_slice(&v);
                    (salt, vv)
                }
                None => {
                    known = false;
                    (random_salt(), random_bytes())
                }
            }
        }
    };

    // 4. 发 challenge:salt || 新鲜挑战
    let challenge: [u8; CHALLENGE_LEN] = random_bytes();
    let mut frame = Vec::with_capacity(CHALLENGE_FRAME_LEN);
    frame.extend_from_slice(&salt);
    frame.extend_from_slice(&challenge);
    stream.write_all(&frame)?;
    stream.flush()?;

    // 5. 校验 proof
    let mut proof = [0u8; 32];
    stream.read_exact(&mut proof)?;
    let expected = auth_proof(&session_key(&verifier, &challenge), &challenge);
    if !known || !ct_eq(&expected, &proof) {
        stream.write_all(&[STATUS_FAIL])?;
        stream.flush()?;
        return Err(nebula_core::Error::Auth("authentication failed".into()));
    }
    stream.write_all(&[STATUS_OK])?;
    stream.flush()?;

    // 6. 加密帧循环:连接级 SQL 会话跨请求保持(USE 生效)。
    let frame_key = session_key(&verifier, &challenge);
    let mut sql_session = Session::new(user.clone(), nebula_core::DEFAULT_DB);
    let mut req_seq = 0u64;
    let mut resp_seq = 0u64;
    loop {
        let payload = match read_frame(&mut stream, &frame_key, Direction::Up, req_seq) {
            Ok(p) => p,
            Err(nebula_core::Error::Protocol(m)) if m == "connection closed" => return Ok(()),
            Err(e) => return Err(e),
        };
        req_seq += 1;
        let req: Request = serde_json::from_slice(&payload)
            .map_err(|e| nebula_core::Error::Protocol(format!("bad request json: {e}")))?;
        let is_close = matches!(req, Request::Close);
        let resp = dispatch_request(&backend, &req, &mut sql_session)?;
        let out = serde_json::to_vec(&resp)
            .map_err(|e| nebula_core::Error::Protocol(format!("serialize response: {e}")))?;
        write_frame(&mut stream, &frame_key, Direction::Down, resp_seq, &out)?;
        resp_seq += 1;
        if is_close {
            return Ok(());
        }
    }
}

/// 请求分发:SQL 在后端 Mutex 上串行执行,使用连接持有的会话。
fn dispatch_request(
    backend: &ConnBackend,
    req: &Request,
    sql_session: &mut Session,
) -> Result<Response> {
    match req {
        Request::Ping => Ok(Response::text("pong", 0)),
        Request::Close => Ok(Response::text("bye", 0)),
        Request::Sql { sql } => {
            // SQL 解析/执行错误转为 ok=false 响应,连接保持可用;
            // 只有传输/协议类错误才用 Err 中断。
            let outcome = match backend {
                ConnBackend::Single { db, .. } => {
                    let mut guard = db
                        .lock()
                        .map_err(|_| nebula_core::Error::Engine("db mutex poisoned".into()))?;
                    run_sql(&mut *guard, sql_session, sql)
                }
                ConnBackend::Cluster { cluster } => {
                    let mut guard = cluster
                        .lock()
                        .map_err(|_| nebula_core::Error::Engine("cluster mutex poisoned".into()))?;
                    run_sql(&mut *guard, sql_session, sql)
                }
            };
            match outcome {
                Ok(resp) => Ok(resp),
                Err(e) => Ok(Response::error(e.to_string())),
            }
        }
    }
}

/// 在会话后端上执行脚本,渲染为线上响应。
fn run_sql(
    host: &mut dyn SessionBackend,
    sql_session: &mut Session,
    sql: &str,
) -> Result<Response> {
    let stmts = nebula_sql::parse_script(sql)?;
    if stmts.is_empty() {
        return Ok(Response::error("empty script"));
    }
    let mut results = Vec::with_capacity(stmts.len());
    for stmt in &stmts {
        results.push(executor::dispatch(host, sql_session, stmt)?);
    }
    if results.len() == 1 {
        return Ok(to_response(&results[0]));
    }
    // 多语句:script 携带全部结果,主字段保留最后一个
    let script: Vec<Response> = results.iter().map(to_response).collect();
    let last = to_response(results.last().unwrap());
    Ok(Response {
        script: Some(script),
        ..last
    })
}

/// QueryResult → 线上 Response。
fn to_response(r: &nebula_engine::QueryResult) -> Response {
    Response {
        ok: true,
        columns: if r.columns.is_empty() {
            None
        } else {
            Some(r.columns.clone())
        },
        rows: if r.rows.is_empty() {
            None
        } else {
            Some(r.rows.clone())
        },
        message: if r.message.is_empty() {
            None
        } else {
            Some(r.message.clone())
        },
        affected: r.affected,
        error: None,
        script: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use nebula_engine::MemBackend;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nebula_srv_{}_{}.ndb", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nebula_srvdir_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    const PW: &str = "server-test-password";

    /// 单文件 server 线程。
    fn spawn_server(name: &str) -> (String, std::path::PathBuf, Arc<AtomicBool>) {
        let path = tmp(name);
        {
            let mut db = Database::create(&path, PW, 4096).unwrap();
            db.close().unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let salt = load_salt(&path).unwrap();
        let master = derive_master_key(PW, &salt);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let db = Database::open(&path, PW).unwrap();
        let backend = ConnBackend::Single {
            db: Arc::new(Mutex::new(db)),
            salt,
            master,
        };
        thread::spawn(move || {
            let _ = serve_until(listener, backend, stop2);
        });
        (addr, path.clone(), stop)
    }

    /// 集群 server 线程。
    fn spawn_cluster(name: &str) -> (String, std::path::PathBuf, Arc<AtomicBool>) {
        let dir = tmp_dir(name);
        Cluster::open_or_create(&dir, PW).unwrap().checkpoint().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let cluster = Cluster::open_or_create(&dir, PW).unwrap();
        let backend = ConnBackend::Cluster {
            cluster: Arc::new(Mutex::new(cluster)),
        };
        thread::spawn(move || {
            let _ = serve_until(listener, backend, stop2);
        });
        (addr, dir.clone(), stop)
    }

    fn shutdown_file(stop: Arc<AtomicBool>, path: &std::path::Path) {
        stop.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(120));
        let _ = std::fs::remove_file(path);
    }

    fn shutdown_dir(stop: Arc<AtomicBool>, dir: &std::path::Path) {
        stop.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(120));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn end_to_end_sql_over_tcp() {
        let (addr, path, stop) = spawn_server("e2e");
        let mut c = Client::connect(&addr, PW).unwrap();

        let r = c
            .sql("INSERT INTO memories (content, tags, importance) VALUES ('Rust 的所有权与借用检查保证内存安全', 'rust, memory', 0.9)")
            .unwrap();
        assert!(r.ok, "{r:?}");

        let r = c
            .sql("SELECT id, content, keywords FROM memories WHERE tag = 'memory' ORDER BY id")
            .unwrap();
        assert!(r.ok, "{r:?}");
        let rows = r.rows.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0][1].contains("借用检查"));

        // 错误 SQL → ok=false 且带 error 字段,连接仍可用
        let r = c.sql("SELECT bogus FROM memories").unwrap();
        assert!(!r.ok);
        assert!(r.error.is_some());

        let r = c.sql("SHOW STATUS").unwrap();
        assert!(r.ok);
        let r = c.sql("CHECKPOINT").unwrap();
        assert!(r.ok);
        assert_eq!(r.message.as_deref(), Some("checkpoint done"));

        assert!(c.close().unwrap().ok);
        drop(c);
        shutdown_file(stop, &path);
    }

    #[test]
    fn wrong_password_rejected() {
        let (addr, path, stop) = spawn_server("wrongpw");
        let err = Client::connect(&addr, "definitely-not-the-password").unwrap_err();
        assert!(matches!(err, nebula_core::Error::Auth(_)), "{err}");
        shutdown_file(stop, &path);
    }

    #[test]
    fn unknown_user_rejected() {
        let (addr, path, stop) = spawn_server("ghostuser");
        let err = Client::connect_as(&addr, "ghost", PW).unwrap_err();
        assert!(matches!(err, nebula_core::Error::Auth(_)), "{err}");
        shutdown_file(stop, &path);
    }

    #[test]
    fn sequential_clients_see_all_records() {
        let (addr, path, stop) = spawn_server("seq");
        {
            let mut c = Client::connect(&addr, PW).unwrap();
            c.sql("INSERT INTO memories (content) VALUES ('第一条记忆:Rust 生命周期')")
                .unwrap();
            c.sql("INSERT INTO memories (content) VALUES ('第二条记忆:页式存储')")
                .unwrap();
            assert!(c.ping().unwrap().ok);
            c.close().unwrap();
        }
        let mut c = Client::connect(&addr, PW).unwrap();
        let r = c.sql("SELECT id, content FROM memories ORDER BY id").unwrap();
        assert_eq!(r.rows.unwrap().len(), 2);
        c.close().unwrap();
        drop(c);
        shutdown_file(stop, &path);
    }

    #[test]
    fn multi_user_cluster_permissions() {
        let (addr, dir, stop) = spawn_cluster("multi");
        // admin 建库 + 建用户 + 授权
        let mut admin = Client::connect_as(&addr, "admin", PW).unwrap();
        admin.sql("CREATE DATABASE work").unwrap();
        admin.sql("CREATE USER bob IDENTIFIED BY 'bobpw'").unwrap();
        admin.sql("GRANT READ ON work TO bob").unwrap();
        admin.close().unwrap();
        drop(admin);

        // bob 连接:work 可读,main 不可写
        let mut bob = Client::connect_as(&addr, "bob", "bobpw").unwrap();
        let r = bob.sql("SEARCH 'anything' IN work LIMIT 3").unwrap();
        assert!(r.ok, "bob 对 work 有读权限");
        let r = bob.sql("INSERT INTO memories (content) VALUES ('x')").unwrap();
        assert!(!r.ok, "bob 对 main 无写权限");
        bob.close().unwrap();
        drop(bob);
        shutdown_dir(stop, &dir);
    }

    #[test]
    fn use_persists_across_requests_on_connection() {
        let (addr, path, stop) = spawn_server("usesess");
        let mut c = Client::connect(&addr, PW).unwrap();
        c.sql("CREATE DATABASE work").unwrap();
        c.sql("USE work").unwrap();
        // 不带 IN 的 SEARCH 应作用于上一条 USE 选定的 work(连接会话保持)
        let r = c.sql("INSERT INTO memories (content) VALUES ('Rust 工作记录')").unwrap();
        assert!(r.ok, "{r:?}");
        let r = c.sql("SELECT id FROM memories").unwrap();
        assert_eq!(r.rows.unwrap().len(), 1, "当前库仍是 work");
        c.close().unwrap();
        drop(c);
        shutdown_file(stop, &path);
    }

    #[test]
    fn load_helpers_work() {
        let path = tmp("helpers");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        db.close().unwrap();
        let salt = load_salt(&path).unwrap();
        let master = load_master_key(&path, PW).unwrap();
        assert_eq!(master, derive_master_key(PW, &salt));
        assert_ne!(master, load_master_key(&path, "wrong").unwrap());
        let bad = tmp("helpers_bad");
        std::fs::write(&bad, b"not a nebula db at all").unwrap();
        assert!(load_salt(&bad).is_err());
        let _ = std::fs::remove_file(&bad);
        let _ = std::fs::remove_file(&path);
    }
}
