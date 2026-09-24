//! Nebula TCP 服务端与客户端协议实现。
//!
//! - [`server`]:线程池式服务端,v2 身份声明 + 挑战应答认证 + 全会话加密
//! - [`client`]:CLI 与程序化调用复用的加密客户端
//! - [`protocol`]:线上帧格式、握手与消息序列化定义

pub mod client;
pub mod protocol;
pub mod server;

pub use client::Client;
pub use protocol::{Request, Response};
pub use server::{load_master_key, load_salt, serve_until, Server};
