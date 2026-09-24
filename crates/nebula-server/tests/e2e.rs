//! 端到端(E2E)测试:真实 TCP 监听 + 真实加密 Client。
//!
//! 覆盖:
//! - 单文件服务:INSERT/SELECT/SEARCH/UPDATE/DELETE/SHOW STATUS 全链路
//! - 事务:BEGIN/ROLLBACK/COMMIT、无事务报错、断连自动回滚、跨连接可见性
//! - 目录集群:建用户、按库 READ/WRITE 授权、越权拒绝、DDL 隐式提交
//! - 错误密码/SQL 错误不拖垮服务;并发连接已提交读一致
//!
//! 这些用例在 `cargo test --workspace` 中与单元测试一起运行。

use std::collections::HashSet;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nebula_cluster::Cluster;
use nebula_crypto::derive_master_key;
use nebula_engine::Database;
use nebula_server::server::{serve_until, ConnBackend};
use nebula_server::Client;

const ADMIN_PW: &str = "E2E-admin-pass-2026";
const BOB_PW: &str = "B0b-secure-pass-2026";

fn unique_dir(label: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nebula_e2e_{label}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_dir_all(p);
}

/// 已启动的测试服务。drop 前调 `shutdown()`。
struct ServerGuard {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
    root: std::path::PathBuf,
}

impl ServerGuard {
    fn addr(&self) -> &str {
        &self.addr
    }

    fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
        cleanup(&self.root);
    }
}

fn spawn(backend: ConnBackend, root: std::path::PathBuf) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let _ = serve_until(listener, backend, stop2);
    });
    ServerGuard {
        addr,
        stop,
        handle,
        root,
    }
}

/// 单文件后端服务(只接受 admin)。
fn spawn_single(label: &str) -> ServerGuard {
    let dir = unique_dir(label);
    let path = dir.join("single.ndb");
    Database::create(&path, ADMIN_PW, 4096).unwrap();

    let db = Database::open(&path, ADMIN_PW).unwrap();
    let salt = nebula_server::server::load_salt(&path).unwrap();
    let master = derive_master_key(ADMIN_PW, &salt);
    let backend = ConnBackend::Single {
        db: Arc::new(Mutex::new(db)),
        salt,
        master,
    };
    spawn(backend, dir)
}

/// 目录集群后端服务。
fn spawn_cluster(label: &str) -> ServerGuard {
    let dir = unique_dir(label);
    let cluster = Cluster::open_or_create(&dir, ADMIN_PW).unwrap();
    let backend = ConnBackend::Cluster {
        cluster: Arc::new(Mutex::new(cluster)),
    };
    spawn(backend, dir)
}

/// 取结果行;服务端对空结果集返回 `(empty result)` 提示(rows=None),
/// 此时按 0 行处理。
fn rows_of(r: &nebula_server::Response) -> Vec<Vec<String>> {
    r.rows.clone().unwrap_or_default()
}

/// 执行 SQL 并断言服务端返回成功;失败时打印错误信息。
fn sql_ok(c: &mut Client, sql: &str) -> nebula_server::Response {
    let r = c.sql(sql).unwrap();
    assert!(r.ok, "expected success for `{sql}`, got error: {:?}", r.error);
    r
}

/// 执行 SQL 并断言被服务端拒绝(ok=false,连接保持)。
fn sql_rejected(c: &mut Client, sql: &str) {
    let r = c.sql(sql).unwrap();
    assert!(!r.ok, "expected rejection for `{sql}`");
    assert!(r.error.is_some(), "rejected response must carry an error");
}

// ---------------- 测试 ----------------

