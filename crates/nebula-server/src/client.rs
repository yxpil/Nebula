//! TCP 客户端:v2 连接、身份声明 + 挑战应答认证、加密帧收发。
//!
//! 客户端需要服务地址、用户名与密码;用户盐由服务端在 challenge 帧中带回。

use std::net::TcpStream;

use nebula_core::Result;
use nebula_crypto::{derive_master_key, KEY_LEN, SALT_LEN};

use crate::protocol::{
    auth_proof, read_frame, session_key, write_frame, write_identity, CHALLENGE_FRAME_LEN,
    CHALLENGE_LEN, Direction, PROTOCOL_MAGIC, READY_BYTE, Request, Response, STATUS_OK,
};

/// 已认证的 TCP 会话。
pub struct Client {
    stream: TcpStream,
    session_key: [u8; KEY_LEN],
    send_seq: u64,
    recv_seq: u64,
}

impl Client {
    /// 连接服务端,以内置 admin 身份完成认证。
    pub fn connect(addr: &str, password: &str) -> Result<Self> {
        Self::connect_as(addr, "admin", password)
    }

    /// 连接服务端,以指定用户身份完成 v2 认证。
    pub fn connect_as(addr: &str, user: &str, password: &str) -> Result<Self> {
        let mut stream = TcpStream::connect(addr)?;

        // 1. 发 hello(魔数),读 ready
        use std::io::{Read, Write};
        stream.write_all(PROTOCOL_MAGIC)?;
        stream.flush()?;
        let mut ready = [0u8; 1];
        stream.read_exact(&mut ready)?;
        if ready[0] != READY_BYTE {
            return Err(nebula_core::Error::Protocol(
                "server rejected the protocol version".into(),
            ));
        }

        // 2. 声明用户名
        write_identity(&mut stream, user)?;

        // 3. 读 challenge 帧:用户盐 + 挑战
        let mut frame = vec![0u8; CHALLENGE_FRAME_LEN];
        stream.read_exact(&mut frame)?;
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&frame[..SALT_LEN]);
        let mut challenge = [0u8; CHALLENGE_LEN];
        challenge.copy_from_slice(&frame[SALT_LEN..CHALLENGE_FRAME_LEN]);

        // 4. 本地派生主密钥/会话密钥,proof 不明文上网
        let master = derive_master_key(password, &salt);
        let session_key = session_key(&master, &challenge);
        let proof = auth_proof(&session_key, &challenge);
        stream.write_all(&proof)?;
        stream.flush()?;

        // 5. 读认证状态
        let mut status = [0u8; 1];
        stream.read_exact(&mut status)?;
        if status[0] != STATUS_OK {
            return Err(nebula_core::Error::Auth(
                "server rejected the user name or password".into(),
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
        self.call(&Request::Sql {
            sql: sql.to_string(),
        })
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
    use crate::server::{serve_until, ConnBackend};
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
        let salt = crate::server::load_salt(&path).unwrap();
        let master = derive_master_key(PW, &salt);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let backend = ConnBackend::Single {
            db: Arc::new(Mutex::new(db)),
            salt,
            master,
        };
        let server = std::thread::spawn(move || {
            let _ = serve_until(listener, backend, stop2);
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
        assert_eq!(
            resp.columns.as_ref().unwrap(),
            &vec!["id".to_string(), "content".to_string(), "keywords".to_string()]
        );
        assert_eq!(resp.rows.as_ref().unwrap().len(), 1);
        assert!(resp.rows.as_ref().unwrap()[0][1].contains("borrow checker"));
        assert!(c.close().unwrap().ok);

        drop(c);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
