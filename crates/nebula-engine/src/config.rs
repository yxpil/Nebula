//! 引擎行为配置。
//!
//! 由 `nebula-config` 从库旁 `nebula.toml` 的 `[engine]`/`[limits]` 段解析后传入,
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
}

impl Default for EngineConfig {
    /// 与配置模板中的默认值一致;未提供配置时使用。
    fn default() -> Self {
        EngineConfig {
            auto_checkpoint: 1000,
            max_content_len: 1 << 20,
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
    }
}
