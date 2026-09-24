//! Nebula 目录集群:一个目录下挂多个 `.ndb` 文件,统一逻辑库命名空间,
//! 并持久化应用层用户表与按库授权。
//!
//! 目录布局:
//! ```text
//! <dir>/
//!   main.ndb           # 默认文件,main 库所在
//!   <db>.ndb           # CREATE DATABASE 创建的受管文件(一库一文件)
//!   _admin.ndb         # 用户(Argon2id 验证器)与授权状态
//!   任意外部.ndb        # ATTACH FILE 挂接,会话结束可 DETACH
//! ```
//!
//! Cluster 同时实现 [`MemBackend`] 与 [`UserDirectory`]:
//! executor / server 把它当后端用;权限数据经 `_admin.ndb` 加密持久化,
//! 与数据文件使用同一集群密码。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use nebula_core::codec::Writer;
use nebula_core::{MemoryId, MemoryRecord, Result};
use nebula_crypto::{derive_master_key, random_salt, SALT_LEN};
use nebula_engine::{
    normalize_db_name, MemBackend, Privilege, RankedRow, Session, UserDirectory,
};
use nebula_sql::ast::{CacheTarget, GrantObject, RelatedSeed};
use nebula_tokenizer::{default_stopword_set, ExtractorConfig};

/// 管理状态快照魔数(原样前缀,存于 _admin.ndb 的快照字节)。
const ADMIN_MAGIC: &[u8] = b"NADM1";
/// 管理文件名(下划线开头,目录扫描时自动排除)。
const ADMIN_FILE: &str = "_admin.ndb";
/// 默认文件名与默认库。
const MAIN_FILE: &str = "main.ndb";
const MAIN_DB: &str = "main";
/// 默认管理员(初始密码 = 集群密码)。
const ADMIN_USER: &str = "admin";

/// 用户记录:密码验证器与盐(verifier = Argon2id(password, salt))。
#[derive(Debug, Clone)]
struct UserRec {
    name: String,
    salt: Vec<u8>,
    verifier: Vec<u8>,
}

/// 一条授权记录。
#[derive(Debug, Clone)]
struct GrantRec {
    object: GrantObject,
    privs: BTreeSet<Privilege>,
}

/// 文件来源:决定 DROP/DETACH 时物理文件如何处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOrigin {
    /// 默认 main.ndb。
    Main,
    /// CREATE DATABASE 产生,受管,DROP 即删文件。
    Managed,
    /// ATTACH 挂接的外部文件,DETACH 只关不删。
    Attached,
}

/// 一个打开的 .ndb 文件槽(槽位 index 稳定;关闭后 db 为 None 墓碑)。
struct FileSlot {
    db: Option<nebula_engine::Database>,
    origin: FileOrigin,
    path: PathBuf,
}

/// 挂载点:别名 → (文件槽, 文件内真实库名)。
#[derive(Debug, Clone)]
struct Mount {
    file: usize,
    real: String,
}

pub struct Cluster {
    dir: PathBuf,
    slots: Vec<FileSlot>,
    mounts: BTreeMap<String, Mount>,
    /// 管理状态存储文件(始终打开;快照槽直接存 NADM 字节)。
    admin_file: nebula_storage::MemoryFile,
    users: BTreeMap<String, UserRec>,
    grants: BTreeMap<String, Vec<GrantRec>>,
    cfg: nebula_engine::EngineConfig,
    extractor_cfg: ExtractorConfig,
    stopwords: HashSet<String>,
    /// 缓存集群密码:会话中新建/挂接文件需要(主密钥本身不缓存)。
    password: String,
}

impl Cluster {
    /// 打开或初始化目录集群(引擎默认配置 + 内置停用词表)。
    pub fn open_or_create(dir: impl AsRef<Path>, password: &str) -> Result<Self> {
        Self::configured(
            dir,
            password,
            &nebula_engine::EngineConfig::default(),
            &ExtractorConfig::default(),
            &default_stopword_set(),
        )
    }

