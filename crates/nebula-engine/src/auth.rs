//! 会话、权限模型与用户目录接口。
//!
//! Nebula 的多用户是**应用层用户表**(用户记录存在集群管理文件中,见
//! nebula-cluster),按**逻辑库**授权:
//! - [`Privilege::Read`]:SELECT / SEARCH / RELATED / SHOW 等读路径;
//! - [`Privilege::Write`]:INSERT / UPDATE / DELETE;
//! - [`Privilege::Admin`]:建删库、挂接、用户与授权管理(全局,`ON *`)。
//!
//! 单文件模式没有用户表:只有内置管理员 [`DEFAULT_USER`],全权访问
//! ([`FullAccess`]);协议层以文件密码认证该账号。

pub use nebula_sql::ast::Privilege;

use nebula_core::{DEFAULT_DB, MemoryId, MemoryRecord, Result};
use nebula_sql::ast::GrantObject;

/// 内置管理员用户名(单文件模式唯一账号)。
pub const DEFAULT_USER: &str = "admin";

/// 单条写操作的撤销动作(undo log)。
#[derive(Debug, Clone)]
pub enum UndoAction {
    /// 插入了 (db, id):撤销即删除。
    Inserted { db: String, id: MemoryId },
    /// 从 db 删除了这些完整记录:撤销即原样写回。
    Deleted {
        db: String,
        records: Vec<MemoryRecord>,
    },
    /// db 中一条记录被替换;撤销即恢复旧记录。
    Replaced {
        db: String,
        old: MemoryRecord,
    },
}

/// 一个活动事务:按执行顺序积累的 undo 动作。
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    undos: Vec<UndoAction>,
}

impl Transaction {
    /// 追加一条 undo。
    pub fn record(&mut self, action: UndoAction) {
        self.undos.push(action);
    }

    /// 是否没有任何写操作(只读事务)。
    pub fn is_empty(&self) -> bool {
        self.undos.is_empty()
    }

    /// 取出全部 undo(反向遍历应用即回滚)。
    pub fn into_undos(self) -> Vec<UndoAction> {
        self.undos
    }
}

/// 一个连接/REPL 会话:当前用户、当前工作库与可选的活动事务。
#[derive(Debug, Clone)]
pub struct Session {
    user: String,
    current_db: String,
    txn: Option<Transaction>,
}

impl Session {
    pub fn new(user: impl Into<String>, current_db: impl Into<String>) -> Self {
        Session {
            user: user.into(),
            current_db: current_db.into(),
            txn: None,
        }
    }

    /// 内置管理员会话(单文件模式默认)。
    pub fn admin() -> Self {
        Session {
            user: DEFAULT_USER.into(),
            current_db: DEFAULT_DB.into(),
            txn: None,
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn current_db(&self) -> &str {
        &self.current_db
    }

    /// 切换工作库(USE 成功后调用)。
    pub fn set_current_db(&mut self, db: impl Into<String>) {
        self.current_db = db.into();
    }

    // ------- 事务状态 -------

    /// 是否有活动事务。
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// 开启事务;已有活动事务时报错(不支持嵌套)。
    pub fn begin_txn(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(nebula_core::Error::Sql(
                "a transaction is already active (nested transactions are not supported)".into(),
            ));
        }
        self.txn = Some(Transaction::default());
        Ok(())
    }

    /// 记录一条 undo(必须在活动事务中调用)。
    pub fn record_undo(&mut self, action: UndoAction) -> Result<()> {
        match self.txn.as_mut() {
            Some(txn) => {
                txn.record(action);
                Ok(())
            }
            None => Err(nebula_core::Error::Engine(
                "internal: record_undo without an active transaction".into(),
            )),
        }
    }

    /// 取出并清除活动事务(COMMIT / ROLLACK 都用它)。
    pub fn take_txn(&mut self) -> Option<Transaction> {
        self.txn.take()
    }
}

/// 用户目录:用户存在性、按库权限查询,以及用户/授权的管理操作。
///
/// 读方法在执行每条语句前做权限闸门;管理方法由 executor 在
/// CREATE/DROP/ALTER USER、GRANT、REVOKE 时调用,由 nebula-cluster
/// 持久化实现。
pub trait UserDirectory {
    /// 用户是否存在。
    fn user_exists(&self, user: &str) -> bool;

