//! Nebula 命令行客户端。
//!
//! 子命令:
//! - `create --db <path>`        创建新库(创建后进入本地 REPL)
//! - `open --db <path>`          打开已有库,进入本地 REPL
//! - `serve --db <path>`         以 TCP 服务端方式打开库(阻塞运行)
//! - `connect --addr host:port`  连接远程服务端,进入远程 REPL
//!
//! 配置:每个库旁边自动生成 `<库文件>.conf.d/` 目录(nebula.toml + stopwords.txt),
//! 所有行为参数(页大小、检查点阈值、上限、分词、停用词、默认地址、CLI 展示、
//! 密码策略)都在其中;`--config <dir>` 可显式指定其它配置目录。
//!
//! 密码输入顺序:`NEBULA_PASSWORD` 环境变量 → 终端隐藏输入(create 需二次确认)。

mod render;
mod repl;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nebula_config::{
    config_dir_for_db, LoadedConfig, NebulaConfig, CONFIG_FILE,
};
use nebula_core::{Error, Result};
use nebula_engine::Database;
use nebula_server::{Client, Server};

/// connect 模式在未指定 --config 时使用的默认配置目录(相对于当前工作目录)。
const CONNECT_CONFIG_DIR: &str = "nebula.conf.d";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => cmd_create(&args[1..]),
        Some("open") => cmd_open(&args[1..]),
        Some("serve") => cmd_serve(&args[1..]),
        Some("connect") => cmd_connect(&args[1..]),
        _ => {
            print_help();
            if args.is_empty() {
                Ok(())
            } else {
                Err(Error::Sql(format!("unknown command '{}'", args[0])))
            }
        }
    }
}

fn print_help() {
    println!(
        "nebula {version} — 加密记忆数据库

用法:
  nebula create  --db <path> [--addr host:port]   创建新库(带 --addr 则直接起服务端)
  nebula open    --db <path>                      打开本地库并进入交互界面
  nebula serve   --db <path> [--addr host:port]   启动 TCP 服务端
  nebula connect --addr <host:port>               连接远程服务端

选项:
  --config <dir>    指定配置目录(默认 <库文件>.conf.d;connect 默认为 ./{CONNECT_CONFIG_DIR})
  --page-size <n>   建库页大小(覆盖配置,4096..65536,须为 2 的幂)
  -h, --help        显示本帮助

配置:
  create 时自动生成 nebula.toml(页大小/检查点/上限/分词/停用词/默认地址/展示/密码策略)
  与 stopwords.txt(停用词表),手工编辑后对下一次 open/serve 生效。

其他:
  NEBULA_PASSWORD   环境变量提供密码(跳过终端输入,便于脚本)",
        version = env!("CARGO_PKG_VERSION")
    );
}

// ---------------- 子命令 ----------------

/// 解析 `host:port`,统一错误类型。
fn parse_addr(text: &str) -> Result<SocketAddr> {
    text.parse::<SocketAddr>()
        .map_err(|_| Error::Protocol(format!("invalid address '{text}' (want host:port)")))
}

/// 确定配置目录:--config 优先;有库路径时取库旁目录;connect 回退到当前目录。
fn resolve_config_dir(opts: &Opts, db_path: Option<&Path>) -> PathBuf {
    if let Some(dir) = &opts.config {
        return dir.clone();
    }
    match db_path {
        Some(p) => config_dir_for_db(p),
        None => PathBuf::from(CONNECT_CONFIG_DIR),
    }
}

/// 加载配置;目录不存在时生成默认配置并提示。
fn load_or_create_config(dir: &Path) -> Result<LoadedConfig> {
    if !dir.join(CONFIG_FILE).is_file() {
        NebulaConfig::write_default_dir(dir)?;
        eprintln!(
            "note: created default config at {} (edit to tune)",
            dir.display()
        );
    }
    nebula_config::NebulaConfig::load_dir(dir)
}

fn cmd_create(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let path = opts.db_path("create")?;
    if path.exists() {
        return Err(Error::Storage(format!(
            "file already exists: {}",
            path.display()
        )));
    }
    let cfg = load_or_create_config(&resolve_config_dir(&opts, Some(&path)))?;
    let password = prompt_new_password(cfg.config.auth.password_min_len)?;
    // 命令行 --page-size 覆盖配置;配置值在建库时固化进库文件头
    let page_size = opts
        .page_size
        .unwrap_or(cfg.config.storage.page_size);
    if let Some(addr) = &opts.addr {
        // 直接以服务端方式启动新库
        let server = Server::create_configured(
            parse_addr(addr)?,
            &path,
            &password,
            page_size,
            &cfg.config.engine,
            &cfg.config.tokenizer.extract,
            &cfg.stopwords,
        )?;
        println!("created database {}", path.display());
        return server.run();
    }
    let db = Database::create_configured(
        &path,
        &password,
        page_size,
        &cfg.config.engine,
        &cfg.config.tokenizer.extract,
        &cfg.stopwords,
    )?;
    println!("created database {} (page size {page_size})", path.display());
    repl::run_local(db, &cfg)
}