    /// 打开或初始化,注入引擎配置/提取配置/停用词表。
    pub fn configured(
        dir: impl AsRef<Path>,
        password: &str,
        cfg: &nebula_engine::EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;

        // _admin.ndb:不存在则创建(底层文件;快照槽存 NADM 管理字节,
        // 不经过 Database,避免与其索引快照互相覆盖)
        let admin_path = dir.join(ADMIN_FILE);
        let mut admin_file = if admin_path.exists() {
            nebula_storage::MemoryFile::open(&admin_path, password)?
        } else {
            nebula_storage::MemoryFile::create(
                &admin_path,
                password,
                nebula_storage::DEFAULT_PAGE_SIZE,
            )?
        };
        let bytes = admin_file.read_index_snapshot().unwrap_or_default();
        let (users, grants) = decode_admin(&bytes)?;

        let mut cluster = Cluster {
            dir: dir.clone(),
            slots: Vec::new(),
            mounts: BTreeMap::new(),
            admin_file,
            users,
            grants,
            cfg: cfg.clone(),
            extractor_cfg: extractor_cfg.clone(),
            stopwords: stopwords.clone(),
            password: password.to_string(),
        };

        // main.ndb:不存在则创建(自带 main 库)
        let main_path = cluster.dir.join(MAIN_FILE);
        let main_idx = if main_path.exists() {
            cluster.open_file(&main_path, FileOrigin::Main)?
        } else {
            cluster.create_file(&main_path, FileOrigin::Main, MAIN_DB)?
        };
        debug_assert_eq!(main_idx, 0);

        // 扫描目录:自动挂接其余 .ndb 文件
        let mut extra: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ndb") {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == MAIN_FILE || name.starts_with('_') {
                continue;
            }
            extra.push(path);
        }
        extra.sort();
        for path in extra {
            cluster.open_file(&path, FileOrigin::Attached)?;
        }

        // 首次初始化:建立默认管理员(密码 = 集群密码)
        if !cluster.users.contains_key(ADMIN_USER) {
            let salt = random_salt();
            let verifier = derive_master_key(password, &salt);
            cluster.users.insert(
                ADMIN_USER.into(),
                UserRec {
                    name: ADMIN_USER.into(),
                    salt: salt.to_vec(),
                    verifier: verifier.to_vec(),
                },
            );
            cluster.grants.insert(
                ADMIN_USER.into(),
                vec![GrantRec {
                    object: GrantObject::AllDatabases,
                    privs: [Privilege::Read, Privilege::Write, Privilege::Admin]
                        .into_iter()
                        .collect(),
                }],
            );
            cluster.save_admin()?;
        }
        Ok(cluster)
    }

    /// 打开已有文件并注册其全部内部库为挂载(别名 = 真实库名)。
    fn open_file(&mut self, path: &Path, origin: FileOrigin) -> Result<usize> {
        let db = nebula_engine::Database::open_configured(
            path,
            &self.password,
            &self.cfg,
            &self.extractor_cfg,
            &self.stopwords,
        )?;
        let names = db.list_dbs();
        let idx = self.slots.len();
        self.slots.push(FileSlot {
            db: Some(db),
            origin,
            path: path.to_path_buf(),
        });
        for name in names {
            if self.mounts.contains_key(&name) {
                return Err(nebula_core::Error::Engine(format!(
                    "database name conflict: '{name}' appears in multiple files ({})",
                    path.display()
                )));
            }
            self.mounts.insert(
                name.clone(),
                Mount {
                    file: idx,
                    real: name,
                },
            );
        }
        Ok(idx)
    }

    /// 创建只含单个库的受管文件并注册挂载。
    fn create_file(
        &mut self,
        path: &Path,
        origin: FileOrigin,
        db_name: &str,
    ) -> Result<usize> {
        let db = if origin == FileOrigin::Main {
            nebula_engine::Database::create_configured(
                path,
                &self.password,
                nebula_storage::DEFAULT_PAGE_SIZE,
                &self.cfg,
                &self.extractor_cfg,
                &self.stopwords,
            )?
        } else {
            nebula_engine::Database::create_configured_single(
                path,
                &self.password,
                nebula_storage::DEFAULT_PAGE_SIZE,
                db_name,
                &self.cfg,
                &self.extractor_cfg,
                &self.stopwords,
            )?
        };
        let real_name = db.list_dbs().into_iter().next().unwrap_or_else(|| db_name.to_string());
        let idx = self.slots.len();
        self.slots.push(FileSlot {
            db: Some(db),
            origin,
            path: path.to_path_buf(),
        });
        self.mounts.insert(
            db_name.to_string(),
            Mount {
                file: idx,
                real: real_name,
            },
        );
        Ok(idx)
    }

    /// 解析别名为挂载。
    fn resolve(&self, alias: &str) -> Result<&Mount> {
        self.mounts.get(alias).ok_or_else(|| {
            nebula_core::Error::Engine(format!("unknown database '{alias}'"))
        })
    }

    fn slot_mut(&mut self, idx: usize) -> Result<&mut nebula_engine::Database> {
        self.slots
            .get_mut(idx)
            .and_then(|s| s.db.as_mut())
            .ok_or_else(|| nebula_core::Error::Engine("database file is closed".into()))
    }

    /// 别名列表 → 按槽位分组的真实库(保持去重)。
    fn group_by_file(&self, dbs: &[String]) -> Result<BTreeMap<usize, Vec<String>>> {
        let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for alias in dbs {
            let m = self.resolve(alias)?;
            let entry = groups.entry(m.file).or_default();
            if !entry.contains(&m.real) {
                entry.push(m.real.clone());
            }
        }
        Ok(groups)
    }

    /// 持久化管理状态到 _admin.ndb。
    fn save_admin(&mut self) -> Result<()> {
        let bytes = encode_admin(&self.users, &self.grants);
        self.admin_file.write_index_snapshot(&bytes)?;
        Ok(())
    }

    /// 服务端认证用:返回 (salt, verifier);用户不存在为 None。
    pub fn verifier_of(&self, user: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        self.users
            .get(user)
            .map(|u| (u.salt.clone(), u.verifier.clone()))
    }

    /// 全部挂载名(测试/诊断)。
    pub fn mount_names(&self) -> Vec<String> {
        self.mounts.keys().cloned().collect()
    }

    /// 集群目录路径(展示用)。
    pub fn dir_display(&self) -> &std::path::Path {
        &self.dir
    }

    /// 关闭集群:所有文件槽落盘并持久化用户/授权。
    pub fn close(&mut self) -> Result<()> {
        MemBackend::checkpoint(self)
    }

    /// 在集群上执行一条 SQL(调用方持有会话)。
    pub fn execute(
        &mut self,
        sql: &str,
        session: &mut Session,
    ) -> Result<nebula_engine::QueryResult> {
        let stmt = nebula_sql::parse(sql)?;
        nebula_engine::executor::dispatch(self, session, &stmt)
    }
}