#[test]
fn e2e_single_server_full_crud_search_lifecycle() {
    let g = spawn_single("crud");
    let mut c = Client::connect(g.addr(), ADMIN_PW).unwrap();

    assert!(c.ping().unwrap().ok);

    // 插入三条记忆(显式列 / 全列 / 默认值三种写法)
    let r = sql_ok(
        &mut c,
        "INSERT INTO memories (content, tags, importance) VALUES ('Rust 借用检查器防止悬垂引用', 'rust, lang', 0.9)",
    );
    assert_eq!(r.affected, 1);
    sql_ok(
        &mut c,
        "INSERT INTO memories VALUES ('BM25 用逆文档频率衡量词的区分度', 'search, bm25', 't', 0.8)",
    );
    sql_ok(&mut c, "INSERT INTO memories (content) VALUES ('一条普通备注')");

    // 全量查询
    assert_eq!(rows_of(&sql_ok(&mut c, "SELECT id FROM memories")).len(), 3);
    // 关键词过滤
    let r = sql_ok(&mut c, "SELECT content FROM memories WHERE keyword = 'rust'");
    assert_eq!(rows_of(&r).len(), 1);
    // 标签过滤(首条记忆 tags 为 rust, lang)
    assert_eq!(
        rows_of(&sql_ok(&mut c, "SELECT id FROM memories WHERE tag = 'lang'")).len(),
        1
    );

    // BM25 检索:返回非空且含 score 列
    let r = sql_ok(&mut c, "SEARCH 'rust 借用' LIMIT 5");
    assert!(r.columns.as_ref().unwrap().iter().any(|x| x == "score"));
    assert!(!rows_of(&r).is_empty());

    // UPDATE 生效
    sql_ok(&mut c, "UPDATE memories SET importance = 0.3 WHERE id = 1");
    let r = sql_ok(&mut c, "SELECT importance FROM memories WHERE id = 1");
    assert_eq!(rows_of(&r)[0][0], "0.30");

    // DELETE 生效
    sql_ok(&mut c, "DELETE FROM memories WHERE id = 2");
    assert_eq!(rows_of(&sql_ok(&mut c, "SELECT id FROM memories")).len(), 2);

    // SHOW STATUS 含事务状态行
    let r = sql_ok(&mut c, "SHOW STATUS");
    assert!(r.rows.unwrap().iter().any(|row| row[0] == "transaction"));

    assert!(c.close().unwrap().ok);
    g.shutdown();
}

#[test]
fn e2e_transaction_rollback_commit_and_disconnect_cleanup() {
    let g = spawn_single("txn");
    let mut c = Client::connect(g.addr(), ADMIN_PW).unwrap();

    // 无活动事务:控制语句报错且不影响连接
    sql_rejected(&mut c, "COMMIT");
    sql_rejected(&mut c, "ROLLBACK");

    // ROLLBACK 路径:事务内可见,回滚后消失
    sql_ok(&mut c, "BEGIN");
    sql_ok(&mut c, "INSERT INTO memories (content) VALUES ('临时草稿')");
    assert_eq!(rows_of(&sql_ok(&mut c, "SELECT id FROM memories")).len(), 1);
    sql_ok(&mut c, "ROLLBACK");
    assert_eq!(rows_of(&sql_ok(&mut c, "SELECT id FROM memories")).len(), 0);

    // COMMIT 路径
    sql_ok(&mut c, "BEGIN");
    sql_ok(&mut c, "INSERT INTO memories (content) VALUES ('正式记录')");
    sql_ok(&mut c, "COMMIT");
    assert_eq!(rows_of(&sql_ok(&mut c, "SELECT id FROM memories")).len(), 1);

    // 另一条新连接:已提交数据跨连接可见
    let mut c2 = Client::connect(g.addr(), ADMIN_PW).unwrap();
    assert_eq!(rows_of(&sql_ok(&mut c2, "SELECT id FROM memories")).len(), 1);

    // 嵌套 BEGIN 被拒绝
    sql_ok(&mut c, "BEGIN");
    sql_rejected(&mut c, "BEGIN");
    // 事务中 CHECKPOINT 被拒绝
    sql_rejected(&mut c, "CHECKPOINT");

    // 断连清理:直接丢弃带活动事务的客户端(不发 Close),
    // 服务端应在断连时自动回滚。
    sql_ok(&mut c, "INSERT INTO memories (content) VALUES ('断连应回滚')");
    drop(c);
    thread::sleep(Duration::from_millis(200));

    let mut c3 = Client::connect(g.addr(), ADMIN_PW).unwrap();
    assert_eq!(
        rows_of(&sql_ok(&mut c3, "SELECT id FROM memories")).len(),
        1,
        "断连前未提交的修改必须已回滚"
    );
    g.shutdown();
}