fn cmd_open(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let path = opts.db_path("open")?;
    let cfg = load_or_create_config(&resolve_config_dir(&opts, Some(&path)))?;
    let password = prompt_password()?;
    let db = Database::open_configured(
        &path,
        &password,
        &cfg.config.engine,
        &cfg.config.tokenizer.extract,
        &cfg.stopwords,
    )?;
    repl::run_local(db, &cfg)
}

fn cmd_serve(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let path = opts.db_path("serve")?;
    let cfg = load_or_create_config(&resolve_config_dir(&opts, Some(&path)))?;
    let password = prompt_password()?;
    let addr = opts
        .addr
        .clone()
        .unwrap_or_else(|| cfg.config.server.default_addr.clone());
    let server = Server::open_configured(
        parse_addr(&addr)?,
        &path,
        &password,
        &cfg.config.engine,
        &cfg.config.tokenizer.extract,
        &cfg.stopwords,
    )?;
    println!("opened database {} (listening on {addr})", path.display());
    server.run()
}

fn cmd_connect(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let cfg = load_or_create_config(&resolve_config_dir(&opts, None))?;
    let addr = opts
        .addr
        .clone()
        .unwrap_or_else(|| cfg.config.server.default_addr.clone());
    // 先解析校验地址格式,避免密码白输
    parse_addr(&addr)?;
    let password = prompt_password()?;
    let client = Client::connect(&addr, &password)?;
    repl::run_remote(client, &cfg)
}

// ---------------- 参数与密码 ----------------

/// 解析后的命令行选项。
struct Opts {
    db: Option<PathBuf>,
    addr: Option<String>,
    page_size: Option<u32>,
    config: Option<PathBuf>,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Self> {
        let mut opts = Opts {
            db: None,
            addr: None,
            page_size: None,
            config: None,
        };
        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();
            let value = args.get(i + 1).map(String::as_str);
            match key {
                "--db" | "-d" => opts.db = Some(PathBuf::from(take_value(key, value)?)),
                "--addr" | "-a" => opts.addr = Some(take_value(key, value)?.to_string()),
                "--config" | "-c" => opts.config = Some(PathBuf::from(take_value(key, value)?)),
                "--page-size" => {
                    let v = take_value(key, value)?;
                    opts.page_size = Some(v.parse::<u32>().map_err(|_| {
                        Error::Sql(format!("invalid --page-size '{v}'"))
                    })?);
                }
                other => {
                    return Err(Error::Sql(format!(
                        "unknown option '{other}' (try -h)"
                    )))
                }
            }
            i += 2;
        }
        Ok(opts)
    }

    fn db_path(&self, cmd: &str) -> Result<PathBuf> {
        self.db
            .clone()
            .ok_or_else(|| Error::Sql(format!("`{cmd}` requires --db <path>")))
    }
}

fn take_value<'a>(key: &str, value: Option<&'a str>) -> Result<&'a str> {
    value.ok_or_else(|| Error::Sql(format!("option '{key}' requires a value")))
}

/// 密码来源:环境变量优先(测试/脚本),否则终端隐藏输入。
fn env_password() -> Option<String> {
    std::env::var("NEBULA_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
}

/// 提示输入密码(无回显)。
fn prompt_password() -> Result<String> {
    if let Some(pw) = env_password() {
        return Ok(pw);
    }
    let pw = rpassword::prompt_password("password: ")?;
    if pw.is_empty() {
        return Err(Error::Auth("password must not be empty".into()));
    }
    Ok(pw)
}

/// 提示输入新密码并二次确认(最小长度来自配置)。
fn prompt_new_password(min_len: usize) -> Result<String> {
    if let Some(pw) = env_password() {
        if pw.chars().count() < min_len {
            return Err(Error::Auth(format!(
                "NEBULA_PASSWORD must be at least {min_len} characters"
            )));
        }
        return Ok(pw);
    }
    let pw = rpassword::prompt_password(format!("new password (at least {min_len} chars): "))?;
    if pw.chars().count() < min_len {
        return Err(Error::Auth(format!(
            "password must be at least {min_len} characters"
        )));
    }
    let again = rpassword::prompt_password("confirm password: ")?;
    if pw != again {
        return Err(Error::Auth("passwords do not match".into()));
    }
    Ok(pw)
}