// ------- 管理状态编码 -------

fn encode_admin(
    users: &BTreeMap<String, UserRec>,
    grants: &BTreeMap<String, Vec<GrantRec>>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.raw(ADMIN_MAGIC);
    // 用户
    w.varint(users.len() as u64);
    for u in users.values() {
        w.str(&u.name);
        w.bytes(&u.salt);
        w.bytes(&u.verifier);
    }
    // 授权
    w.varint(grants.len() as u64);
    for (user, recs) in grants {
        w.str(user);
        w.varint(recs.len() as u64);
        for r in recs {
            match &r.object {
                GrantObject::Db(name) => {
                    w.u8(0);
                    w.str(name);
                }
                GrantObject::AllDatabases => w.u8(1),
            }
            w.varint(r.privs.len() as u64);
            for p in &r.privs {
                w.u8(match p {
                    Privilege::Read => 0,
                    Privilege::Write => 1,
                    Privilege::Admin => 2,
                });
            }
        }
    }
    w.into_vec()
}

fn decode_admin(
    bytes: &[u8],
) -> Result<(BTreeMap<String, UserRec>, BTreeMap<String, Vec<GrantRec>>)> {
    let mut users = BTreeMap::new();
    let mut grants = BTreeMap::new();
    if bytes.is_empty() {
        return Ok((users, grants));
    }
    if !bytes.starts_with(ADMIN_MAGIC) {
        return Err(nebula_core::Error::Codec(
            "admin snapshot has bad magic".into(),
        ));
    }
    let mut r = nebula_core::codec::Reader::new(&bytes[ADMIN_MAGIC.len()..]);
    let nu = r.varint()? as usize;
    for _ in 0..nu {
        let name = r.str()?;
        let salt = r.bytes()?;
        let verifier = r.bytes()?;
        users.insert(name.clone(), UserRec { name, salt, verifier });
    }
    let ng = r.varint()? as usize;
    for _ in 0..ng {
        let user = r.str()?;
        let nr = r.varint()? as usize;
        let mut recs = Vec::with_capacity(nr);
        for _ in 0..nr {
            let object = match r.u8()? {
                0 => GrantObject::Db(r.str()?),
                1 => GrantObject::AllDatabases,
                other => {
                    return Err(nebula_core::Error::Codec(format!(
                        "bad grant object tag {other}"
                    )))
                }
            };
            let np = r.varint()? as usize;
            let mut privs = BTreeSet::new();
            for _ in 0..np {
                privs.insert(match r.u8()? {
                    0 => Privilege::Read,
                    1 => Privilege::Write,
                    2 => Privilege::Admin,
                    other => {
                        return Err(nebula_core::Error::Codec(format!(
                            "bad privilege tag {other}"
                        )))
                    }
                });
            }
            recs.push(GrantRec { object, privs });
        }
        grants.insert(user, recs);
    }
    Ok((users, grants))
}

