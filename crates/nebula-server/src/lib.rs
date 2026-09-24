//! Nebula TCP 服务端与客户端协议实现。
//!
//! - [`server`]:线程池式服务端,挑战应答密码认证 + 全会话 ChaCha20-Poly1305 加密
//! - [`client`]:CLI 与程序化调用复用的加密客户端
//! - [`protocol`]:线上帧格式、握手与消息序列化定义

pub mod client;
pub mod protocol;
pub mod server;

pub use client::Client;
pub use protocol::{Request, Response};
pub use server::{load_master_key, load_salt, serve, serve_until, Server};
