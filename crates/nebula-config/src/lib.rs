//! Nebula 库旁配置:每个数据库文件旁边一个配置文件夹。
//!
//! 目录约定(数据库为 `data/mymemory.ndb` 时):
//! ```text
//! data/mymemory.ndb           数据库文件(加密二进制)
//! data/mymemory.ndb.conf.d/   配置文件夹(本 crate 管理)
//!   ├── nebula.toml           主配置(create 时生成,可手工编辑)
//!   └── stopwords.txt         停用词表(一行一词)
//! ```
//!
//! 设计要点:
//! - 所有行为参数(页大小、检查点阈值、内容上限、分词、停用词、检索联想、
//!   缓存容量与热点预加载、默认地址、CLI 展示、密码策略)都在配置文件中,
//!   代码内没有可调常量;
//! - 缺省字段回退内置默认值(与生成模板一致),保证向前兼容;
//! - 本 crate 只负责“读/写/校验”,把解析结果转交各 crate 自己的配置类型
//!   ([`nebula_engine::EngineConfig`] / [`nebula_tokenizer::ExtractorConfig`]);
//! - 二进制格式与协议结构常量(魔数、偏移、nonce/tag 长度、Argon2 强度等)
//!   不属于配置项——它们是 `.ndb` 格式与 TCP 协议的定义,保持代码内固定。

use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use nebula_core::{Error, Result};
use nebula_engine::EngineConfig;
use nebula_storage::{MAX_PAGE_SIZE, MIN_PAGE_SIZE};
use nebula_tokenizer::{default_stopword_set, load_stopwords_file, ExtractorConfig};

/// 配置目录后缀:`<库文件>.conf.d`。
pub const CONFIG_DIR_SUFFIX: &str = "conf.d";
/// 主配置文件名。
pub const CONFIG_FILE: &str = "nebula.toml";
/// 默认停用词表文件名。
pub const STOPWORDS_FILE: &str = "stopwords.txt";

/// 数据库文件对应的库旁配置目录路径:`foo.ndb` → `foo.ndb.conf.d`。
pub fn config_dir_for_db(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(format!(".{CONFIG_DIR_SUFFIX}"));
    PathBuf::from(s)
}

// ---------------- 配置段 ----------------

/// `[storage]` 存储段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// 建库页大小(字节,2 的幂,4096..=65536);仅 create 时生效,之后以库文件头为准。
    pub page_size: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig { page_size: 4096 }
    }
}

/// `[tokenizer]` 分词段(扁平展开 ExtractorConfig 的四个字段)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TokenizerSection {
    /// 提取参数(max_keywords / max_key_points / min_keyword_len / max_key_point_len)。
    #[serde(flatten)]
    pub extract: ExtractorConfig,
    /// 停用词表文件(相对本配置目录)。
    pub stopwords_file: String,
}

impl Default for TokenizerSection {
    fn default() -> Self {
        TokenizerSection {
            extract: ExtractorConfig::default(),
            stopwords_file: STOPWORDS_FILE.to_string(),
        }
    }
}

/// `[server]` 服务端段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// serve / connect 的默认地址(host:port)。
    pub default_addr: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            default_addr: "127.0.0.1:7777".to_string(),
        }
    }
}

/// `[cli]` 命令行展示段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CliConfig {
    /// 表格单元格最大显示宽度(字符)。
    pub cell_max: usize,
    /// 表格最多显示行数。
    pub max_rows: usize,
    /// REPL 提示符。
    pub prompt: String,
}

impl Default for CliConfig {
    fn default() -> Self {
        CliConfig {
            cell_max: 60,
            max_rows: 50,
            prompt: "nebula> ".to_string(),
        }
    }
}

/// `[auth]` 密码策略段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// 新建密码最小长度。
    pub password_min_len: usize,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            password_min_len: 8,
        }
    }
}