    /// 用户对指定库是否拥有某权限。
    fn has_priv(&self, user: &str, db: &str, priv_: Privilege) -> bool;

    /// 是否全局管理员(拥有 `Admin ON *`)。
    fn is_admin(&self, user: &str) -> bool;

    /// 创建用户;用户已存在时返回 false(if_not_exists 为 true 时不报错)。
    fn create_user(
        &mut self,
        user: &str,
        password: &str,
        if_not_exists: bool,
    ) -> Result<bool>;

    /// 删除用户;用户不存在时返回 false(if_exists 为 true 时不报错)。
    fn drop_user(&mut self, user: &str, if_exists: bool) -> Result<bool>;

    /// 修改用户密码(用户必须存在)。
    fn alter_user(&mut self, user: &str, password: &str) -> Result<()>;

    /// 追加授权(重复授权幂等)。
    fn grant(&mut self, user: &str, object: &GrantObject, privs: &[Privilege]) -> Result<()>;

    /// 收回授权(未持有的权限视为已收回,幂等)。
    fn revoke(&mut self, user: &str, object: &GrantObject, privs: &[Privilege]) -> Result<()>;

    /// 全部用户名(按字典序)。
    fn list_users(&self) -> Vec<String>;

    /// 用户的全部授权 `(对象, 权限)`(按对象稳定排序)。
    fn list_grants(&self, user: &str) -> Vec<(GrantObject, Vec<Privilege>)>;
}

/// 单文件模式的全权目录:只认内置管理员 admin,任何库上拥有全部权限。
pub struct FullAccess;

impl UserDirectory for FullAccess {
    fn user_exists(&self, user: &str) -> bool {
        user == DEFAULT_USER
    }

    fn has_priv(&self, user: &str, _db: &str, _priv_: Privilege) -> bool {
        user == DEFAULT_USER
    }

    fn is_admin(&self, user: &str) -> bool {
        user == DEFAULT_USER
    }

    fn create_user(
        &mut self,
        _user: &str,
        _password: &str,
        _if_not_exists: bool,
    ) -> Result<bool> {
        Err(nebula_core::Error::Engine(
            "user management requires directory mode (open a cluster directory with --dir)"
                .into(),
        ))
    }

    fn drop_user(&mut self, _user: &str, _if_exists: bool) -> Result<bool> {
        Err(nebula_core::Error::Engine(
            "user management requires directory mode (--dir)".into(),
        ))
    }

    fn alter_user(&mut self, _user: &str, _password: &str) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "user management requires directory mode (--dir)".into(),
        ))
    }

    fn grant(
        &mut self,
        _user: &str,
        _object: &GrantObject,
        _privs: &[Privilege],
    ) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "GRANT requires directory mode (--dir)".into(),
        ))
    }

    fn revoke(
        &mut self,
        _user: &str,
        _object: &GrantObject,
        _privs: &[Privilege],
    ) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "REVOKE requires directory mode (--dir)".into(),
        ))
    }

    fn list_users(&self) -> Vec<String> {
        vec![DEFAULT_USER.into()]
    }

    fn list_grants(&self, user: &str) -> Vec<(GrantObject, Vec<Privilege>)> {
        if user != DEFAULT_USER {
            return Vec::new();
        }
        vec![(
            GrantObject::AllDatabases,
            vec![Privilege::Read, Privilege::Write, Privilege::Admin],
        )]
    }
}

/// 要求用户对某库拥有某权限,否则返回权限拒绝错误。
pub fn require_priv(
    dir: &dyn UserDirectory,
    user: &str,
    db: &str,
    priv_: Privilege,
) -> Result<()> {
    if dir.has_priv(user, db, priv_) {
        Ok(())
    } else {
        Err(nebula_core::Error::Auth(format!(
            "permission denied: user '{user}' lacks {priv_:?} on database '{db}'"
        )))
    }
}

/// 要求全局管理员(Admin ON *),否则返回权限拒绝错误。
pub fn require_admin(dir: &dyn UserDirectory, user: &str) -> Result<()> {
    if dir.is_admin(user) {
        Ok(())
    } else {
        Err(nebula_core::Error::Auth(format!(
            "permission denied: user '{user}' is not an administrator (Admin ON * required)"
        )))
    }
}
