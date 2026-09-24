//! 停用词:内置默认表(JSON 数据文件,编译期嵌入)与运行时文件加载。
//!
//! 默认表存放在 `stopwords.json`(本 crate 根目录),通过 `include_str!`
//! 嵌入二进制——数据与代码分离,改词表只改 JSON,不需要动 Rust 代码。
//!
//! 运行时停用词来自库旁配置目录的 `stopwords.txt`(一行一词,`#` 开头为注释),
//! 内置默认表仅用于生成该模板与无配置的单元测试。

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

/// 内置默认停用词表(编译期嵌入的 JSON 数据文件)。
const DEFAULT_STOPWORDS_JSON: &str = include_str!("../stopwords.json");

/// `stopwords.json` 的结构。
#[derive(Debug, Deserialize)]
struct StopwordFile {
    /// 词条列表(重复项无影响,加载时去重)。
    stopwords: Vec<String>,
}

/// 内置默认停用词集合(解析嵌入的 JSON;仅用于模板生成与测试)。
///
/// JSON 是编译期嵌入数据,正常不会损坏;但即使损坏也只降级为空集合
/// (停用词不过滤),绝不在运行时 panic。
pub fn default_stopword_set() -> HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        match serde_json::from_str::<StopwordFile>(DEFAULT_STOPWORDS_JSON) {
            Ok(parsed) if !parsed.stopwords.is_empty() => {
                parsed.stopwords.into_iter().collect()
            }
            Ok(_) => {
                eprintln!("warning: embedded stopwords.json is empty; stopword filtering disabled");
                HashSet::new()
            }
            Err(e) => {
                eprintln!("warning: embedded stopwords.json is invalid ({e}); stopword filtering disabled");
                HashSet::new()
            }
        }
    })
    .clone()
}

/// 从文件加载停用词:一行一词,忽略空行与 `#` 注释行,词条去除首尾空白。
pub fn load_stopwords_file(path: &Path) -> std::io::Result<HashSet<String>> {
    let text = std::fs::read_to_string(path)?;
    let mut set = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        set.insert(line.to_string());
    }
    Ok(set)
}

/// 是否命中停用词(查运行时集合,中英文同一张表)。
pub fn is_stopword(term: &str, stopwords: &HashSet<String>) -> bool {
    stopwords.contains(term)
}

/// 是否 CJK 统一表意文字(含扩展)。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0x3400..=0x4DBF    // Ext A
        | 0x20000..=0x2A6DF  // Ext B
        | 0xF900..=0xFAFF    // Compatibility Ideographs
    )
}

/// 以 Unicode 标点/空白为主的词视为噪声(如 jieba 切出的 "。"、" ")。
pub fn is_noise(term: &str) -> bool {
    term.chars().all(|c| c.is_whitespace() || c.is_ascii_punctuation()
        || matches!(c, '，'|'。'|'！'|'？'|'；'|'：'|'、'|'“'|'”'|'‘'|'’'|'（'|'）'|'《'|'》'|'—'|'…'|'·'|'【'|'】'|'「'|'」'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_json_loads() {
        let set = default_stopword_set();
        assert!(set.len() > 200, "expected a sizable default list, got {}", set.len());
        assert!(set.contains("的"));
        assert!(set.contains("rust") == false);
        assert!(set.contains("the"));
        assert!(set.contains("所有权") == false);
    }

    #[test]
    fn json_has_no_empty_entries() {
        // 直接解析 JSON 校验数据质量(未经过去重)
        let parsed: StopwordFile = serde_json::from_str(DEFAULT_STOPWORDS_JSON).unwrap();
        assert!(parsed.stopwords.iter().all(|w| !w.trim().is_empty()));
    }
}
