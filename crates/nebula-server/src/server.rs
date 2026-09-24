//! TCP 服务端:监听、挑战应答认证、线程池(thread per connection)、命令分发。
//!
//! 密码只在启动时输入一次:服务端打开数据库并派生主密钥常驻内存,
//! 客户端每次连接用挑战应答证明自己知道密码,无需传递密码本身。
//! 所有连接共享同一个 [`Database`](nebula_engine::Database)(Mutex 串行化)。

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
use nebula_crypto::{ct_eq, derive_master_key, random_bytes, SALT_LEN};
use nebula_engine::{Database, EngineConfig};
use nebula_storage::page::{HEADER_PREFIX_LEN, MAGIC, SALT_OFFSET};
use nebula_tokenizer::ExtractorConfig;

use crate::protocol::{
    auth_proof, read_frame, session_key, write_frame, CHALLENGE_LEN, Direction, HELLO_LEN,
    PROTOCOL_MAGIC, PROOF_LEN, Request, Response, STATUS_FAIL, STATUS_OK,
};

/// 服务端句柄。
pub struct Server {
    addr: SocketAddr,
    salt: [u8; SALT_LEN],
    master_key: [u8; 32],
    db: Arc<Mutex<Database>>,
}

impl Server {
    /// 打开已有数据库并准备服务(密码错误即失败)。
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

    /// 打开已有数据库并准备服务,注入引擎/提取配置与停用词(库旁配置)。
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
        let master_key = derive_master_key(password, &salt);
        Ok(Server {
            addr,
            salt,
            master_key,
            db: Arc::new(Mutex::new(db)),
        })
    }

    /// 创建新数据库并准备服务(文件必须不存在)。
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

    /// 创建新数据库并准备服务,注入引擎/提取配置与停用词(库旁配置)。
    pub fn create_configured(
        addr: SocketAddr,
        path: &Path,
        password: &str,
        page_size: u32,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let db = Database::create_configured(path, password, page_size, cfg, extractor_cfg, stopwords)?;
        let salt = load_salt(path)?;
        let master_key = derive_master_key(password, &salt);
        Ok(Server {
            addr,
            salt,
            master_key,
            db: Arc::new(Mutex::new(db)),
        })
    }

    /// 绑定地址并进入 accept 循环(阻塞,直到进程终止)。
    pub fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(self.addr)?;
        let db_path = self
            .db
            .lock()
            .map(|db| db.path().display().to_string())
            .unwrap_or_else(|_| "<locked>".into());
        println!(
            "nebula-server listening on {} (db: {})",
            listener.local_addr()?,
            db_path
        );
        serve_until(
            listener,
            self.salt,
            self.master_key,
            Arc::clone(&self.db),
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// 数据库路径(展示用)。
    pub fn db_path(&self) -> String {
        self.db
            .lock()
            .map(|db| db.path().display().to_string())
            .unwrap_or_else(|_| "<locked>".into())
    }
}

/// 读取数据库文件头中的盐(明文前缀,受头页 AEAD 之外的整体文件完整性隐含保护)。
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
///
/// [`Database::open`] 已校验密码;这里按同一盐再派生一次,
/// 用于握手时的会话密钥推导(Argon2id 成本固定,启动一次性开销)。
pub fn load_master_key(path: &Path, password: &str) -> Result<[u8; 32]> {
    let salt = load_salt(path)?;
    Ok(derive_master_key(password, &salt))
}

/// 在已绑定的监听器上服务(阻塞,直到进程终止)。
pub fn serve(
    listener: TcpListener,
    salt: [u8; SALT_LEN],
    master_key: [u8; 32],
    db: Arc<Mutex<Database>>,
) -> Result<()> {
    serve_until(
        listener,
        salt,
        master_key,
        db,
        Arc::new(AtomicBool::new(false)),
    )
}

