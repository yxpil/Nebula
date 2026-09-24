//! 结果集表格渲染(MySQL 客户端风格 ASCII 边框)。
//!
//! 约定:
//! - 超长单元格截断并加 `…`(宽度上限 `cell_max` 来自配置)
//! - 多行单元格(key_points 等项目符号列表)换行符替换为 ` ⏎ `
//! - 超过 `max_rows` 行折叠为摘要行,避免刷屏(上限来自配置)

use std::fmt::Write as _;

/// 换行占位符。
const NL: &str = " ⏎ ";

/// 打印一个结果集。空列时打印 "(empty result)"。
pub fn print_table(columns: &[String], rows: &[Vec<String>], cell_max: usize, max_rows: usize) {
    if columns.is_empty() || rows.is_empty() {
        println!("(empty result)");
        return;
    }
    print!("{}", render_table(columns, rows, cell_max, max_rows));
    if rows.len() > max_rows {
        println!("... {} more rows", rows.len() - max_rows);
    }
    println!("{} rows in set", rows.len());
}

/// 渲染结果为字符串(供打印与测试复用)。
pub fn render_table(
    columns: &[String],
    rows: &[Vec<String>],
    cell_max: usize,
    max_rows: usize,
) -> String {
    let display: Vec<Vec<String>> = rows
        .iter()
        .take(max_rows)
        .map(|row| {
            columns
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let cell = row.get(i).map(String::as_str).unwrap_or("");
                    truncate_cell(cell, cell_max)
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
fn truncate_cell(cell: &str, cell_max: usize) -> String {
    let s = cell.replace('\n', NL);
    if s.chars().count() <= cell_max {
        return s;
    }
    let kept: String = s.chars().take(cell_max - 1).collect();
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
        let out = render_table(&cols, &rows, 60, 50);
        assert_eq!(
            out,
            "+----+---------+\n| id | content |\n+----+---------+\n| 1  | hello   |\n+----+---------+\n"
        );
    }

    #[test]
    fn multiline_and_truncation() {
        assert_eq!(truncate_cell("a\nb", 60), "a ⏎ b");
        let long: String = "x".repeat(100);
        let t = truncate_cell(&long, 60);
        assert_eq!(t.chars().count(), 60);
        assert!(t.ends_with('…'));
        // 配置化:更小的 cell_max 截断更早
        let t2 = truncate_cell(&long, 10);
        assert_eq!(t2.chars().count(), 10);
    }

    #[test]
    fn row_cap_note() {
        let cols = vec!["id".to_string()];
        let rows: Vec<Vec<String>> = (0..60).map(|i| vec![i.to_string()]).collect();
        // 60 行 → render 50 行 + "10 more rows" 提示
        let out = render_table(&cols, &rows, 60, 50);
        // 上边框1 + 表头1 + 中边框1 + 50 数据行 + 下边框1 = 54 行
        assert_eq!(out.matches('\n').count(), 54);
        // 配置化:更小的 max_rows 渲染更少行(3 边框 + 1 表头 + 5 数据行)
        let out2 = render_table(&cols, &rows, 60, 5);
        assert_eq!(out2.matches('\n').count(), 3 + 1 + 5);
    }
}