/// 用户名规则:与库名同语法(大小写不敏感)。
fn normalize_user_name(user: &str) -> Result<String> {
    normalize_db_name(user).map_err(|e| match e {
        nebula_core::Error::Engine(m) => {
            nebula_core::Error::Engine(m.replace("database", "user"))
        }
        other => other,
    })
}

// ------- UserDirectory 实现 -------

impl UserDirectory for Cluster {
    fn user_exists(&self, user: &str) -> bool {
        self.users.contains_key(user)
    }

    fn has_priv(&self, user: &str, alias: &str, want: Privilege) -> bool {
        let Some(recs) = self.grants.get(user) else {
            return false;
        };
        recs.iter().any(|r| {
            r.privs.contains(&want)
                && match &r.object {
                    GrantObject::AllDatabases => true,
                    GrantObject::Db(n) => n == alias,
                }
        })
    }

    fn is_admin(&self, user: &str) -> bool {
        self.grants.get(user).is_some_and(|recs| {
            recs.iter().any(|r| {
                r.object == GrantObject::AllDatabases && r.privs.contains(&Privilege::Admin)
            })
        })
    }

    fn create_user(
        &mut self,
        user: &str,
        password: &str,
        if_not_exists: bool,
    ) -> Result<bool> {
        let name = normalize_user_name(user)?;
        if self.users.contains_key(&name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(nebula_core::Error::Engine(format!(
                "user '{name}' already exists"
            )));
        }
        let salt = random_salt();
        let verifier = derive_master_key(password, &salt);
        self.users.insert(
            name.clone(),
            UserRec {
                name: name.clone(),
                salt: salt.to_vec(),
                verifier: verifier.to_vec(),
            },
        );
        self.grants.insert(name.clone(), Vec::new());
        self.save_admin()?;
        Ok(true)
    }

    fn drop_user(&mut self, user: &str, if_exists: bool) -> Result<bool> {
        let name = normalize_user_name(user)?;
        if name == ADMIN_USER {
            return Err(nebula_core::Error::Engine(
                "cannot drop the built-in administrator".into(),
            ));
        }
        if self.users.remove(&name).is_none() {
            if if_exists {
                return Ok(false);
            }
            return Err(nebula_core::Error::Engine(format!(
                "user '{name}' does not exist"
            )));
        }
        self.grants.remove(&name);
        self.save_admin()?;
        Ok(true)
    }

    fn alter_user(&mut self, user: &str, password: &str) -> Result<()> {
        let name = normalize_user_name(user)?;
        let Some(rec) = self.users.get_mut(&name) else {
            return Err(nebula_core::Error::Engine(format!(
                "user '{name}' does not exist"
            )));
        };
        let salt: [u8; SALT_LEN] = random_salt();
        let verifier = derive_master_key(password, &salt);
        rec.salt = salt.to_vec();
        rec.verifier = verifier.to_vec();
        self.save_admin()
    }

    fn grant(
        &mut self,
        user: &str,
        object: &GrantObject,
        privs: &[Privilege],
    ) -> Result<()> {
        let name = normalize_user_name(user)?;
        if !self.users.contains_key(&name) {
            return Err(nebula_core::Error::Engine(format!(
                "user '{name}' does not exist"
            )));
        }
        if let GrantObject::Db(alias) = object {
            if !self.mounts.contains_key(alias) {
                return Err(nebula_core::Error::Engine(format!(
                    "unknown database '{alias}'"
                )));
            }
        }
        let recs = self.grants.entry(name).or_default();
        match recs.iter_mut().find(|r| &r.object == object) {
            Some(r) => r.privs.extend(privs.iter().copied()),
            None => recs.push(GrantRec {
                object: object.clone(),
                privs: privs.iter().copied().collect(),
            }),
        }
        self.save_admin()
    }