/// 完整配置:一个库旁目录的全部可调参数。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NebulaConfig {
    pub storage: StorageConfig,
    pub engine: EngineConfig,
    pub tokenizer: TokenizerSection,
    pub server: ServerConfig,
    pub cli: CliConfig,
    pub auth: AuthConfig,
}

impl NebulaConfig {
    /// 解析 TOML 文本(缺省字段回退内置默认值)。
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::Config(format!("invalid config: {e}")))
    }

    /// 从配置目录加载:`nebula.toml` + 停用词文件。
    /// 停用词文件不存在时为空集合(不影响其它配置生效)。
    pub fn load_dir(dir: &Path) -> Result<LoadedConfig> {
        let toml_path = dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&toml_path).map_err(|e| {
            Error::Config(format!(
                "cannot read config {}: {e}",
                toml_path.display()
            ))
        })?;
        let config: NebulaConfig = toml::from_str(&text).map_err(|e| {
            Error::Config(format!(
                "invalid config {}: {e}",
                toml_path.display()
            ))
        })?;

        let sw_path = config.stopwords_path(dir);
        let stopwords = if sw_path.is_file() {
            load_stopwords_file(&sw_path).map_err(|e| {
                Error::Config(format!(
                    "cannot read stopwords {}: {e}",
                    sw_path.display()
                ))
            })?
        } else {
            HashSet::new()
        };

        let loaded = LoadedConfig {
            config,
            dir: dir.to_path_buf(),
            stopwords,
        };
        loaded.validate()?;
        Ok(loaded)
    }

    /// 在配置目录生成默认配置(nebula.toml 模板 + 默认停用词表)。
    pub fn write_default_dir(dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).map_err(|e| {
            Error::Config(format!("cannot create config dir {}: {e}", dir.display()))
        })?;
        fs::write(dir.join(CONFIG_FILE), CONFIG_TEMPLATE).map_err(|e| {
            Error::Config(format!("cannot write config template: {e}"))
        })?;
        fs::write(dir.join(STOPWORDS_FILE), stopwords_template()).map_err(|e| {
            Error::Config(format!("cannot write stopwords template: {e}"))
        })?;
        Ok(())
    }

    /// 停用词文件的绝对路径(停用词路径限制在配置目录内)。
    pub fn stopwords_path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.tokenizer.stopwords_file)
    }
}

/// 加载后的配置:完整参数 + 停用词集合。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: NebulaConfig,
    /// 配置目录(错误信息 / 诊断用)。
    pub dir: PathBuf,
    /// 停用词集合(文件不存在时为空)。
    pub stopwords: HashSet<String>,
}

