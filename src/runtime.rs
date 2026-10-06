// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 运行时绑定 —— 对应 PHP 版 `Runtime.php` + `Util.php` 里需要依赖的那几个方法。
//!
//! PHP 的 `Runtime` 是静态门面：静态绑定的适配器 + `RequestContext` 里按执行上下文
//! 隔离的可变状态（webman 常驻进程与 Hyperf 协程下多请求交错，进程级可变状态会被
//! 下一个请求覆盖）。Rust 版把这两件事一起消掉：
//!
//! - **不可变绑定** → 一个 [`Runtime`] 实例，构造后不再变化，`Arc` 共享即可；
//! - **每请求状态** → 显式传参（`&GroupSnapshot`、`locale`），随调用栈走，
//!   既不需要上下文存储，也不可能串组。
//!
//! 存储驱动、秒传索引、事件出口、MIME 探测器都以 `Arc<dyn Trait>` 注入，
//! 默认值分别是 local 磁盘、Null（一调用就报错）、空实现、魔数探测器。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, StorageDriver};
use crate::error::Result;
// `Error` 只在「开了 s3 配置但没开 s3 feature」的兜底分支里用
#[cfg(not(feature = "s3"))]
use crate::error::Error;
use crate::events::{EventSink, NoopEvents};
use crate::instant::{EXPIRE_SECONDS, InstantIndex, InstantStore, NullInstantStore};
use crate::mime::{MagicBytesDetector, MimeDetector};
use crate::resource::Resource;
use crate::saved_path::SavedPath;
use crate::storage::{LocalStorage, Storage};
use crate::util::{file_stem_of, is_safe_path_component};

pub struct Runtime {
    config: Arc<Config>,
    base_path: PathBuf,
    upload_root: PathBuf,
    storage: Arc<dyn Storage>,
    instant: Arc<dyn InstantStore>,
    events: Arc<dyn EventSink>,
    detector: Arc<dyn MimeDetector>,
}

impl Runtime {
    /// 按配置装配运行时。
    ///
    /// `base_path` 是宿主项目根（PHP 里 `Runtime::basePath()`）；上传根目录即
    /// `base_path/root_dir`。返回 [`Error::Backend`] 的唯一情形是存储驱动配置
    /// 不可用（`driver = s3` 但没开 `s3` feature，或驱动名不认识）——
    /// 与 PHP 在 `Storage::driver()` 里报错的时机不同（构造期 vs 请求期），
    /// 但都选择「大声失败」而不是悄悄回落 local。
    pub fn new(config: Config, base_path: impl Into<PathBuf>) -> Result<Self> {
        let config = Arc::new(config);
        let base_path = base_path.into();
        let upload_root = base_path.join(&config.root_dir);
        let storage = default_storage(&config, &upload_root)?;

        Ok(Self {
            config,
            base_path,
            upload_root,
            storage,
            instant: Arc::new(NullInstantStore),
            events: Arc::new(NoopEvents),
            detector: Arc::new(MagicBytesDetector),
        })
    }

    // —— 替换注入（链式，构造期完成）——

    pub fn with_storage(mut self, storage: Arc<dyn Storage>) -> Self {
        self.storage = storage;
        self
    }

    pub fn with_instant(mut self, instant: Arc<dyn InstantStore>) -> Self {
        self.instant = instant;
        self
    }

    pub fn with_events(mut self, events: Arc<dyn EventSink>) -> Self {
        self.events = events;
        self
    }

    pub fn with_detector(mut self, detector: Arc<dyn MimeDetector>) -> Self {
        self.detector = detector;
        self
    }

    // —— 只读访问 ——

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    pub fn upload_root(&self) -> &Path {
        &self.upload_root
    }

    pub fn storage(&self) -> &dyn Storage {
        self.storage.as_ref()
    }

    pub fn instant(&self) -> &dyn InstantStore {
        self.instant.as_ref()
    }

    pub fn events(&self) -> &dyn EventSink {
        self.events.as_ref()
    }

    pub fn detector(&self) -> &dyn MimeDetector {
        self.detector.as_ref()
    }

