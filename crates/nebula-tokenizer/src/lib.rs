//! 基于 jieba-rs 的中英文分词器、关键词提取与关键点(关键句)提取。
//!
//! 设计目标:
//! - 中英文混合文本统一处理:英文小写化,CJK 保留原词;
//! - 关键词 = 词频(TF) × 词长权重,非线性归一化到 0..1;
//! - 关键点 = 句子级抽取式打分(关键词覆盖度 + 位置权重 + 长度惩罚 + 相似句去重)。

use nebula_core::Keyword;
use std::collections::{HashMap, HashSet};

pub mod stopwords;

pub use stopwords::{default_stopword_set, load_stopwords_file};

use crate::stopwords::{is_cjk, is_noise, is_stopword};

use jieba_rs::Jieba;

/// 提取行为配置(全部字段可在配置文件中调整)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ExtractorConfig {
    /// 每条记忆最多保留的关键词数。
    pub max_keywords: usize,
    /// 每条记忆最多保留的关键点数。
    pub max_key_points: usize,
    /// 关键词最小字符数(中英文统一按字符数)。
    pub min_keyword_len: usize,
    /// 关键点句子最大字符数,超出截断。
    pub max_key_point_len: usize,
}

impl Default for ExtractorConfig {
    fn default() -> Self {
        ExtractorConfig {
            max_keywords: 16,
            max_key_points: 3,
            min_keyword_len: 2,
            max_key_point_len: 140,
        }
    }
}

/// 分词与关键信息提取器。内部持有 jieba 实例(词典启动加载一次)
/// 与运行时停用词集合(来自库旁配置目录的 stopwords.txt)。
pub struct TextExtractor {
    jieba: Jieba,
    config: ExtractorConfig,
    stopwords: HashSet<String>,
}

impl TextExtractor {
    /// 使用给定提取配置与停用词集合构造。
    pub fn new(config: ExtractorConfig, stopwords: HashSet<String>) -> Self {
        TextExtractor {
            jieba: Jieba::new(),
            config,
            stopwords,
        }
    }

    /// 默认配置 + 内置默认停用词表(单元测试用)。
    pub fn with_default_config() -> Self {
        Self::new(ExtractorConfig::default(), default_stopword_set())
    }

    /// 提取配置(读取上限用于展示截断)。
    pub fn config(&self) -> &ExtractorConfig {
        &self.config
    }

    /// 原始分词:英文统一小写,不过滤停用词/噪声(供搜索引擎式场景使用)。
    pub fn tokenize_raw(&self, text: &str) -> Vec<String> {
        self.jieba
            .cut(text, true)
            .into_iter()
            .map(|t| normalize_term(t))
            .collect()
    }

    /// 过滤后的分词:去停用词、噪声、过短词,保留重复(供 TF 统计)。
    pub fn tokenize(&self, text: &str) -> Vec<String> {
        let min_len = self.config.min_keyword_len;
        self.tokenize_raw(text)
            .into_iter()
            .filter(|t| char_len(t) >= min_len && !is_noise(t) && !is_stopword(t, &self.stopwords))
            .collect()
    }

    /// 从文本中提取带权重的关键词,按权重降序。
    pub fn extract_keywords(&self, text: &str) -> Vec<Keyword> {
        let tokens = self.tokenize(text);
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut tf: HashMap<&str, usize> = HashMap::new();
        for t in &tokens {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }

        // 权重 = 词频对数压缩 × 词长增益;越具体、出现越多的词排越前。
        let mut scored: Vec<(Keyword, f32)> = tf
            .into_iter()
            .map(|(term, count)| {
                let tf_weight = 1.0 + (count as f32).ln();
                let len_weight = 1.0 + 0.12 * (char_len(term).saturating_sub(2).min(6) as f32);
                let raw = tf_weight * len_weight;
                (Keyword::new(term, raw), raw)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.term.cmp(&b.0.term))
        });