impl LoadedConfig {
    /// 校验全部参数;返回第一个非法项。
    pub fn validate(&self) -> Result<()> {
        let c = &self.config;
        if !c.storage.page_size.is_power_of_two()
            || !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&c.storage.page_size)
        {
            return Err(Error::Config(format!(
                "storage.page_size must be a power of two in {MIN_PAGE_SIZE}..={MAX_PAGE_SIZE}, got {}",
                c.storage.page_size
            )));
        }
        if c.engine.auto_checkpoint == 0 {
            return Err(Error::Config(
                "engine.auto_checkpoint must be >= 1".into(),
            ));
        }
        if c.engine.max_content_len == 0 {
            return Err(Error::Config(
                "engine.max_content_len must be >= 1".into(),
            ));
        }
        let s = &c.engine.search;
        if !s.k1.is_finite() || s.k1 < 0.0 {
            return Err(Error::Config(
                "engine.search.k1 must be a finite number >= 0".into(),
            ));
        }
        if !s.b.is_finite() || !(0.0..=1.0).contains(&s.b) {
            return Err(Error::Config(
                "engine.search.b must be a finite number in 0.0..=1.0".into(),
            ));
        }
        if s.hops > 2 {
            return Err(Error::Config("engine.search.hops must be 0..=2".into()));
        }
        if s.expansion_limit == 0 {
            return Err(Error::Config(
                "engine.search.expansion_limit must be >= 1".into(),
            ));
        }
        if !s.expansion_decay.is_finite() || !(0.0..=1.0).contains(&s.expansion_decay) {
            return Err(Error::Config(
                "engine.search.expansion_decay must be a finite number in 0.0..=1.0".into(),
            ));
        }
        if !s.similarity_weight.is_finite() || s.similarity_weight < 0.0 {
            return Err(Error::Config(
                "engine.search.similarity_weight must be a finite number >= 0".into(),
            ));
        }
        if !s.min_score.is_finite() || s.min_score < 0.0 {
            return Err(Error::Config(
                "engine.search.min_score must be a finite number >= 0".into(),
            ));
        }
        if s.default_limit == 0 {
            return Err(Error::Config(
                "engine.search.default_limit must be >= 1".into(),
            ));
        }
        let k = &c.engine.cache;
        if k.doc_cache_capacity == 0 && k.hot_preload > 0 {
            return Err(Error::Config(
                "engine.cache.hot_preload must be 0 when engine.cache.doc_cache_capacity = 0"
                    .into(),
            ));
        }
        if c.tokenizer.extract.max_keywords == 0 || c.tokenizer.extract.max_key_points == 0 {
            return Err(Error::Config(
                "tokenizer.max_keywords / max_key_points must be >= 1".into(),
            ));
        }
        if c.tokenizer.extract.min_keyword_len == 0
            || c.tokenizer.extract.max_key_point_len == 0
        {
            return Err(Error::Config(
                "tokenizer.min_keyword_len / max_key_point_len must be >= 1".into(),
            ));
        }
        if c.cli.cell_max < 4 {
            return Err(Error::Config("cli.cell_max must be >= 4".into()));
        }
        if c.cli.max_rows == 0 {
            return Err(Error::Config("cli.max_rows must be >= 1".into()));
        }
        if c.cli.prompt.is_empty() {
            return Err(Error::Config("cli.prompt must not be empty".into()));
        }
        if c.auth.password_min_len == 0 {
            return Err(Error::Config(
                "auth.password_min_len must be >= 1".into(),
            ));
        }
        c.server
            .default_addr
            .parse::<SocketAddr>()
            .map_err(|_| {
                Error::Config(format!(
                    "server.default_addr '{}' is not a valid host:port",
                    c.server.default_addr
                ))
            })?;
        self.validate_stopwords_path()
    }

    /// 停用词路径必须是配置目录内的单个普通文件名。
    fn validate_stopwords_path(&self) -> Result<()> {
        let raw = &self.config.tokenizer.stopwords_file;
        if raw.is_empty() {
            return Err(Error::Config(
                "tokenizer.stopwords_file must not be empty".into(),
            ));
        }
        let mut comps = Path::new(raw).components();
        let ok = matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none();
        if !ok {
            return Err(Error::Config(format!(
                "tokenizer.stopwords_file must be a plain file name inside the config dir, got '{raw}'"
            )));
        }
        Ok(())
    }
}

/// 默认配置模板(带注释)。测试保证其解析结果等于内置默认值。
pub const CONFIG_TEMPLATE: &str = "\
# Nebula 记忆数据库配置文件
# 位置:<数据库文件>.conf.d/nebula.toml,create 时自动生成;可手工编辑。
# 缺省的字段会回退到下方列出的内置默认值。

[storage]
# 建库页大小(字节,2 的幂,4096..=65536);仅 create 时生效,之后以库文件头为准
page_size = 4096

[engine]
# 累计多少条 INSERT 后自动写索引快照
auto_checkpoint = 1000
# 单条记忆内容上限(字符数)
max_content_len = 1048576

