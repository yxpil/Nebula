//! TCP 客户端:连接、挑战应答认证、加密帧收发。
//!
//! 客户端只需要数据库地址与密码;库文件的盐由服务端在 hello 中带回。

use std::net::TcpStream;

use nebula_core::Result;
use nebula_crypto::{derive_master_key, KEY_LEN, SALT_LEN};

use crate::protocol::{
    auth_proof, read_frame, session_key, write_frame, CHALLENGE_LEN, Direction, HELLO_LEN,
    PROTOCOL_MAGIC, Request, Response, STATUS_OK,
};

/// 已认证的 TCP 会话。
pub struct Client {
    stream: TcpStream,
    session_key: [u8; KEY_LEN],
    send_seq: u64,
    recv_seq: u64,
}

impl Client {
    /// 连接服务端并用密码完成挑战应答认证。
    pub fn connect(addr: &str, password: &str) -> Result<Self> {
        let mut stream = TcpStream::connect(addr)?;
        let mut hello = vec![0u8; HELLO_LEN];
        std::io::Read::read_exact(&mut stream, &mut hello)?;
        if &hello[..PROTOCOL_MAGIC.len()] != PROTOCOL_MAGIC {
            return Err(nebula_core::Error::Protocol(
                "server is not a nebula server (bad magic)".into(),
            ));
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&hello[PROTOCOL_MAGIC.len()..PROTOCOL_MAGIC.len() + SALT_LEN]);
        let mut challenge = [0u8; CHALLENGE_LEN];
        challenge.copy_from_slice(&hello[PROTOCOL_MAGIC.len() + SALT_LEN..HELLO_LEN]);

        // 客户端本地派生主密钥与会话密钥,proof 不明文上网。
        let master = derive_master_key(password, &salt);
        let session_key = session_key(&master, &challenge);
        let proof = auth_proof(&session_key, &challenge);
        use std::io::Write;
        stream.write_all(&proof)?;
        stream.flush()?;

        let mut status = [0u8; 1];
        std::io::Read::read_exact(&mut stream, &mut status)?;
        if status[0] != STATUS_OK {
            return Err(nebula_core::Error::Auth(
                "server rejected the password".into(),
            ));
        }
        Ok(Client {
            stream,
            session_key,
            send_seq: 0,
            recv_seq: 0,
        })
    }

    /// 对端地址(展示用)。
    pub fn peer_addr(&self) -> String {
        self.stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".into())
    }

    /// 发送请求并等待响应。
    pub fn call(&mut self, req: &Request) -> Result<Response> {
        let json = serde_json::to_vec(req)
            .map_err(|e| nebula_core::Error::Protocol(format!("serialize request: {e}")))?;
        write_frame(
            &mut self.stream,
            &self.session_key,
            Direction::Up,
            self.send_seq,
            &json,
        )?;
        self.send_seq += 1;
        let payload = read_frame(
            &mut self.stream,
            &self.session_key,
            Direction::Down,
            self.recv_seq,
        )?;
        self.recv_seq += 1;
        let resp: Response = serde_json::from_slice(&payload)
            .map_err(|e| nebula_core::Error::Protocol(format!("parse response: {e}")))?;
        Ok(resp)
    }

    /// 执行一条 SQL。服务端逻辑错误以 `Ok(ok=false)` 返回,传输/协议错误才为 `Err`。
    pub fn sql(&mut self, sql: &str) -> Result<Response> {
        self.call(&Request::Sql { sql: sql.to_string() })
    }

    /// 连通性探测。
    pub fn ping(&mut self) -> Result<Response> {
        self.call(&Request::Ping)
    }

    /// 通知服务端关闭会话(服务端随后断开)。
    pub fn close(&mut self) -> Result<Response> {
        self.call(&Request::Close)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("peer", &self.peer_addr())
            .field("send_seq", &self.send_seq)
            .field("recv_seq", &self.recv_seq)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{load_salt, serve_until};
    use nebula_crypto::derive_master_key;
    use nebula_engine::Database;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nebula_cli_{}_{}.ndb", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    const PW: &str = "client-test-password";

    #[test]
    fn client_connect_sql_and_close() {
        let path = tmp("basic");
        {
            let mut db = Database::create(&path, PW, 4096).unwrap();
            db.close().unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let db = Database::open(&path, PW).unwrap();
        let salt = load_salt(&path).unwrap();
        let master = derive_master_key(PW, &salt);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let _ = serve_until(listener, salt, master, Arc::new(Mutex::new(db)), stop2);
        });

        let mut c = Client::connect(&addr, PW).unwrap();
        assert!(c.ping().unwrap().ok);
        let resp = c
            .sql("INSERT INTO memories (content, tags) VALUES ('Rust 内存安全 borrow checker', 'rust, lang')")
            .unwrap();
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.affected, 1);
        let resp = c
            .sql("SELECT id, content, keywords FROM memories WHERE keyword = 'rust'")
            .unwrap();
        assert!(resp.ok, "{resp:?}");
        let cols = resp.columns.unwrap();
        assert_eq!(cols, vec!["id", "content", "keywords"]);
        assert_eq!(resp.rows.as_ref().unwrap().len(), 1);
        assert!(resp.rows.as_ref().unwrap()[0][1].contains("borrow checker"));
        assert!(c.close().unwrap().ok);

        drop(c);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