        let max_raw = scored
            .first()
            .map(|(_, r)| *r)
            .unwrap_or(1.0)
            .max(f32::EPSILON);
        let mut out: Vec<Keyword> = scored
            .into_iter()
            .take(self.config.max_keywords)
            .map(|(mut k, raw)| {
                k.weight = (raw / max_raw).clamp(0.0, 1.0);
                k
            })
            .collect();
        out.shrink_to_fit();
        out
    }

    /// 提取记忆关键点(关键句)。句子按中英文标点切分,抽取式打分。
    pub fn extract_key_points(&self, text: &str, keywords: &[Keyword]) -> Vec<String> {
        let sentences = split_sentences(text);
        if sentences.is_empty() {
            return Vec::new();
        }

        // 关键词总权重,用于归一化覆盖度。
        let total_weight: f32 = keywords.iter().map(|k| k.weight).sum();
        let total_weight = if total_weight <= 0.0 { 1.0 } else { total_weight };

        let mut scored: Vec<(usize, String, f32)> = Vec::new();
        for (idx, sentence) in sentences.iter().enumerate() {
            let hit: f32 = keywords
                .iter()
                .filter(|k| sentence.contains(&k.term))
                .map(|k| k.weight)
                .sum();
            if hit <= 0.0 {
                continue;
            }
            let coverage = (hit / total_weight).min(1.0);
            // 位置权重:越靠前的句子越可能是主题句。
            let position = 1.0 / (1.0 + 0.08 * idx as f32);
            // 长度惩罚:以 40 字符为中心的高斯,过短/过长都降权。
            let len = char_len(sentence) as f32;
            let length = (-((len - 40.0).powi(2)) / (2.0 * 25.0_f32.powi(2))).exp();
            let score = 0.60 * coverage + 0.25 * position + 0.15 * length;
            scored.push((idx, sentence.clone(), score));
        }
        if scored.is_empty() {
            return Vec::new();
        }
        scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        // 相似句去重(字符二元滑窗 Jaccard)。
        let mut chosen: Vec<(usize, String, f32)> = Vec::new();
        for (idx, sentence, score) in scored {
            if chosen.len() >= self.config.max_key_points {
                break;
            }
            let bigrams = char_bigrams(&sentence);
            let dup = chosen.iter().any(|(_, s, _)| {
                let other = char_bigrams(s);
                jaccard(&bigrams, &other) > 0.75
            });
            if dup {
                continue;
            }
            chosen.push((idx, sentence, score));
        }
        // 输出保持原文出现顺序,便于阅读。
        chosen.sort_by_key(|(idx, _, _)| *idx);
        chosen
            .into_iter()
            .map(|(_, mut s, _)| {
                if char_len(&s) > self.config.max_key_point_len {
                    s = s.chars().take(self.config.max_key_point_len).collect();
                }
                s
            })
            .collect()
    }
}

impl Default for TextExtractor {
    fn default() -> Self {
        Self::with_default_config()
    }
}

