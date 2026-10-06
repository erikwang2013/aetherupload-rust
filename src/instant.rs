// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 秒传索引 —— 对应 PHP 版 `RedisSavedPath.php`。
//!
//! Redis 客户端以 trait 注入（对齐 snowflake-rust 的做法）：本 crate 既不依赖
//! `redis` crate，也不需要网络栈。宿主把自家的客户端（`redis`、`fred`、连接池……）
//! 包一层实现 [`InstantStore`] 即可；没接的同学保持默认的 [`NullInstantStore`]，
//! 只要不开 `instant_completion`，内核根本不会碰它。
//!
//! **键格式与 PHP 版完全一致**（`aetherupload:resource:<group>_<hash>`，每条独立 TTL），
//! 且保留旧版单 hash（`aetherupload_resource`）的回退读取 —— 从 PHP 版迁移过来的站点
//! 不需要先跑一遍 `build` 就能继续命中秒传。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::util::is_safe_path_component;

/// 每条记录独立 key 的前缀（与 PHP `RedisSavedPath::KEY_PREFIX` 一致）。
pub const KEY_PREFIX: &str = "aetherupload:resource:";

/// 旧版把所有记录放在单个 hash 里；仅用于回退读取与清理。
pub const LEGACY_HASH_KEY: &str = "aetherupload_resource";

/// 默认 TTL：7 天（`resource_redis_expire` 可覆盖）。
pub const EXPIRE_SECONDS: u64 = 604_800;

/// 秒传索引的存储端口：五个动作。
///
/// 刻意不做返回类型归一化 —— PHP 版在 `predis`（`null`/`0`）与 `phpredis`
/// （`false`/`bool`）之间的分支就是兼容层；Rust 用 `Option` 把这层差异在 trait
/// 实现里一次抹平：**未命中一律 `None`**。
pub trait InstantStore: Send + Sync {
    /// 读取一个 key；未命中返回 `Ok(None)`。
    fn get(&self, key: &str) -> Result<Option<String>>;

    /// `SETEX`：写入并设置该条记录自己的过期秒数。
    fn set_ex(&self, key: &str, value: &str, ttl_seconds: u64) -> Result<()>;

    /// 删除一个 key（幂等：不存在也算成功）。
    fn del(&self, key: &str) -> Result<()>;

    /// 批量写入（`aetherupload:build` 重建索引时按批提交）。
    /// 默认逐条 `SETEX`；客户端支持 pipeline 时覆盖它即可。
    fn set_multi(&self, entries: &[(String, String)], ttl_seconds: u64) -> Result<()> {
        for (key, value) in entries {
            self.set_ex(key, value, ttl_seconds)?;
        }

        Ok(())
    }

    /// 旧版单 hash 回退读取；不需要兼容存量数据的实现用默认实现即可。
    fn legacy_get(&self, _field: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// 删除旧版单 hash 里的字段；默认什么都不做。
    fn legacy_del(&self, _field: &str) -> Result<()> {
        Ok(())
    }

    /// 删除某个前缀下的全部记录（`aetherupload:build` 重建前清空索引用）。
    /// 默认实现什么都不做 —— 不支持遍历的实现（如受限的云 Redis）如实保持空操作，
    /// 由 `Console::build` 把「清空失败」如实报出来。
    fn delete_prefix(&self, _prefix: &str) -> Result<()> {
        Ok(())
    }
}

/// 未接 Redis 的默认实现：**一调用就报错**，而不是静默失败。
///
/// 与 PHP `NullRedis` 同一取向：开了秒传却没接客户端时，报错远比「秒传静默失效」
/// 容易排查。内核只在 `instant_completion = true` 时才会碰它。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullInstantStore;

impl NullInstantStore {
    /// 统一的失败构造：一调用就报错，且消息说清「缺什么、怎么补」。
    fn fail<T>(&self) -> Result<T> {
        Err(Error::Backend(
            "未为此宿主配置 Redis：请注册 InstantStore 实现，或关闭 instant_completion".to_string(),
        ))
    }
}

impl InstantStore for NullInstantStore {
    fn get(&self, _key: &str) -> Result<Option<String>> {
        self.fail()
    }

    fn set_ex(&self, _key: &str, _value: &str, _ttl_seconds: u64) -> Result<()> {
        self.fail()
    }

    fn del(&self, _key: &str) -> Result<()> {
        self.fail()
    }
}

/// 内存实现：**供测试与示例使用**（PHP 侧对应测试里的数组替身）。
///
/// 不做过期 —— TTL 参数被忽略；只在单进程内有效。生产环境请注入真实 Redis 客户端。
#[derive(Debug, Default)]
pub struct MemoryInstantStore {
    entries: Mutex<HashMap<String, String>>,
}

impl MemoryInstantStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InstantStore for MemoryInstantStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.entries.lock().unwrap().get(key).cloned())
    }

    fn set_ex(&self, key: &str, value: &str, _ttl_seconds: u64) -> Result<()> {
        self.entries
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn del(&self, key: &str) -> Result<()> {
        self.entries.lock().unwrap().remove(key);
        Ok(())
    }

    fn delete_prefix(&self, prefix: &str) -> Result<()> {
        self.entries
            .lock()
            .unwrap()
            .retain(|key, _| !key.starts_with(prefix));
        Ok(())
    }
}

/// 秒传索引的门面：键构造 + 回退读取语义（对应 `RedisSavedPath` 的静态方法）。
pub struct InstantIndex<'a> {
    store: &'a dyn InstantStore,
    ttl_seconds: u64,
}

impl<'a> InstantIndex<'a> {
    pub fn new(store: &'a dyn InstantStore, ttl_seconds: u64) -> Self {
        Self { store, ttl_seconds }
    }