[engine.search]
# BM25 词频饱和参数 k1(0 = 完全不看词频,1.2 为经典默认)
k1 = 1.2
# BM25 文档长度归一化强度 b(0 = 不归一化,1 = 完全归一化)
b = 0.75
# 共现图查询扩展跳数(0 = 关闭联想扩展,1 ~ 2)
hops = 1
# 每个查询最多引入的扩展词数(按共现权重取前 N)
expansion_limit = 8
# 扩展词权重衰减:每多一跳,权重乘以该系数
expansion_decay = 0.5
# 联合重排中种子相似度(余弦)的权重(0 = 只用 BM25)
similarity_weight = 0.5
# 相关度下限:低于该分数的结果不返回(0 = 不过滤)
min_score = 0.0
# SEARCH / RELATED 未显式 LIMIT 时的默认返回条数
default_limit = 10

[engine.cache]
# 查询缓存容量:最近多少条 SEARCH / RELATED 的排序结果常驻内存,重复检索免打分(0 = 关闭)
query_cache_capacity = 256
# 文档缓存容量:最近读过多少条记忆记录常驻内存,避免重复解密存储页(0 = 关闭)
doc_cache_capacity = 128
# 打开库时按历史读取热度预加载进文档缓存的条数(优先加载热点,提高速度;0 = 不预加载)
hot_preload = 32

[tokenizer]
# 每条记忆实际提取的关键词数(同时作为 SELECT 展示截断上限)
max_keywords = 16
# 每条记忆实际提取的关键点数
max_key_points = 3
# 关键词最小字符数(中英文统一按字符数)
min_keyword_len = 2
# 关键点句子最大字符数,超出截断
max_key_point_len = 140
# 停用词表文件(相对本配置目录)
stopwords_file = \"stopwords.txt\"

[server]
# serve / connect 命令的默认地址
default_addr = \"127.0.0.1:7777\"

[cli]
# 表格单元格最大显示宽度(字符数)
cell_max = 60
# 表格最多显示行数
max_rows = 50
# REPL 提示符
prompt = \"nebula> \"

[auth]
# 新建密码的最小长度
password_min_len = 8
";

