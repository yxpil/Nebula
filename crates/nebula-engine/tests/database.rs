//! 引擎端到端集成测试:SQL CRUD、检索策略、自动提取、重开一致性。

use nebula_engine::Database;

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nebula_e2e_{}_{}.ndb", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

const PW: &str = "e2e-test-password";

/// 造 3 条记忆:rust / memory / db 三类关键词。
fn seed(db: &mut Database) {
    db.execute("INSERT INTO memories (content, tags, importance) VALUES ('Rust 的所有权与借用检查在编译期保证内存安全', 'rust, lang', 0.9)").unwrap();
    db.execute("INSERT INTO memories (content, tags) VALUES ('今天学习了 Rust 的生命周期标注,理解了借用检查如何防止悬垂引用', 'rust, study')").unwrap();
    db.execute("INSERT INTO memories (content, tags, importance) VALUES ('MySQL 使用 B+ 树作为索引结构,页是存储的基本单位', 'db, mysql', 0.7)").unwrap();
}

#[test]
fn crud_and_where_clauses() {
    let path = tmp("crud");
    let mut db = Database::create(&path, PW, 4096).unwrap();
    seed(&mut db);

    // SELECT * 计数
    let r = db.execute("SELECT id FROM memories").unwrap();
    assert_eq!(r.rows.len(), 3);

    // keyword = 走倒排索引
    let r = db
        .execute("SELECT content FROM memories WHERE keyword = 'rust'")
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert!(r.rows[0][0].contains("Rust"));

    // tag =
    let r = db
        .execute("SELECT content FROM memories WHERE tag = 'mysql'")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert!(r.rows[0][0].contains("B+ 树"));

    // id =
    let r = db.execute("SELECT content FROM memories WHERE id = 2").unwrap();
    assert_eq!(r.rows.len(), 1);

    // content LIKE 退化全表扫描 + importance 比较 + AND 组合
    let r = db
        .execute("SELECT content FROM memories WHERE content LIKE '%生命周期%' AND importance >= 0.5")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert!(r.rows[0][0].contains("生命周期"));

    // ORDER BY + LIMIT
    let r = db
        .execute("SELECT importance FROM memories ORDER BY importance DESC LIMIT 1")
        .unwrap();
    assert_eq!(r.rows[0][0], "0.90");

    // 未知列报错
    assert!(db.execute("SELECT bogus FROM memories").is_err());

    // UPDATE:改 importance 与 tags
    let r = db
        .execute("UPDATE memories SET importance = 0.3, tags = 'db' WHERE id = 3")
        .unwrap();
    assert_eq!(r.affected, 1);
    let r = db
        .execute("SELECT importance, tags FROM memories WHERE id = 3")
        .unwrap();
    assert_eq!(r.rows[0][0], "0.30");
    assert_eq!(r.rows[0][1], "db");

    // DELETE 需要 WHERE(防误删)
    assert!(db.execute("DELETE FROM memories").is_err());
    let r = db
        .execute("DELETE FROM memories WHERE keyword = 'rust'")
        .unwrap();
    assert_eq!(r.affected, 2);
    let r = db.execute("SELECT id FROM memories").unwrap();
    assert_eq!(r.rows.len(), 1);

    // 元语句
    let r = db.execute("SHOW TABLES").unwrap();
    assert_eq!(r.rows, vec![vec!["memories".to_string()]]);
    let r = db.execute("CHECKPOINT").unwrap();
    assert_eq!(r.message, "checkpoint done");

    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn auto_extract_keywords_and_points() {
    let path = tmp("extract");
    let mut db = Database::create(&path, PW, 4096).unwrap();
    db.execute("INSERT INTO memories (content) VALUES ('Rust 的借用检查器保证内存安全。Rust 没有垃圾回收器。')")
        .unwrap();
    let r = db
        .execute("SELECT content, keywords, key_points FROM memories")
        .unwrap();
    // 关键词包含 rust,且权重已归一化
    assert!(r.rows[0][1].contains("rust"), "{}", r.rows[0][1]);
    // 关键点按句抽取
    assert!(r.rows[0][2].contains("借用检查器"), "{}", r.rows[0][2]);
    // search_by_keyword 便捷 API
    let hits = db.search_by_keyword("main", "rust", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].1 > 0.0);
    // 索引内词项可枚举
    assert!(db.all_terms("main").iter().any(|(t, _)| t == "rust"));
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reopen_preserves_state() {
    let path = tmp("reopen");
    {
        let mut db = Database::create(&path, PW, 4096).unwrap();
        seed(&mut db);
        db.execute("DELETE FROM memories WHERE id = 1").unwrap();
        db.close().unwrap();
    }
    // 重开:DROP 后索引快照必须已落盘,不能复活
    let mut db = Database::open(&path, PW).unwrap();
    assert_eq!(db.memory_count(), 2);
    let r = db.execute("SELECT id FROM memories ORDER BY id").unwrap();
    assert_eq!(
        r.rows,
        vec![vec!["2".to_string()], vec!["3".to_string()]]
    );
    // 新插入继续分配 id(next_record_id 持久化)
    let r = db
        .execute("INSERT INTO memories (content) VALUES ('重启之后的第四条记忆')")
        .unwrap();
    assert!(r.message.contains("main.4"), "{}", r.message);
    db.close().unwrap();

    // 密码错误必须拒绝
    assert!(Database::open(&path, "wrong-password").is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn large_record_spans_pages_and_auto_checkpoint() {
    let path = tmp("pages");
    let mut db = Database::create(&path, PW, 4096).unwrap();
    // 远超单页容量的大记录
    let big = "记忆内容".repeat(5_000); // ~15KB
    db.execute(&format!(
        "INSERT INTO memories (content, tags) VALUES ('{}', 'big')",
        big
    ))
    .unwrap();
    let r = db
        .execute("SELECT content FROM memories WHERE tag = 'big'")
        .unwrap();
    assert_eq!(r.rows[0][0], big);
    // SHOW STATUS 反映页数 > 头页/目录页
    let r = db.execute("SHOW STATUS").unwrap();
    let page_count = r
        .rows
        .iter()
        .find(|row| row[0] == "page_count")
        .map(|row| row[1].parse::<u64>().unwrap())
        .unwrap();
    assert!(page_count > 2, "expected multi-page file, got {page_count}");
    db.close().unwrap();
    let _ = std::fs::remove_file(&path);
}