    /// 构造记录键 `group_hash` —— 对应 `RedisSavedPath::getKey()`。
    ///
    /// hash 是客户端可控值：非安全字符集（最长 64）直接 [`Error::InvalidOperation`]，
    /// 否则会拼出 `file_Array` 这类垃圾键。
    pub fn key(group: &str, hash: &str) -> Result<String> {
        if !is_safe_path_component(hash, false, Some(64)) {
            return Err(Error::InvalidOperation);
        }

        Ok(format!("{group}_{hash}"))
    }

    /// 读取：主键未命中时回退旧版单 hash（历史数据仍可命中）。
    ///
    /// 与 PHP 的差异：PHP 未命中抛 `read error`、由调用方 catch 成 null；这里直接
    /// 返回 `Ok(None)` —— 调用点少一层 catch，语义相同。
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        if let Some(saved_path) = self.store.get(&format!("{KEY_PREFIX}{key}"))? {
            return Ok(Some(saved_path));
        }

        self.store.legacy_get(key)
    }

    /// 写入：`SETEX` + 每条记录独立 TTL。
    pub fn set(&self, key: &str, saved_path: &str) -> Result<()> {
        self.store
            .set_ex(&format!("{KEY_PREFIX}{key}"), saved_path, self.ttl_seconds)
    }

    /// 批量写入：`(键, savedPath)` 列表，键会补上统一前缀。
    pub fn set_multi(&self, entries: &[(String, String)]) -> Result<()> {
        let prefixed: Vec<(String, String)> = entries
            .iter()
            .map(|(key, saved_path)| (format!("{KEY_PREFIX}{key}"), saved_path.clone()))
            .collect();

        self.store.set_multi(&prefixed, self.ttl_seconds)
    }

    /// 删除：主键 + 旧版字段一起删（幂等）。
    pub fn delete(&self, key: &str) -> Result<()> {
        self.store.del(&format!("{KEY_PREFIX}{key}"))?;
        self.store.legacy_del(key)
    }

    /// 清空全部索引（`aetherupload:build` 重建前置动作）。
    pub fn delete_all(&self) -> Result<()> {
        self.store.delete_prefix(KEY_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format_matches_php() {
        assert_eq!(
            InstantIndex::key("file", "d41d8cd98f00b204e9800998ecf8427e").unwrap(),
            "file_d41d8cd98f00b204e9800998ecf8427e"
        );
        // 客户端可控值：非法字符与超长一律拒
        assert!(matches!(
            InstantIndex::key("file", "../etc"),
            Err(Error::InvalidOperation)
        ));
        assert!(matches!(
            InstantIndex::key("file", ""),
            Err(Error::InvalidOperation)
        ));
        assert!(matches!(
            InstantIndex::key("file", &"a".repeat(65)),
            Err(Error::InvalidOperation)
        ));
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let store = MemoryInstantStore::new();
        let index = InstantIndex::new(&store, EXPIRE_SECONDS);

        assert_eq!(index.get("file_abc").unwrap(), None);
        index.set("file_abc", "file_202610_abc.jpg").unwrap();
        assert_eq!(
            index.get("file_abc").unwrap().as_deref(),
            Some("file_202610_abc.jpg")
        );
        // 键前缀与 PHP 一致
        assert!(
            store
                .get("aetherupload:resource:file_abc")
                .unwrap()
                .is_some()
        );

        index.delete("file_abc").unwrap();
        assert_eq!(index.get("file_abc").unwrap(), None);
        // 幂等
        index.delete("file_abc").unwrap();
    }

    /// 旧版单 hash 的数据仍要能读到（PHP 版的回退读取）。
    #[test]
    fn legacy_hash_fallback_is_readable() {
        struct LegacyStore(MemoryInstantStore);

        impl InstantStore for LegacyStore {
            fn get(&self, key: &str) -> Result<Option<String>> {
                self.0.get(key)
            }
            fn set_ex(&self, key: &str, value: &str, ttl: u64) -> Result<()> {
                self.0.set_ex(key, value, ttl)
            }
            fn del(&self, key: &str) -> Result<()> {
                self.0.del(key)
            }
            fn legacy_get(&self, field: &str) -> Result<Option<String>> {
                self.0.get(&format!("{LEGACY_HASH_KEY}:{field}"))
            }
            fn legacy_del(&self, field: &str) -> Result<()> {
                self.0.del(&format!("{LEGACY_HASH_KEY}:{field}"))
            }
        }

        let store = LegacyStore(MemoryInstantStore::new());
        store
            .set_ex(
                &format!("{LEGACY_HASH_KEY}:file_old"),
                "file_202610_old.jpg",
                0,
            )
            .unwrap();

        let index = InstantIndex::new(&store, EXPIRE_SECONDS);
        assert_eq!(
            index.get("file_old").unwrap().as_deref(),
            Some("file_202610_old.jpg")
        );

        index.delete("file_old").unwrap();
        assert_eq!(index.get("file_old").unwrap(), None);
    }

    #[test]
    fn null_store_fails_loudly() {
        let store = NullInstantStore;
        assert!(matches!(store.get("k"), Err(Error::Backend(_))));
        assert!(matches!(store.set_ex("k", "v", 1), Err(Error::Backend(_))));
        assert!(matches!(store.del("k"), Err(Error::Backend(_))));
    }

    #[test]
    fn delete_all_clears_the_prefix() {
        let store = MemoryInstantStore::new();
        let index = InstantIndex::new(&store, EXPIRE_SECONDS);

        index.set("file_a", "a.jpg").unwrap();
        index.set("file_b", "b.jpg").unwrap();
        index.delete_all().unwrap();

        assert_eq!(index.get("file_a").unwrap(), None);
        assert_eq!(index.get("file_b").unwrap(), None);
    }
}