/// 默认停用词表模板文本(一行一词,# 注释;词条排序保证输出稳定可复现)。
pub fn stopwords_template() -> String {
    let mut out = String::from(
        "# Nebula 停用词表:一行一词,'#' 开头为注释行。\n\
         # 命中的词不进入关键词与关键点提取;修改后对后续 open/serve 生效。\n\
         # 可将整表清空(保留注释)以关闭停用词过滤。\n\n",
    );
    let mut words: Vec<String> = default_stopword_set().into_iter().collect();
    words.sort();
    for w in words {
        out.push_str(&w);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("nebula_cfg_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn template_parses_to_defaults() {
        let parsed = NebulaConfig::from_toml(CONFIG_TEMPLATE).unwrap();
        assert_eq!(parsed, NebulaConfig::default());
    }

    #[test]
    fn partial_config_falls_back_to_defaults() {
        let cfg = NebulaConfig::from_toml("[engine]\nauto_checkpoint = 5\n").unwrap();
        assert_eq!(cfg.engine.auto_checkpoint, 5);
        assert_eq!(cfg.storage.page_size, NebulaConfig::default().storage.page_size);
        assert_eq!(cfg.tokenizer.extract, ExtractorConfig::default());
        assert_eq!(cfg.cli.prompt, "nebula> ");
    }

    #[test]
    fn search_section_parses() {
        let cfg = NebulaConfig::from_toml(
            "[engine.search]\nk1 = 2.0\nb = 0.5\nhops = 2\nexpansion_limit = 3\n\
             expansion_decay = 0.25\nsimilarity_weight = 1.5\nmin_score = 0.1\ndefault_limit = 5\n",
        )
        .unwrap();
        let s = cfg.engine.search;
        assert_eq!((s.k1, s.b), (2.0, 0.5));
        assert_eq!(s.hops, 2);
        assert_eq!(s.expansion_limit, 3);
        assert_eq!(s.expansion_decay, 0.25);
        assert_eq!(s.similarity_weight, 1.5);
        assert_eq!(s.min_score, 0.1);
        assert_eq!(s.default_limit, 5);
        // 只给部分字段:其余回退内置默认
        let cfg = NebulaConfig::from_toml("[engine.search]\nhops = 0\n").unwrap();
        assert_eq!(cfg.engine.search.hops, 0);
        assert_eq!(cfg.engine.search.k1, 1.2);
        assert_eq!(cfg.engine.search.b, 0.75);
    }

    #[test]
    fn validation_rejects_bad_search_values() {
        let bad = [
            "k1 = -0.5",
            "b = 1.5",
            "b = -0.1",
            "hops = 3",
            "expansion_limit = 0",
            "expansion_decay = 1.5",
            "similarity_weight = -1.0",
            "min_score = -0.01",
            "default_limit = 0",
        ];
        for kv in bad {
            let cfg = NebulaConfig::from_toml(&format!("[engine.search]\n{kv}\n")).unwrap();
            let loaded = LoadedConfig {
                config: cfg,
                dir: PathBuf::from("."),
                stopwords: HashSet::new(),
            };
            let err = loaded.validate().unwrap_err().to_string();
            assert!(err.contains("engine.search"), "'{kv}' 的错误信息应带段前缀: {err}");
        }
        // 边界合法值:hops = 0(关闭扩展)、k1 = 0、min_score = 0、default_limit = 1
        for ok in [
            "[engine.search]\nhops = 0\n",
            "[engine.search]\nk1 = 0\n",
            "[engine.search]\nb = 1.0\n",
            "[engine.search]\nmin_score = 0\n",
            "[engine.search]\ndefault_limit = 1\n",
        ] {
            let cfg = NebulaConfig::from_toml(ok).unwrap();
            let loaded = LoadedConfig {
                config: cfg,
                dir: PathBuf::from("."),
                stopwords: HashSet::new(),
            };
            loaded.validate().unwrap();
        }
    }

    #[test]
    fn cache_section_parses() {
        let cfg = NebulaConfig::from_toml(
            "[engine.cache]\nquery_cache_capacity = 8\ndoc_cache_capacity = 4\nhot_preload = 0\n",
        )
        .unwrap();
        let k = cfg.engine.cache;
        assert_eq!(k.query_cache_capacity, 8);
        assert_eq!(k.doc_cache_capacity, 4);
        assert_eq!(k.hot_preload, 0);
        // 只给部分字段:其余回退内置默认
        let cfg = NebulaConfig::from_toml("[engine.cache]\nhot_preload = 64\n").unwrap();
        assert_eq!(cfg.engine.cache.hot_preload, 64);
        assert_eq!(cfg.engine.cache.query_cache_capacity, 256);
        assert_eq!(cfg.engine.cache.doc_cache_capacity, 128);
    }

    #[test]
    fn validation_rejects_preload_without_doc_cache() {
        // 文档缓存关闭时不允许预加载(没有缓存可加载)
        let cfg =
            NebulaConfig::from_toml("[engine.cache]\ndoc_cache_capacity = 0\nhot_preload = 32\n")
                .unwrap();
        let loaded = LoadedConfig {
            config: cfg,
            dir: PathBuf::from("."),
            stopwords: HashSet::new(),
        };
        let err = loaded.validate().unwrap_err().to_string();
        assert!(
            err.contains("engine.cache"),
            "错误信息应带段前缀: {err}"
        );
        // 合法:两者同时为 0,或只关查询缓存 / 只关预加载
        for ok in [
            "[engine.cache]\ndoc_cache_capacity = 0\nhot_preload = 0\n",
            "[engine.cache]\nhot_preload = 0\n",
            "[engine.cache]\nquery_cache_capacity = 0\n",
            "[engine.cache]\ndoc_cache_capacity = 1\nhot_preload = 64\n",
        ] {
            let cfg = NebulaConfig::from_toml(ok).unwrap();
            let loaded = LoadedConfig {
                config: cfg,
                dir: PathBuf::from("."),
                stopwords: HashSet::new(),
            };
            loaded.validate().unwrap();
        }
        // 只关文档缓存而不关预加载:hot_preload 回退默认 32 > 0,同样拒绝
        let cfg = NebulaConfig::from_toml("[engine.cache]\ndoc_cache_capacity = 0\n").unwrap();
        let loaded = LoadedConfig {
            config: cfg,
            dir: PathBuf::from("."),
            stopwords: HashSet::new(),
        };
        assert!(loaded.validate().is_err());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let cfg = NebulaConfig::from_toml("[storage]\npage_size = 8192\nfuture_key = 1\n").unwrap();
        assert_eq!(cfg.storage.page_size, 8192);
    }

    #[test]
    fn config_dir_naming() {
        let p = Path::new("data/mymemory.ndb");
        assert_eq!(
            config_dir_for_db(p),
            PathBuf::from("data/mymemory.ndb.conf.d")
        );
    }

    #[test]
    fn write_then_load_roundtrip() {
        let dir = tmp_dir("roundtrip");
        NebulaConfig::write_default_dir(&dir).unwrap();
        assert!(dir.join(CONFIG_FILE).is_file());
        assert!(dir.join(STOPWORDS_FILE).is_file());
        let loaded = LoadedConfig {
            config: NebulaConfig::load_dir(&dir).unwrap().config,
            dir: dir.clone(),
            stopwords: HashSet::new(),
        };
        loaded.validate().unwrap();
        let full = NebulaConfig::load_dir(&dir).unwrap();
        assert_eq!(full.config, NebulaConfig::default());
        // 默认模板内含停用词,加载后应非空且含典型词
        assert!(full.stopwords.contains("rust") == false); // rust 不是停用词
        assert!(full.stopwords.contains("的"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edited_config_is_picked_up() {
        let dir = tmp_dir("edited");
        NebulaConfig::write_default_dir(&dir).unwrap();
        let toml_path = dir.join(CONFIG_FILE);
        let mut text = fs::read_to_string(&toml_path).unwrap();
        text = text.replace("auto_checkpoint = 1000", "auto_checkpoint = 7");
        text = text.replace("cell_max = 60", "cell_max = 20");
        fs::write(&toml_path, text).unwrap();
        let loaded = NebulaConfig::load_dir(&dir).unwrap();
        assert_eq!(loaded.config.engine.auto_checkpoint, 7);
        assert_eq!(loaded.config.cli.cell_max, 20);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_stopwords_file() {
        let dir = tmp_dir("stopwords");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("my_stop.txt"), "# 自定义\nrust\n").unwrap();
        let text = "[tokenizer]\nstopwords_file = \"my_stop.txt\"\n";
        let cfg_path = dir.join(CONFIG_FILE);
        fs::write(&cfg_path, text).unwrap();
        let loaded = NebulaConfig::load_dir(&dir).unwrap();
        assert!(loaded.stopwords.contains("rust"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let bad_page = NebulaConfig::from_toml("[storage]\npage_size = 1000\n").unwrap();
        let loaded = LoadedConfig {
            config: bad_page,
            dir: PathBuf::from("."),
            stopwords: HashSet::new(),
        };
        assert!(loaded.validate().is_err());

        let bad_addr = NebulaConfig::from_toml("[server]\ndefault_addr = \"not-an-addr\"\n").unwrap();
        let loaded = LoadedConfig {
            config: bad_addr,
            dir: PathBuf::from("."),
            stopwords: HashSet::new(),
        };
        assert!(loaded.validate().is_err());
    }

    #[test]
    fn stopwords_path_cannot_escape_dir() {
        for bad in ["../secret.txt", "/etc/passwd", "sub/dir.txt", ""] {
            let cfg = NebulaConfig::from_toml(&format!(
                "[tokenizer]\nstopwords_file = \"{bad}\"\n"
            ))
            .unwrap();
            let loaded = LoadedConfig {
                config: cfg,
                dir: PathBuf::from("."),
                stopwords: HashSet::new(),
            };
            assert!(loaded.validate().is_err(), "should reject '{bad}'");
        }
    }
}
