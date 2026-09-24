//! 交互式 REPL:本地模式(直连引擎)与远程模式(走 TCP 客户端)共用一套命令集。
//!
//! 提示符、表格宽度、行数上限等展示参数全部来自库旁配置([`LoadedConfig`])。

use std::io::{BufRead, Write};

use nebula_config::LoadedConfig;
use nebula_core::Result;
use nebula_engine::Database;
use nebula_server::{Client, Response};

use crate::render::print_table;

/// REPL 的帮助文本。
const HELP: &str = "\
可用命令:
  <SQL>           执行一条 SQL(INSERT/SELECT/DELETE/UPDATE/CHECKPOINT/SEARCH/RELATED ...;可用分号分隔多条)
  help            显示本帮助
  exit | quit     退出(本地模式会自动 CHECKPOINT)
示例:
  INSERT INTO memories (content, tags) VALUES ('内容', 'tag1,tag2');
  SELECT id, content, keywords FROM memories WHERE keyword = 'rust';
  SELECT * FROM memories WHERE content LIKE '%关键词%' ORDER BY id LIMIT 10;
  SEARCH 'rust 所有权' LIMIT 5;
  RELATED TO 12 LIMIT 5;
  RELATED '编译期检查 内存安全' LIMIT 5;
说明:
  SEARCH / RELATED 按 BM25 相关度 + 共现联想 + 关键词相似度重排,
  返回列 id/score/content/keywords/tags/importance(score 为相关度分数)。
  检索参数(k1/b/联想跳数/衰减/相似度权重等)见库旁配置 [engine.search] 段。";

/// 本地模式:直接驱动 [`Database`]。
pub fn run_local(mut db: Database, cfg: &LoadedConfig) -> Result<()> {
    println!(
        "connected to {} ({} memories). 输入 help 查看用法, exit 退出。",
        db.path().display(),
        db.memory_count()
    );
    let prompt = cfg.config.cli.prompt.clone();
    let (cell_max, max_rows) = (cfg.config.cli.cell_max, cfg.config.cli.max_rows);
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{prompt}");
        flush_stdout();
        let Some(line) = read_line(&mut lines) else {
            break; // EOF(Ctrl+C / Ctrl+Z / 管道关闭)
        };
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        match sql.to_lowercase().as_str() {
            "exit" | "quit" | "\\q" => break,
            "help" | "\\h" | "?" => {
                println!("{HELP}");
                continue;
            }
            _ => {}
        }
        // 单行可含多条以分号分隔的语句,逐条执行并打印结果。
        match db.execute_script(sql) {
            Ok(results) => {
                for r in &results {
                    print_result(&r.columns, &r.rows, &r.message, r.affected, cell_max, max_rows);
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }
    // 退出前落盘,保证索引快照与目录持久化。
    db.close()?;
    println!("bye.");
    Ok(())
}

/// 远程模式:通过 [`Client`] 走加密 TCP 会话。
pub fn run_remote(mut client: Client, cfg: &LoadedConfig) -> Result<()> {
    println!(
        "connected to {}. 输入 help 查看用法, exit 断开。",
        client.peer_addr()
    );
    let prompt = cfg.config.cli.prompt.clone();
    let (cell_max, max_rows) = (cfg.config.cli.cell_max, cfg.config.cli.max_rows);
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{prompt}");
        flush_stdout();
        let Some(line) = read_line(&mut lines) else {
            break;
        };
        let sql = line.trim();
        if sql.is_empty() {
            continue;
        }
        match sql.to_lowercase().as_str() {
            "exit" | "quit" | "\\q" => break,
            "help" | "\\h" | "?" => {
                println!("{HELP}");
                continue;
            }
            _ => {}
        }
        match client.sql(sql) {
            Ok(resp) if resp.ok => {
                // 多语句时 script 携带全部子结果,主字段是最后一条;
                // 单语句时直接渲染主结果。
                if let Some(list) = &resp.script {
                    for sub in list {
                        print_response(sub, cell_max, max_rows);
                    }
                } else {
                    print_response(&resp, cell_max, max_rows);
                }
            }
            Ok(resp) => {
                eprintln!("error: {}", resp.error.unwrap_or_else(|| "unknown".into()))
            }
            Err(e) => {
                eprintln!("error: {e}");
                if matches!(e, nebula_core::Error::Auth(_)) {
                    break;
                }
            }
        }
    }
    let _ = client.close();
    println!("bye.");
    Ok(())
}

fn read_line<I: Iterator<Item = std::io::Result<String>>>(lines: &mut I) -> Option<String> {
    match lines.next() {
        Some(Ok(line)) => Some(line),
        Some(Err(e)) => {
            eprintln!("read error: {e}");
            None
        }
        None => None,
    }
}

fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

fn print_result(
    columns: &[String],
    rows: &[Vec<String>],
    message: &str,
    affected: u64,
    cell_max: usize,
    max_rows: usize,
) {
    if columns.is_empty() && rows.is_empty() {
        if !message.is_empty() {
            println!("{message}");
        } else {
            println!("OK, affected {affected}");
        }
    } else {
        print_table(columns, rows, cell_max, max_rows);
    }
}

/// 渲染一条服务端响应(复用本地模式的打印逻辑)。
fn print_response(r: &Response, cell_max: usize, max_rows: usize) {
    let empty: Vec<String> = Vec::new();
    let empty_rows: Vec<Vec<String>> = Vec::new();
    print_result(
        r.columns.as_ref().unwrap_or(&empty),
        r.rows.as_ref().unwrap_or(&empty_rows),
        r.message.as_deref().unwrap_or(""),
        r.affected,
        cell_max,
        max_rows,
    );
}

#[cfg(test)]
mod tests {
    /// REPL 元命令判定(与 main 循环同一套规则)。
    fn is_meta(line: &str) -> bool {
        matches!(
            line.trim().to_lowercase().as_str(),
            "exit" | "quit" | "\\q" | "help" | "\\h" | "?"
        )
    }

    #[test]
    fn meta_commands_recognized() {
        assert!(is_meta("exit"));
        assert!(is_meta("QUIT"));
        assert!(is_meta("  help  "));
        assert!(is_meta("\\q"));
        assert!(!is_meta("SELECT 1"));
        assert!(!is_meta("insert into memories values ('exit')"));
    }
}
