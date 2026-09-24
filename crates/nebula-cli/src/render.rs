//! 结果集表格渲染(MySQL 客户端风格 ASCII 边框)。
//!
//! 约定:
//! - 超长单元格截断并加 `…`
//! - 多行单元格(key_points 等项目符号列表)换行符替换为 ` ⏎ `
//! - 超过 50 行折叠为摘要行,避免刷屏

use std::fmt::Write as _;

/// 单元格最大显示宽度(字符数)。
const CELL_MAX: usize = 60;
/// 最多展示的行数。
const MAX_ROWS: usize = 50;
/// 换行占位符。
const NL: &str = " ⏎ ";

/// 打印一个结果集。空列时打印 "(empty result)"。
pub fn print_table(columns: &[String], rows: &[Vec<String>]) {
    if columns.is_empty() || rows.is_empty() {
        println!("(empty result)");
        return;
    }
    print!("{}", render_table(columns, rows));
    if rows.len() > MAX_ROWS {
        println!("... {} more rows", rows.len() - MAX_ROWS);
    }
    println!("{} rows in set", rows.len());
}

/// 渲染结果为字符串(供打印与测试复用)。
pub fn render_table(columns: &[String], rows: &[Vec<String>]) -> String {
    let display: Vec<Vec<String>> = rows
        .iter()
        .take(MAX_ROWS)
        .map(|row| {
            columns
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let cell = row.get(i).map(String::as_str).unwrap_or("");
                    truncate_cell(cell)
                })
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in &display {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    push_border(&mut out, &widths);
    push_row(&mut out, columns, &widths);
    push_border(&mut out, &widths);
    for row in &display {
        push_row(&mut out, row, &widths);
    }
    push_border(&mut out, &widths);
    out
}

/// 换行占位 + 超宽截断。
fn truncate_cell(cell: &str) -> String {
    let s = cell.replace('\n', NL);
    if s.chars().count() <= CELL_MAX {
        return s;
    }
    let kept: String = s.chars().take(CELL_MAX - 1).collect();
    format!("{kept}…")
}

fn push_border(out: &mut String, widths: &[usize]) {
    out.push('+');
    for w in widths {
        for _ in 0..(w + 2) {
            out.push('-');
        }
        out.push('+');
    }
    out.push('\n');
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    out.push('|');
    for (i, w) in widths.iter().enumerate() {
        let cell = cells.get(i).map(String::as_str).unwrap_or("");
        let pad = w.saturating_sub(cell.chars().count());
        let _ = write!(out, " {cell}{}", " ".repeat(pad + 1));
        out.push('|');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_table_shape() {
        let cols = vec!["id".to_string(), "content".to_string()];
        let rows = vec![vec!["1".to_string(), "hello".to_string()]];
        let out = render_table(&cols, &rows);
        assert_eq!(
            out,
            "+----+---------+\n| id | content |\n+----+---------+\n| 1  | hello   |\n+----+---------+\n"
        );
    }

    #[test]
    fn multiline_and_truncation() {
        assert_eq!(truncate_cell("a\nb"), "a ⏎ b");
        let long: String = "x".repeat(100);
        let t = truncate_cell(&long);
        assert_eq!(t.chars().count(), CELL_MAX);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn row_cap_note() {
        let cols = vec!["id".to_string()];
        let rows: Vec<Vec<String>> = (0..60).map(|i| vec![i.to_string()]).collect();
        // 60 行 → render 50 行 + "10 more rows" 提示
        let out = render_table(&cols, &rows);
        // 上边框1 + 表头1 + 中边框1 + 50 数据行 + 下边框1 = 54 行
        assert_eq!(out.matches('\n').count(), 54);
    }
}
