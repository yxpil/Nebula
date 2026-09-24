//! 引擎行为配置。
//!
//! 由 `nebula-config` 从库旁 `nebula.toml` 的 `[engine]` 段解析后传入,
//! 引擎自身不读取任何配置文件(保持 crate 独立)。
//! 分词与停用词参数属于 `nebula_tokenizer::ExtractorConfig`,另行注入。

/// 引擎行为配置(全部字段可在配置文件中调整)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// 累计多少条 INSERT 后自动写索引快照。
    pub auto_checkpoint: u64,
    /// 单条记忆内容上限(字符数)。
    pub max_content_len: usize,
    /// 检索与联想配置(TOML 中对应 `[engine.search]` 子段)。
    pub search: SearchConfig,
}

impl Default for EngineConfig {
    /// 与配置模板中的默认值一致;未提供配置时使用。
    fn default() -> Self {
        EngineConfig {
            auto_checkpoint: 1000,
            max_content_len: 1 << 20,
            search: SearchConfig::default(),
        }
    }
}

/// 检索与联想配置(BM25 排序 / 共现图扩展 / 种子相似度重排)。
///
/// 三层打分:
/// 1. BM25:查询词(含共现扩展词)对候选记忆的相关度;
/// 2. 种子相似度:关键词权重向量的余弦相似度(RELATED 语句);
/// 3. 联合重排:`score = bm25 + similarity_weight * cosine`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// BM25 词频饱和参数 k1(0 = 完全不看词频,1.2 为经典默认)。
    pub k1: f32,
    /// BM25 文档长度归一化强度 b(0 = 不归一化,1 = 完全归一化)。
    pub b: f32,
    /// 共现图查询扩展跳数(0 = 关闭联想扩展,1 ~ 2)。
    pub hops: u8,
    /// 每个查询最多引入的扩展词数(按共现权重取前 N)。
    pub expansion_limit: usize,
    /// 扩展词权重衰减:每多一跳,权重乘以该系数。
    pub expansion_decay: f32,
    /// 联合重排中种子相似度(余弦)的权重。
    pub similarity_weight: f32,
    /// 相关度下限:低于该分数的结果不返回(0 = 不过滤)。
    pub min_score: f32,
    /// SEARCH / RELATED 未显式 LIMIT 时的默认返回条数。
    pub default_limit: usize,
}

impl Default for SearchConfig {
    /// 与配置模板中的默认值一致;未提供配置时使用。
    fn default() -> Self {
        SearchConfig {
            k1: 1.2,
            b: 0.75,
            hops: 1,
            expansion_limit: 8,
            expansion_decay: 0.5,
            similarity_weight: 0.5,
            min_score: 0.0,
            default_limit: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_match_template() {
        let c = EngineConfig::default();
        assert_eq!(c.auto_checkpoint, 1000);
        assert_eq!(c.max_content_len, 1 << 20);
        let s = c.search;
        assert_eq!(s.k1, 1.2);
        assert_eq!(s.b, 0.75);
        assert_eq!(s.hops, 1);
        assert_eq!(s.expansion_limit, 8);
        assert_eq!(s.expansion_decay, 0.5);
        assert_eq!(s.similarity_weight, 0.5);
        assert_eq!(s.min_score, 0.0);
        assert_eq!(s.default_limit, 10);
    }
}
