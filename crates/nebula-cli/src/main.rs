//! Nebula 命令行客户端。
//!
//! 子命令:
//! - `create --db <path>`        创建新库(创建后进入本地 REPL)
//! - `open --db <path>`          打开已有库,进入本地 REPL
//! - `serve --db <path>`         以 TCP 服务端方式打开库(阻塞运行)
//! - `connect --addr host:port`  连接远程服务端,进入远程 REPL
//!
//! 密码输入顺序:`NEBULA_PASSWORD` 环境变量 → 终端隐藏输入(create 需二次确认)。

mod render;
mod repl;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use nebula_core::{Error, Result};
use nebula_engine::Database;
use nebula_server::{Client, Server};

/// 默认监听/连接地址。
const DEFAULT_ADDR: &str = "127.0.0.1:7777";
/// 默认页大小。
const DEFAULT_PAGE_SIZE: u32 = 4096;

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
  nebula serve   --db <path> [--addr host:port]   启动 TCP 服务端(默认 {DEFAULT_ADDR})
  nebula connect --addr <host:port>               连接远程服务端

其他:
  --page-size <n>   建库页大小(4096..65536,须为 2 的幂)
  -h, --help        显示本帮助
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

fn cmd_create(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let path = opts.db_path("create")?;
    if path.exists() {
        return Err(Error::Storage(format!(
            "file already exists: {}",
            path.display()
        )));
    }
    let password = prompt_new_password()?;
    let page_size = opts.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
    if let Some(addr) = &opts.addr {
        // 直接以服务端方式启动新库
        let server = Server::create(parse_addr(addr)?, &path, &password, page_size)?;
        println!("created database {}", path.display());
        return server.run();
    }
    let db = Database::create(&path, &password, page_size)?;
    println!("created database {} (page size {page_size})", path.display());
    repl::run_local(db)
}

fn cmd_open(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let path = opts.db_path("open")?;
    let password = prompt_password()?;
    let db = Database::open(&path, &password)?;
    repl::run_local(db)
}

fn cmd_serve(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let path = opts.db_path("serve")?;
    let password = prompt_password()?;
    let addr = opts.addr.unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let server = Server::open(parse_addr(&addr)?, &path, &password)?;
    println!("opened database {}", path.display());
    server.run()
}

fn cmd_connect(args: &[String]) -> Result<()> {
    let opts = Opts::parse(args)?;
    let addr = opts.addr.unwrap_or_else(|| DEFAULT_ADDR.to_string());
    // 先解析校验地址格式,避免密码白输
    parse_addr(&addr)?;
    let password = prompt_password()?;
    let client = Client::connect(&addr, &password)?;
    repl::run_remote(client)
}

// ---------------- 参数与密码 ----------------

/// 解析后的命令行选项。
struct Opts {
    db: Option<PathBuf>,
    addr: Option<String>,
    page_size: Option<u32>,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Self> {
        let mut opts = Opts {
            db: None,
            addr: None,
            page_size: None,
        };
        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();
            let value = args.get(i + 1).map(String::as_str);
            match key {
                "--db" | "-d" => opts.db = Some(PathBuf::from(take_value(key, value)?)),
                "--addr" | "-a" => opts.addr = Some(take_value(key, value)?.to_string()),
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

/// 提示输入新密码并二次确认。
fn prompt_new_password() -> Result<String> {
    if let Some(pw) = env_password() {
        if pw.len() < 8 {
            return Err(Error::Auth(
                "NEBULA_PASSWORD must be at least 8 characters".into(),
            ));
        }
        return Ok(pw);
    }
    let pw = rpassword::prompt_password("new password (at least 8 chars): ")?;
    if pw.len() < 8 {
        return Err(Error::Auth("password must be at least 8 characters".into()));
    }
    let again = rpassword::prompt_password("confirm password: ")?;
    if pw != again {
        return Err(Error::Auth("passwords do not match".into()));
    }
    Ok(pw)
}