    /// 秒传索引门面（TTL 取 `resource_redis_expire`）。
    pub fn instant_index(&self) -> InstantIndex<'_> {
        InstantIndex::new(self.instant.as_ref(), self.config.resource_redis_expire)
    }

    /// 按 TTL 默认值 7 天构造（`InstantIndex::new` 的显式版本，测试与迁移脚本用）。
    pub fn instant_index_with_ttl(&self, ttl_seconds: u64) -> InstantIndex<'_> {
        InstantIndex::new(self.instant.as_ref(), ttl_seconds)
    }

    /// 默认 TTL 常量透出（对应 PHP `RedisSavedPath::EXPIRE_SECONDS`）。
    pub fn default_instant_ttl() -> u64 {
        EXPIRE_SECONDS
    }

    // —— 资源寻址与删除（PHP `Util::getResource` / `deleteResource` / `deleteRedisSavedPath`）——

    /// 由 `savedPath` 解析出成品对象；分组不存在或路径非法时返回 `None`
    /// （PHP 版同样用 catch 把异常吞成 `false`）。
    pub fn resource(&self, saved_path: &str) -> Option<Resource> {
        let params = SavedPath::decode(saved_path).ok()?;
        let snapshot = self.config.resolve_group(&params.group).ok()?;

        Some(Resource::new(
            &self.upload_root,
            &params.group,
            &snapshot.group_dir,
            &params.group_sub_dir,
            &params.resource_name,
        ))
    }

    /// 删除资源文件，并联动清理秒传记录 —— 避免死链。
    ///
    /// 幂等语义与 PHP 一致：文件不存在、分组非法、秒传后端故障都返回 `false`
    /// （调用方据此决定是否重试），**不 panic 也不向上抛**。
    pub fn delete_resource(&self, saved_path: &str) -> bool {
        let Some(resource) = self.resource(saved_path) else {
            return false;
        };

        if resource.delete(self.storage()).is_err() {
            return false;
        }

        // 秒传清理失败不影响删除结果（与 PHP 的 try/catch 一致）
        let _ = self.delete_instant_path(saved_path);

        true
    }

    /// 删除对应的秒传记录。键里的 hash 取文件名去掉扩展名 ——
    /// 那正是上传时写进去的整份 md5。
    pub fn delete_instant_path(&self, saved_path: &str) -> bool {
        let Ok(params) = SavedPath::decode(saved_path) else {
            return false;
        };

        let hash = file_stem_of(&params.resource_name);
        if !is_safe_path_component(&hash, false, Some(64)) {
            return false;
        }

        let Ok(key) = InstantIndex::key(&params.group, &hash) else {
            return false;
        };

        self.instant_index().delete(&key).is_ok()
    }
}

/// 手写 `Debug`：注入进来的 trait 对象（存储 / 秒传 / 事件 / 探测器）不打印内部结构，
/// 只给「这是谁、配了哪些分组、开关状态如何」—— 日志与排查要的是这些。
impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("base_path", &self.base_path)
            .field("upload_root", &self.upload_root)
            .field("root_dir", &self.config.root_dir)
            .field("chunk_size", &self.config.chunk_size)
            .field("groups", &self.config.groups.keys().collect::<Vec<_>>())
            .field("instant_completion", &self.config.instant_completion)
            .field("lax_mode", &self.config.lax_mode)
            .field("x_accel_redirect", &self.config.x_accel_redirect)
            .field("storage_driver", &self.config.storage.driver)
            .finish_non_exhaustive()
    }
}

/// 按 `storage.driver` 造驱动 —— 对应 PHP `Storage::driver()`。
fn default_storage(config: &Config, upload_root: &Path) -> Result<Arc<dyn Storage>> {
    match config.storage.driver {
        StorageDriver::Local => Ok(Arc::new(LocalStorage::new(upload_root))),

        StorageDriver::S3 => {
            #[cfg(feature = "s3")]
            {
                Ok(Arc::new(crate::storage::s3::S3Storage::new(
                    config.storage.s3.clone(),
                    upload_root,
                )?))
            }

            #[cfg(not(feature = "s3"))]
            {
                let _ = upload_root;
                Err(Error::Backend(
                    "storage.driver = s3 需要启用 crate feature `s3`".to_string(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instant::MemoryInstantStore;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("aetherupload-runtime-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("file/202610")).unwrap();
        root
    }

    #[test]
    fn paths_follow_root_dir_config() {
        let root = temp_root("paths");
        let runtime = Runtime::new(Config::default(), &root).unwrap();

        assert_eq!(runtime.base_path(), root.as_path());
        assert_eq!(runtime.upload_root(), root.join("storage/app/aetherupload"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resource_lookup_and_deletion() {
        let root = temp_root("delete");
        let runtime = Runtime::new(Config::default(), &root)
            .unwrap()
            .with_instant(Arc::new(MemoryInstantStore::new()));

        // 造一份成品 + 一条秒传记录
        let file = runtime.upload_root().join("file/202610/abc.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();

        let saved_path = "file_202610_abc.txt";
        runtime
            .instant_index()
            .set("file_abc", saved_path)
            .expect("内存 store 可写");

        let resource = runtime.resource(saved_path).expect("可解析");
        assert_eq!(resource.name, "abc.txt");
        assert_eq!(resource.path, file);

        assert!(runtime.delete_resource(saved_path));
        assert!(!file.exists());
        assert_eq!(
            runtime.instant_index().get("file_abc").unwrap(),
            None,
            "秒传记录应联动清理"
        );

        // 幂等：再删一次失败（文件已不在），非法路径也失败
        assert!(!runtime.delete_resource(saved_path));
        assert!(!runtime.delete_resource("bad_path"));
        assert!(!runtime.delete_resource("nope_group_202610_abc.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn s3_without_feature_is_a_loud_error() {
        let root = temp_root("s3");
        let mut config = Config::default();
        config.storage.driver = StorageDriver::S3;

        let result = Runtime::new(config, &root);

        #[cfg(not(feature = "s3"))]
        assert!(matches!(result, Err(Error::Backend(_))));
        #[cfg(feature = "s3")]
        assert!(result.is_err(), "s3 配置缺 bucket 应当报错");

        let _ = std::fs::remove_dir_all(&root);
    }
}