/// 带停止开关的服务循环(测试用:置位 `stop` 后 Accept 循环在 ~50ms 内退出)。
pub fn serve_until(
    listener: TcpListener,
    salt: [u8; SALT_LEN],
    master_key: [u8; 32],
    db: Arc<Mutex<Database>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // Windows 上 accept 出的流会继承监听口的 nonblocking 标志,必须复位,
                // 否则后续 read_exact 直接 WSAEWOULDBLOCK。
                if let Err(e) = stream.set_nonblocking(false) {
                    eprintln!("failed to reset stream blocking mode: {e}");
                    continue;
                }
                let db = Arc::clone(&db);
                thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, salt, master_key, db) {
                        // 客户端主动断开属于正常分支,其余错误打印到 stderr。
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
fn handle_conn(
    mut stream: TcpStream,
    salt: [u8; SALT_LEN],
    master_key: [u8; 32],
    db: Arc<Mutex<Database>>,
) -> Result<()> {
    // 1. 服务端发 hello:魔数 + 盐(公开,客户端据此派生主密钥)+ 新鲜挑战
    let challenge: [u8; CHALLENGE_LEN] = random_bytes();
    let mut hello = Vec::with_capacity(HELLO_LEN);
    hello.extend_from_slice(PROTOCOL_MAGIC);
    hello.extend_from_slice(&salt);
    hello.extend_from_slice(&challenge);
    stream.write_all(&hello)?;
    stream.flush()?;

    // 2. 校验 proof(挑战每次新鲜 → 防重放;错误密码直接断开)
    let mut proof = [0u8; PROOF_LEN];
    stream.read_exact(&mut proof)?;
    let expected = auth_proof(&session_key(&master_key, &challenge), &challenge);
    if !ct_eq(&expected, &proof) {
        stream.write_all(&[STATUS_FAIL])?;
        stream.flush()?;
        return Err(nebula_core::Error::Auth("wrong password".into()));
    }
    stream.write_all(&[STATUS_OK])?;
    stream.flush()?;

    // 3. 加密帧循环:上行(请求)与下行(响应)各自独立编号,方向进 AAD。
    let session = session_key(&master_key, &challenge);
    let mut req_seq = 0u64;
    let mut resp_seq = 0u64;
    loop {
        let payload = match read_frame(&mut stream, &session, Direction::Up, req_seq) {
            Ok(p) => p,
            Err(nebula_core::Error::Protocol(m)) if m == "connection closed" => return Ok(()),
            Err(e) => return Err(e),
        };
        req_seq += 1;
        let req: Request = serde_json::from_slice(&payload)
            .map_err(|e| nebula_core::Error::Protocol(format!("bad request json: {e}")))?;
        let is_close = matches!(req, Request::Close);
        let resp = dispatch(&db, &req)?;
        let out = serde_json::to_vec(&resp)
            .map_err(|e| nebula_core::Error::Protocol(format!("serialize response: {e}")))?;
        write_frame(&mut stream, &session, Direction::Down, resp_seq, &out)?;
        resp_seq += 1;
        if is_close {
            return Ok(());
        }
    }
}

/// 命令分发:所有 SQL 在数据库 Mutex 上串行执行。
fn dispatch(db: &Arc<Mutex<Database>>, req: &Request) -> Result<Response> {
    match req {
        Request::Ping => Ok(Response::text("pong", 0)),
        Request::Close => Ok(Response::text("bye", 0)),
        Request::Sql { sql } => {
            let mut guard = db
                .lock()
                .map_err(|_| nebula_core::Error::Engine("db mutex poisoned".into()))?;
            match guard.execute_script(sql) {
                Ok(results) if results.is_empty() => Ok(Response::error("empty script")),
                Ok(results) if results.len() == 1 => Ok(to_response(&results[0])),
                Ok(results) => {
                    // 多语句:script 携带全部结果,主字段保留最后一个
                    let script: Vec<Response> = results.iter().map(to_response).collect();
                    let last = to_response(results.last().unwrap());
                    Ok(Response {
                        script: Some(script),
                        ..last
                    })
                }
                Err(e) => Ok(Response::error(e.to_string())),
            }
        }
    }
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

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nebula_srv_{}_{}.ndb", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    const PW: &str = "server-test-password";

    /// 起一个 server 线程,返回 (地址, 库路径, 停止开关)。
    /// 测试结束:置位 stop → join → 删临时库文件。
    fn spawn_server(name: &str) -> (String, std::path::PathBuf, Arc<AtomicBool>) {
        let path = tmp(name);
        {
            let mut db = Database::create(&path, PW, 4096).unwrap();
            db.close().unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let db = Database::open(&path, PW).unwrap();
        let salt = load_salt(&path).unwrap();
        let master = derive_master_key(PW, &salt);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        thread::spawn(move || {
            let _ = serve_until(listener, salt, master, Arc::new(Mutex::new(db)), stop2);
        });
        (addr, path.clone(), stop)
    }

    /// 等连接处理线程退出后删除临时库(此时所有 File 句柄已释放)。
    fn shutdown(stop: Arc<AtomicBool>, path: &std::path::Path) {
        stop.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(120));
        let _ = std::fs::remove_file(path);
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

        // 元语句
        let r = c.sql("SHOW STATUS").unwrap();
        assert!(r.ok);
        assert_eq!(
            r.columns.as_ref().unwrap(),
            &vec!["Variable".to_string(), "Value".to_string()]
        );

        let r = c.sql("CHECKPOINT").unwrap();
        assert!(r.ok);
        assert_eq!(r.message.as_deref(), Some("checkpoint done"));

        assert!(c.close().unwrap().ok);
        drop(c);
        shutdown(stop, &path);
    }

    #[test]
    fn wrong_password_rejected() {
        let (addr, path, stop) = spawn_server("wrongpw");
        let err = Client::connect(&addr, "definitely-not-the-password").unwrap_err();
        assert!(matches!(err, nebula_core::Error::Auth(_)), "{err}");
        shutdown(stop, &path);
    }

    #[test]
    fn sequential_clients_see_all_records() {
        let (addr, path, stop) = spawn_server("seq");
        {
            let mut c = Client::connect(&addr, PW).unwrap();
            c.sql("INSERT INTO memories (content) VALUES ('第一条记忆:今天学习了 Rust 生命周期')")
                .unwrap();
            c.sql("INSERT INTO memories (content) VALUES ('第二条记忆:数据库页式存储设计')")
                .unwrap();
            assert!(c.ping().unwrap().ok);
            c.close().unwrap();
        }
        // 新连接查询仍在(内存索引持久化)
        let mut c = Client::connect(&addr, PW).unwrap();
        let r = c.sql("SELECT id, content FROM memories ORDER BY id").unwrap();
        let rows = r.rows.unwrap();
        assert_eq!(rows.len(), 2);
        c.close().unwrap();
        drop(c);
        shutdown(stop, &path);
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
        // 非 nebula 文件
        let bad = tmp("helpers_bad");
        std::fs::write(&bad, b"not a nebula db at all").unwrap();
        assert!(load_salt(&bad).is_err());
        let _ = std::fs::remove_file(&bad);
        let _ = std::fs::remove_file(&path);
    }
}