    fn revoke(
        &mut self,
        user: &str,
        object: &GrantObject,
        privs: &[Privilege],
    ) -> Result<()> {
        let name = normalize_user_name(user)?;
        if let Some(recs) = self.grants.get_mut(&name) {
            for r in recs.iter_mut() {
                if &r.object == object {
                    for p in privs {
                        r.privs.remove(p);
                    }
                }
            }
            recs.retain(|r| !r.privs.is_empty());
        }
        self.save_admin()
    }

    fn list_users(&self) -> Vec<String> {
        self.users.keys().cloned().collect()
    }

    fn list_grants(&self, user: &str) -> Vec<(GrantObject, Vec<Privilege>)> {
        let Some(recs) = self.grants.get(user) else {
            return Vec::new();
        };
        recs.iter()
            .map(|r| {
                let mut privs: Vec<Privilege> = r.privs.iter().copied().collect();
                privs.sort_by_key(|p| format!("{p:?}"));
                (r.object.clone(), privs)
            })
            .collect()
    }
}

// ------- MemBackend 实现 -------

impl MemBackend for Cluster {
    fn list_dbs(&self) -> Vec<String> {
        self.mounts.keys().cloned().collect()
    }

    fn db_exists(&self, db: &str) -> bool {
        self.mounts.contains_key(db)
    }

    fn create_db(&mut self, db: &str) -> Result<bool> {
        let name = normalize_db_name(db)?;
        if self.mounts.contains_key(&name) {
            return Ok(false);
        }
        let path = self.dir.join(format!("{name}.ndb"));
        if path.exists() {
            // 磁盘已有但未挂载:直接打开注册(不覆盖)
            self.open_file(&path, FileOrigin::Managed)?;
        } else {
            self.create_file(&path, FileOrigin::Managed, &name)?;
        }
        Ok(true)
    }