#[test]
fn e2e_cluster_users_privileges_and_ddl_implicit_commit() {
    let g = spawn_cluster("auth");

    // 错误密码:认证阶段失败,但服务继续运行
    assert!(Client::connect_as(g.addr(), "admin", "wrong-password").is_err());

    let mut admin = Client::connect_as(g.addr(), "admin", ADMIN_PW).unwrap();
    sql_ok(
        &mut admin,
        &format!("CREATE USER bob IDENTIFIED BY '{BOB_PW}'"),
    );

    let mut bob = Client::connect_as(g.addr(), "bob", BOB_PW).unwrap();
    // 无任何授权:读/写都拒绝
    sql_rejected(&mut bob, "SELECT id FROM memories");
    sql_rejected(
        &mut bob,
        "INSERT INTO memories (content) VALUES ('越权写入')",
    );

    // 授予 READ:可读(空表);仍不可写
    sql_ok(&mut admin, "GRANT READ ON main TO bob");
    assert_eq!(rows_of(&sql_ok(&mut bob, "SELECT id FROM memories")).len(), 0);
    sql_rejected(
        &mut bob,
        "INSERT INTO memories (content) VALUES ('仍越权')",
    );

    // 追加 WRITE:写入成功
    sql_ok(&mut admin, "GRANT WRITE ON main TO bob");
    sql_ok(
        &mut bob,
        "INSERT INTO memories (content) VALUES ('bob 的合法记录')",
    );

    // 撤销 WRITE 后再次拒绝
    sql_ok(&mut admin, "REVOKE WRITE ON main FROM bob");
    sql_rejected(
        &mut bob,
        "INSERT INTO memories (content) VALUES ('撤权后写入')",
    );

    // DDL 隐式提交:admin 开事务写入后执行 CREATE DATABASE,
    // 事务被自动提交,ROLLBACK 不再可撤销。
    sql_ok(&mut admin, "BEGIN");
    sql_ok(
        &mut admin,
        "INSERT INTO memories (content) VALUES ('DDL 前的事务写入')",
    );
    sql_ok(&mut admin, "CREATE DATABASE work");
    sql_rejected(&mut admin, "ROLLBACK"); // 已无活动事务
    // 两条写入(bob + admin)均在
    assert_eq!(
        rows_of(&sql_ok(&mut admin, "SELECT id FROM memories")).len(),
        2
    );

    // 错误 SQL 返回 ok=false,但连接与服务仍然健康
    sql_rejected(&mut admin, "THIS IS NOT VALID SQL");
    assert!(admin.ping().unwrap().ok);

    // SHOW GRANTS 可查
    let r = sql_ok(&mut admin, "SHOW GRANTS FOR bob");
    assert!(r.rows.unwrap().iter().any(|row| row.join(" ").contains("main")));

    g.shutdown();
}

#[test]
fn e2e_concurrent_connections_share_committed_state() {
    let g = spawn_cluster("conc");
    let mut a = Client::connect(g.addr(), ADMIN_PW).unwrap();
    let mut b = Client::connect(g.addr(), ADMIN_PW).unwrap();

    assert_eq!(rows_of(&sql_ok(&mut b, "SELECT id FROM memories")).len(), 0);
    sql_ok(
        &mut a,
        "INSERT INTO memories (content) VALUES ('A 连接提交的数据')",
    );
    // 普通 INSERT 自动落盘:B 立即可见
    assert_eq!(rows_of(&sql_ok(&mut b, "SELECT id FROM memories")).len(), 1);

    // A 事务未提交:ES 阶段隔离级别为"读未提交"(共享内存索引),
    // B 可瞬时观察到未提交数据;但 ROLLBACK 后 B 视角也必须恢复。
    sql_ok(&mut a, "BEGIN");
    sql_ok(
        &mut a,
        "INSERT INTO memories (content) VALUES ('事务中数据')",
    );
    assert_eq!(rows_of(&sql_ok(&mut b, "SELECT id FROM memories")).len(), 2);
    sql_ok(&mut a, "ROLLBACK");
    assert_eq!(
        rows_of(&sql_ok(&mut b, "SELECT id FROM memories")).len(),
        1,
        "其他连接观察到的未提交数据必须随 ROLLBACK 消失"
    );

    // 提交后:B 视角稳定为 2
    sql_ok(&mut a, "BEGIN");
    sql_ok(
        &mut a,
        "INSERT INTO memories (content) VALUES ('事务中数据')",
    );
    sql_ok(&mut a, "COMMIT");
    assert_eq!(rows_of(&sql_ok(&mut b, "SELECT id FROM memories")).len(), 2);

    g.shutdown();
}

// 防御:确认默认停用词 API 在测试构建中仍可用(与 server 启动路径一致)。
#[test]
fn e2e_default_stopwords_load_without_panic() {
    let _set: HashSet<String> = nebula_tokenizer::default_stopword_set();
}