/// 归一化词项:去首尾空白,英文小写;CJK 不动。
fn normalize_term(t: &str) -> String {
    let t = t.trim();
    if t.chars().any(|c| c.is_ascii_alphabetic()) && !t.chars().any(is_cjk) {
        t.to_lowercase()
    } else {
        t.to_string()
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// 按中英文句读与换行切句,保留句子内原始文本。
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        buf.push(c);
        let is_end = matches!(
            c,
            '。' | '！' | '？' | '.' | '!' | '?' | ';' | '；' | '\n'
        );
        if is_end {
            // 吞掉连续收尾标点与右引号,避免句子以引号结尾被截断。
            while let Some(&next) = chars.peek() {
                if matches!(next, '"' | '”' | '\'' | '’' | ')' | '）' | '.' | '。' | '！' | '？') {
                    buf.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            let s = buf.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            buf.clear();
        }
    }
    let s = buf.trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

fn char_bigrams(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 2 {
        return chars.iter().map(|c| c.to_string()).collect();
    }
    chars.windows(2).map(|w| w.iter().collect()).collect()
}

fn jaccard(a: &[String], b: &[String]) -> f32 {
    use std::collections::HashSet;
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex() -> TextExtractor {
        TextExtractor::with_default_config()
    }

    #[test]
    fn tokenize_cn_en_mixed() {
        let e = ex();
        let tokens = e.tokenize("Rust 的所有权机制让内存安全无需 GC。");
        assert!(tokens.contains(&"rust".to_string()));
        assert!(!tokens.contains(&"的".to_string()));
        assert!(!tokens.iter().any(|t| t.is_empty()));
    }

    #[test]
    fn keywords_rank_content_terms() {
        let e = ex();
        let text = "Rust 的所有权机制让内存安全无需垃圾回收。Rust 编译器在编译期检查借用检查。\
                    Rust 的并发安全来自所有权与类型系统。";
        let kw = e.extract_keywords(text);
        assert!(!kw.is_empty());
        let terms: Vec<&str> = kw.iter().map(|k| k.term.as_str()).collect();
        assert!(terms.contains(&"rust"));
        assert!(terms.contains(&"所有权"));
        assert!(terms.contains(&"编译"));
        // 权重降序且归一化
        let w: Vec<f32> = kw.iter().map(|k| k.weight).collect();
        assert!(w[0] <= 1.0 + 1e-6);
        assert!(w.windows(2).all(|p| p[0] >= p[1] - 1e-6));
    }

    #[test]
    fn key_points_are_sentences() {
        let e = ex();
        let text = "今天学习了 Rust 的所有权机制。所有权在编译期保证内存安全。\
                    我们下午还讨论了 BTreeMap 的适用场景,它适合有序键范围查询。\
                    BTreeMap 的插入删除都是 O(log n)。";
        let kw = e.extract_keywords(text);
        let kps = e.extract_key_points(text, &kw);
        assert!(!kps.is_empty());
        assert!(kps.iter().any(|s| s.contains("所有权")));
        // 关键点按原文顺序输出
        let pos: Vec<usize> = kps
            .iter()
            .map(|s| text.find(s).unwrap_or(usize::MAX))
            .collect();
        assert!(pos.windows(2).all(|p| p[0] < p[1]), "{pos:?}");
    }

    #[test]
    fn empty_and_noise_inputs() {
        let e = ex();
        assert!(e.extract_keywords("").is_empty());
        assert!(e.extract_keywords("。。。").is_empty());
        let kw = e.extract_keywords("hello world");
        assert!(!kw.is_empty());
        let kps = e.extract_key_points("hello world hello world", &kw);
        assert!(kps.iter().all(|s| !s.is_empty()));
    }

    #[test]
    fn split_sentences_keeps_quotes() {
        let s = split_sentences("他说:\"今天天气不错。\" 然后离开了。下一句");
        assert_eq!(s, vec!["他说:\"今天天气不错。\"", "然后离开了。", "下一句"]);
    }

    #[test]
    fn custom_stopword_set_overrides_default() {
        // 空停用词表时,默认停用词(如 "的")不再被过滤的语义由调用方保证;
        // 这里验证自定义集合能拦下原本不在默认表里的词。
        let cfg = ExtractorConfig {
            min_keyword_len: 1,
            ..ExtractorConfig::default()
        };
        let mut custom = default_stopword_set();
        custom.insert("rust".to_string());
        let e = TextExtractor::new(cfg, custom);
        let tokens = e.tokenize("rust 所有权");
        assert!(!tokens.contains(&"rust".to_string()));
        assert!(tokens.contains(&"所有权".to_string()));
    }

    #[test]
    fn stopwords_file_roundtrip() {
        use crate::stopwords::load_stopwords_file;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("nebula_stop_{}.txt", std::process::id()));
        std::fs::write(&path, "# 注释\nrust\n\n  内存  \n").unwrap();
        let set = load_stopwords_file(&path).unwrap();
        assert!(set.contains("rust"));
        assert!(set.contains("内存"));
        assert_eq!(set.len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