    fn drop_db(&mut self, db: &str) -> Result<()> {
        let name = normalize_db_name(db)?;
        let Some(mount) = self.mounts.remove(&name) else {
            return Err(nebula_core::Error::Engine(format!(
                "unknown database '{name}'"
            )));
        };
        let (file_idx, real) = (mount.file, mount.real);
        let origin = self
            .slots
            .get(file_idx)
            .map(|s| s.origin)
            .unwrap_or(FileOrigin::Managed);
        // 库内删除
        if let Some(fdb) = self.slots.get_mut(file_idx).and_then(|s| s.db.as_mut()) {
            if fdb.db_exists(&real) {
                fdb.drop_database(&real)?;
            }
        }
        // 受管文件且已无挂载 → 关闭并删除物理文件
        let still_mounted = self.mounts.values().any(|m| m.file == file_idx);
        if origin == FileOrigin::Managed && !still_mounted {
            let path = self.slots[file_idx].path.clone();
            if let Some(slot) = self.slots.get_mut(file_idx) {
                slot.db = None;
            }
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    fn attach_file(&mut self, path: &str, alias: &str) -> Result<()> {
        let alias = normalize_db_name(alias)?;
        if self.mounts.contains_key(&alias) {
            return Err(nebula_core::Error::Engine(format!(
                "database '{alias}' already exists"
            )));
        }
        let p = PathBuf::from(path);
        if !p.exists() {
            return Err(nebula_core::Error::Engine(format!(
                "file not found: {path}"
            )));
        }
        let idx = self.open_file(&p, FileOrigin::Attached)?;
        // 别名必须是该文件的某个真实库名(不支持改名,避免落盘库名不一致)
        let real_names: Vec<String> = self
            .mounts
            .values()
            .filter(|m| m.file == idx)
            .map(|m| m.real.clone())
            .collect();
        if !real_names.contains(&alias) {
            // 回滚刚做的挂载,关闭文件槽
            self.mounts.retain(|_, m| m.file != idx);
            if let Some(slot) = self.slots.get_mut(idx) {
                slot.db = None;
            }
            return Err(nebula_core::Error::Engine(format!(
                "file contains database(s) {}, attach using one of those names",
                real_names.join(", ")
            )));
        }
        Ok(())
    }

    fn detach_db(&mut self, db: &str) -> Result<()> {
        let name = normalize_db_name(db)?;
        let Some(mount) = self.mounts.remove(&name) else {
            return Err(nebula_core::Error::Engine(format!(
                "unknown database '{name}'"
            )));
        };
        let file_idx = mount.file;
        let origin = self
            .slots
            .get(file_idx)
            .map(|s| s.origin)
            .unwrap_or(FileOrigin::Managed);
        if origin != FileOrigin::Attached {
            // 非挂接库:恢复挂载,拒绝操作
            self.mounts.insert(name.clone(), mount);
            return Err(nebula_core::Error::Engine(format!(
                "database '{name}' is not an attached file"
            )));
        }
        let still_mounted = self.mounts.values().any(|m| m.file == file_idx);
        if !still_mounted {
            if let Some(slot) = self.slots.get_mut(file_idx) {
                slot.db = None; // 只关闭,不删除外部文件
            }
        }
        Ok(())
    }

    fn on_use(&mut self, db: &str) -> Result<()> {
        let mount = self.resolve(db)?.clone();
        if let Some(fdb) = self.slots.get_mut(mount.file).and_then(|s| s.db.as_mut()) {
            fdb.on_use(&mount.real)?;
        }
        Ok(())
    }

    fn insert_mem(
        &mut self,
        db: &str,
        content: String,
        tags: Vec<String>,
        source: String,
        importance: f32,
    ) -> Result<MemoryId> {
        let mount = self.resolve(db)?.clone();
        self.slot_mut(mount.file)?
            .insert_mem(&mount.real, content, tags, source, importance)
    }

    fn fetch_mem(&mut self, db: &str, id: MemoryId) -> Result<Option<MemoryRecord>> {
        let mount = self.resolve(db)?.clone();
        self.slot_mut(mount.file)?.fetch_mem(&mount.real, id)
    }

    fn replace_mem(&mut self, db: &str, record: &MemoryRecord) -> Result<()> {
        let mount = self.resolve(db)?.clone();
        self.slot_mut(mount.file)?
            .replace_mem(&mount.real, record)
    }

    fn delete_mems(&mut self, db: &str, ids: &[MemoryId]) -> Result<u64> {
        let mount = self.resolve(db)?.clone();
        self.slot_mut(mount.file)?
            .delete_mems(&mount.real, ids)
    }

    fn keyword_hits(&self, db: &str, term: &str) -> Vec<(MemoryId, f32)> {
        let Ok(mount) = self.resolve(db) else {
            return Vec::new();
        };
        self.slots
            .get(mount.file)
            .and_then(|s| s.db.as_ref())
            .map_or_else(Vec::new, |f| f.keyword_hits(&mount.real, term))
    }

    fn tag_hits(&self, db: &str, tag: &str) -> Vec<MemoryId> {
        let Ok(mount) = self.resolve(db) else {
            return Vec::new();
        };
        self.slots
            .get(mount.file)
            .and_then(|s| s.db.as_ref())
            .map_or_else(Vec::new, |f| f.tag_hits(&mount.real, tag))
    }

    fn all_ids(&self, db: &str) -> Vec<MemoryId> {
        let Ok(mount) = self.resolve(db) else {
            return Vec::new();
        };
        self.slots
            .get(mount.file)
            .and_then(|s| s.db.as_ref())
            .map_or_else(Vec::new, |f| f.all_ids(&mount.real))
    }

    fn search_ranked(
        &mut self,
        dbs: &[String],
        query: &str,
        limit: usize,
    ) -> Result<Vec<RankedRow>> {
        let groups = self.group_by_file(dbs)?;
        let mut all = Vec::new();
        for (file_idx, real_dbs) in groups {
            let fdb = self.slot_mut(file_idx)?;
            let rows = fdb.search_ranked(&real_dbs, query, limit)?;
            all.extend(rows.into_iter().map(|r| {
                let alias = self.alias_of(file_idx, &r.db);
                RankedRow::new(alias, r.id, r.score)
            }));
        }
        sort_global(&mut all);
        all.truncate(limit);
        Ok(all)
    }

    fn related_ranked(
        &mut self,
        dbs: &[String],
        seed: RelatedSeed,
        limit: usize,
    ) -> Result<Vec<RankedRow>> {
        let groups = self.group_by_file(dbs)?;

        // 解析种子到 (种子文件, 文件内种子形态);跨文件时,
        // 非种子文件用种子原文 Text 参与打分。
        let (owner_file, per_file_seed): (Option<usize>, RelatedSeed) = match seed {
            RelatedSeed::Id(id) => {
                // executor 已转 QualifiedId;此处兜底用 dbs 首库
                let first_alias = dbs.first().cloned();
                if let Some(alias) = first_alias {
                    let m = self.resolve(&alias)?.clone();
                    (Some(m.file), RelatedSeed::QualifiedId(m.real, id))
                } else {
                    return Err(nebula_core::Error::Engine(
                        "RELATED TO id requires a database".into(),
                    ));
                }
            }
            RelatedSeed::QualifiedId(alias, id) => {
                let m = self.resolve(&alias)?.clone();
                (Some(m.file), RelatedSeed::QualifiedId(m.real, id))
            }
            RelatedSeed::Text(t) => (None, RelatedSeed::Text(t)),
        };

        let mut all = Vec::new();
        for (file_idx, real_dbs) in groups {
            let use_seed = if Some(file_idx) == owner_file {
                per_file_seed.clone()
            } else {
                // 非种子文件:需取种子原文做文本检索
                match &per_file_seed {
                    RelatedSeed::Text(t) => RelatedSeed::Text(t.clone()),
                    RelatedSeed::QualifiedId(real, id) => {
                        let rec = self.fetch_mem_via(owner_file.unwrap(), real, *id)?;
                        let Some(rec) = rec else {
                            return Err(nebula_core::Error::Sql(format!(
                                "RELATED TO: no memory {real}.{id}"
                            )));
                        };
                        RelatedSeed::Text(rec.content)
                    }
                    RelatedSeed::Id(_) => continue,
                }
            };
            let fdb = self.slot_mut(file_idx)?;
            let rows = fdb.related_ranked(&real_dbs, use_seed, limit)?;
            all.extend(rows.into_iter().map(|r| {
                let alias = self.alias_of(file_idx, &r.db);
                RankedRow::new(alias, r.id, r.score)
            }));
        }
        sort_global(&mut all);
        all.truncate(limit);
        Ok(all)
    }

    fn cache_rows(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for slot in &self.slots {
            if let Some(db) = &slot.db {
                for mut row in db.cache_rows() {
                    row[0] = format!("{}:{}", slot.path.display(), row[0]);
                    out.push(row);
                }
            }
        }
        out
    }

    fn clear_caches(&mut self) {
        for slot in &mut self.slots {
            if let Some(db) = slot.db.as_mut() {
                db.clear_caches();
            }
        }
    }

    fn set_cache_capacity(&mut self, target: CacheTarget, capacity: usize) {
        for slot in &mut self.slots {
            if let Some(db) = slot.db.as_mut() {
                db.set_cache_capacity(target, capacity);
            }
        }
    }

    fn hot_rows(&self, db: &str, limit: usize) -> Vec<Vec<String>> {
        let Ok(mount) = self.resolve(db) else {
            return Vec::new();
        };
        self.slots
            .get(mount.file)
            .and_then(|s| s.db.as_ref())
            .map_or_else(Vec::new, |f| f.hot_rows(&mount.real, limit))
    }

    fn status_rows(&self) -> Vec<Vec<String>> {
        let mut rows = vec![
            vec!["cluster_dir".into(), self.dir.display().to_string()],
            vec!["databases".into(), self.mounts.len().to_string()],
            vec![
                "files_open".into(),
                self.slots.iter().filter(|s| s.db.is_some()).count().to_string(),
            ],
            vec!["users".into(), self.users.len().to_string()],
        ];
        for slot in &self.slots {
            if let Some(db) = &slot.db {
                rows.push(vec![
                    format!("file:{}", slot.path.display()),
                    db.memory_count().to_string(),
                ]);
            }
        }
        rows
    }

    fn checkpoint(&mut self) -> Result<()> {
        for slot in &mut self.slots {
            if let Some(db) = slot.db.as_mut() {
                db.checkpoint()?;
            }
        }
        self.save_admin()
    }
}

impl Cluster {
    /// 真实库名 → 对外别名(同文件内回退为真实名)。
    fn alias_of(&self, file: usize, real: &str) -> String {
        self.mounts
            .iter()
            .find(|(_, m)| m.file == file && m.real == real)
            .map(|(a, _)| a.clone())
            .unwrap_or_else(|| real.to_string())
    }

    /// 绕过集群命名空间直接到指定槽位取记录(种子解析用)。
    fn fetch_mem_via(
        &mut self,
        file: usize,
        real: &str,
        id: MemoryId,
    ) -> Result<Option<MemoryRecord>> {
        self.slot_mut(file)?.fetch_mem(real, id)
    }
}

fn sort_global(rows: &mut Vec<RankedRow>) {
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.db.cmp(&b.db))
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_engine::Session;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("nebula-cluster-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn cluster_creates_default_admin_and_main() {
        let dir = temp_dir("init");
        let c = Cluster::open_or_create(&dir, "secret").unwrap();
        assert!(c.mount_names().contains(&"main".to_string()));
        assert!(c.user_exists(ADMIN_USER));
        assert!(c.is_admin(ADMIN_USER));
        assert!(c.verifier_of(ADMIN_USER).is_some());
    }

    #[test]
    fn create_drop_database_lifecycle() {
        let dir = temp_dir("crud");
        let mut c = Cluster::open_or_create(&dir, "pw").unwrap();
        let mut s = Session::admin();
        c.execute("CREATE DATABASE work", &mut s).unwrap();
        assert!(c.db_exists("work"));
        assert!(dir.join("work.ndb").exists());
        c.execute("USE work", &mut s).unwrap();
        c.execute(
            "INSERT INTO memories (content) VALUES ('learn rust')",
            &mut s,
        )
        .unwrap();
        c.execute("DROP DATABASE work", &mut s).unwrap();
        assert!(!c.db_exists("work"));
        assert!(!dir.join("work.ndb").exists());
    }

    #[test]
    fn grants_enforced_across_files() {
        let dir = temp_dir("grant");
        let mut c = Cluster::open_or_create(&dir, "pw").unwrap();
        let mut s = Session::admin();
        c.execute("CREATE DATABASE work", &mut s).unwrap();
        c.execute("CREATE USER bob IDENTIFIED BY 'bobpw'", &mut s).unwrap();
        c.execute("GRANT READ ON work TO bob", &mut s).unwrap();
        assert!(c.has_priv("bob", "work", Privilege::Read));
        assert!(!c.has_priv("bob", "work", Privilege::Write));
        c.execute("REVOKE READ ON work FROM bob", &mut s).unwrap();
        assert!(!c.has_priv("bob", "work", Privilege::Read));
    }

    #[test]
    fn non_admin_cannot_write_without_grant() {
        let dir = temp_dir("deny");
        let mut c = Cluster::open_or_create(&dir, "pw").unwrap();
        let mut s = Session::admin();
        c.execute("CREATE USER carol IDENTIFIED BY 'p'", &mut s).unwrap();
        let mut bob = Session::new("carol", "main");
        let err = c.execute("INSERT { hello } INTO main", &mut bob);
        assert!(err.is_err());
    }

    #[test]
    fn admin_state_survives_reopen() {
        let dir = temp_dir("reopen");
        {
            let mut c = Cluster::open_or_create(&dir, "pw").unwrap();
            let mut s = Session::admin();
            c.execute("CREATE USER dana IDENTIFIED BY 'x'", &mut s).unwrap();
            c.execute("CREATE DATABASE work", &mut s).unwrap();
            c.execute("GRANT WRITE ON work TO dana", &mut s).unwrap();
            c.checkpoint().unwrap();
        }
        let c2 = Cluster::open_or_create(&dir, "pw").unwrap();
        assert!(c2.user_exists("dana"));
        assert!(c2.has_priv("dana", "work", Privilege::Write));
        assert!(c2.db_exists("work"));
    }

    #[test]
    fn auto_mounts_ndb_files_on_open() {
        let dir = temp_dir("scan");
        let mut c = Cluster::open_or_create(&dir, "pw").unwrap();
        let mut s = Session::admin();
        c.execute("CREATE DATABASE alpha", &mut s).unwrap();
        c.checkpoint().unwrap();
        drop(c);
        let c2 = Cluster::open_or_create(&dir, "pw").unwrap();
        assert!(c2.db_exists("alpha"));
    }
}
