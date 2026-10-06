// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 配置结构与分组解析 —— 对应 PHP 版 `config/aetherupload.php` + `ConfigMapper.php`。
//!
//! 键名与 PHP 版逐字相同（`root_dir` / `chunk_size` / `resource_subdir_rule` …），
//! 便于对照迁移；Rust 用结构体承载，编译期就能发现拼错的键。
//!
//! **与 PHP 的两点结构性差异**：
//!
//! 1. PHP 的 `ConfigMapper` 是进程级单例 + 每请求 `applyGroupConfig()` 快照，
//!    因为常驻进程/协程下多个请求会交错。Rust 版没有单例：一次上传调用开始时
//!    用 [`Config::resolve_group`] 解析出 [`GroupSnapshot`]，之后全程只读它
//!    —— 同样的隔离效果，不需要锁，也不可能串组。
//! 2. PHP 有四个 `middleware_*` 配置（把回调名交给宿主框架）。Rust 版不做中间件
//!    注册表：宿主直接用自己的中间件机制（Axum 的 `layer`、Actix 的 `wrap` 等），
//!    路由路径由 `route_*` 给出，见各 `integrations/`。

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// 子目录生成规则（对应 `resource_subdir_rule`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubdirRule {
    Year,
    #[default]
    Month,
    Date,
    Const,
}

impl SubdirRule {
    /// 解析配置里的字符串；**未知取值按月**（与 PHP `generateSubDirName()` 的
    /// `default` 分支一致）。
    pub fn parse(value: &str) -> Self {
        match value {
            "year" => Self::Year,
            "date" => Self::Date,
            "const" => Self::Const,
            _ => Self::Month,
        }
    }
}

/// 存储驱动（对应 `storage.driver`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageDriver {
    #[default]
    Local,
    S3,
}

/// S3 驱动配置（对应 `storage.s3`）。字段名与 PHP 版一致。
#[derive(Debug, Clone)]
pub struct S3Config {
    /// 留空 = AWS 默认（按 region 推导）；自托管填自家端点。
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// MinIO / Ceph 等自托管必须 true；AWS 等公有云可用 false。
    pub path_style: bool,
    /// 对象键前缀（可空）。
    pub prefix: String,
    /// 超过此大小走 multipart（默认 100MB）。
    pub multipart_threshold: u64,
    /// 签名时 payload 哈希形态：`hash`（默认）| `unsigned`。
    pub payload_signing: PayloadSigning,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            region: "us-east-1".to_string(),
            bucket: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            path_style: true,
            prefix: String::new(),
            multipart_threshold: 104_857_600,
            payload_signing: PayloadSigning::Hash,
        }
    }
}

/// `payload_signing` 的两种形态（华为云 OBS 等只接受 UNSIGNED-PAYLOAD）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadSigning {
    #[default]
    Hash,
    Unsigned,
}

/// 存储配置段。
#[derive(Debug, Clone, Default)]
pub struct StorageConfig {
    pub driver: StorageDriver,
    pub s3: S3Config,
}

/// 一个资源分组的配置（对应 `groups.<name>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConfig {
    /// 分组目录名（上传根目录下的第一层）。
    pub group_dir: String,
    /// 该分组允许的单文件最大字节数；`0` = 不限制（但声明大小为 0 仍会被拒）。
    pub resource_maxsize: u64,
    /// 扩展名白名单；空 = 只受黑名单约束。
    pub resource_extensions: Vec<String>,
    pub event_before_upload_complete: bool,
    pub event_upload_complete: bool,
}

/// 插件配置。字段与 `config/aetherupload.php` 一一对应。
#[derive(Debug, Clone)]
pub struct Config {
    /// 对应 `enable`：宿主可据此决定是否挂载路由。
    pub enable: bool,
    /// 秒传开关（需 Redis 与浏览器支持，默认关闭）。
    pub instant_completion: bool,
    /// 秒传记录 TTL（秒），默认 7 天。
    pub resource_redis_expire: u64,
    /// 上传根目录（相对宿主项目根），默认 `storage/app/aetherupload`。
    pub root_dir: String,
    /// 分块大小（字节）。
    pub chunk_size: u64,
    pub resource_subdir_rule: SubdirRule,
    /// 后缀名黑名单：命中的一律拒绝。
    pub forbidden_extensions: Vec<String>,
    /// 额外 MIME 映射（`(扩展名, 类型)`），覆盖内置表。
    pub extra_mime_types: Vec<(String, String)>,
    pub route_preprocess: String,
    pub route_uploading: String,
    pub route_display: String,
    pub route_download: String,
    /// 宽松模式：跳过上传前的 hash 计算；开启后无法秒传与完整性校验。
    pub lax_mode: bool,
    /// 交由前置服务器（如 nginx）直发文件。
    pub x_accel_redirect: bool,
    pub storage: StorageConfig,
    pub groups: BTreeMap<String, GroupConfig>,
}

impl Default for Config {
    /// 与 PHP `config/aetherupload.php` 的默认值逐项一致（含默认 `file` 分组）。
    fn default() -> Self {
        Self {
            enable: true,
            instant_completion: false,
            resource_redis_expire: 604_800,
            root_dir: "storage/app/aetherupload".to_string(),
            chunk_size: 1_000_000,
            resource_subdir_rule: SubdirRule::Month,
            forbidden_extensions: [
                "php", "part", "html", "shtml", "htm", "shtm", "xhtml", "xml", "js", "jsp", "asp",
                "java", "py", "sh", "bat", "exe", "dll", "cgi", "htaccess", "reg", "aspx", "vbs",
            ]
            .iter()
            .map(|ext| ext.to_string())
            .collect(),
            extra_mime_types: Vec::new(),
            route_preprocess: "/aetherupload/preprocess".to_string(),
            route_uploading: "/aetherupload/uploading".to_string(),
            route_display: "/aetherupload/display".to_string(),
            route_download: "/aetherupload/download".to_string(),
            lax_mode: false,
            x_accel_redirect: false,
            storage: StorageConfig::default(),
            groups: BTreeMap::from([(
                "file".to_string(),
                GroupConfig {
                    group_dir: "file".to_string(),
                    resource_maxsize: 104_857_600,
                    resource_extensions: [
                        "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "pdf", "doc", "docx",
                        "xls", "xlsx", "ppt", "pptx", "txt", "zip", "rar", "7z", "mp4", "mp3",
                        "wav",
                    ]
                    .iter()
                    .map(|ext| ext.to_string())
                    .collect(),
                    event_before_upload_complete: false,
                    event_upload_complete: false,
                },
            )]),
        }
    }
}

impl Config {
    /// 解析分组，得到本次调用全程使用的配置快照 —— 对应 PHP `ConfigMapper::applyGroupConfig()`。
    ///
    /// 两条拒绝规则原样搬过来：
    /// - 分组名含下划线 → [`Error::InvalidOperation`]。分组名参与 `savedPath` 的
    ///   `_` 分隔编码，含下划线会让解码错位、该分组下资源永久 404。PHP 在配置与上传
    ///   两个阶段都会拦；Rust 版在这里（唯一的解析入口）拦一次。
    /// - 分组不存在 → [`Error::InvalidOperation`]。
    pub fn resolve_group(&self, group: &str) -> Result<GroupSnapshot> {
        if group.is_empty() || group.contains('_') {
            return Err(Error::InvalidOperation);
        }

        let Some(group_config) = self.groups.get(group) else {
            return Err(Error::InvalidOperation);
        };

        Ok(GroupSnapshot {
            group: group.to_string(),
            group_dir: group_config.group_dir.clone(),
            resource_maxsize: group_config.resource_maxsize,
            resource_extensions: group_config.resource_extensions.clone(),
            event_before_upload_complete: group_config.event_before_upload_complete,
            event_upload_complete: group_config.event_upload_complete,
            root_dir: self.root_dir.clone(),
            chunk_size: self.chunk_size,
            resource_subdir_rule: self.resource_subdir_rule,
            forbidden_extensions: self.forbidden_extensions.clone(),
            instant_completion: self.instant_completion,
            resource_redis_expire: self.resource_redis_expire,
            lax_mode: self.lax_mode,
            x_accel_redirect: self.x_accel_redirect,
            extra_mime_types: self.extra_mime_types.clone(),
        })
    }

    /// 便捷判断：驱动是否为 s3（适配器与控制器用它决定读路径）。
    pub fn is_s3(&self) -> bool {
        self.storage.driver == StorageDriver::S3
    }
}

/// 一次调用（一个请求）的配置快照：分组配置 + 相关全局项。
///
/// 上传/读取的下游函数全部接收 `&GroupSnapshot` 而不是 `&Config` —— 这样「一组一快照、
/// 全程不换」在类型上就成立了（PHP 版靠每请求重建 ConfigMapper 实例达到同样效果）。
#[derive(Debug, Clone)]
pub struct GroupSnapshot {
    pub group: String,
    pub group_dir: String,
    pub resource_maxsize: u64,
    pub resource_extensions: Vec<String>,
    pub event_before_upload_complete: bool,
    pub event_upload_complete: bool,

    // 以下为全局项在本次调用里的快照
    pub root_dir: String,
    pub chunk_size: u64,
    pub resource_subdir_rule: SubdirRule,
    pub forbidden_extensions: Vec<String>,
    pub instant_completion: bool,
    pub resource_redis_expire: u64,
    pub lax_mode: bool,
    pub x_accel_redirect: bool,
    pub extra_mime_types: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_php_config() {
        let config = Config::default();

        assert!(config.enable);
        assert!(!config.instant_completion);
        assert_eq!(config.resource_redis_expire, 604_800);
        assert_eq!(config.root_dir, "storage/app/aetherupload");
        assert_eq!(config.chunk_size, 1_000_000);
        assert_eq!(config.resource_subdir_rule, SubdirRule::Month);
        assert!(!config.lax_mode && !config.x_accel_redirect);
        assert_eq!(config.route_preprocess, "/aetherupload/preprocess");
        assert_eq!(config.route_uploading, "/aetherupload/uploading");
        assert_eq!(config.route_display, "/aetherupload/display");
        assert_eq!(config.route_download, "/aetherupload/download");
        assert_eq!(config.storage.driver, StorageDriver::Local);
        assert_eq!(config.storage.s3.multipart_threshold, 104_857_600);

        let default_group = config.groups.get("file").unwrap();
        assert_eq!(default_group.group_dir, "file");
        assert_eq!(default_group.resource_maxsize, 104_857_600);
        assert!(
            default_group
                .resource_extensions
                .contains(&"jpg".to_string())
        );
        assert!(config.forbidden_extensions.contains(&"php".to_string()));
    }

    #[test]
    fn resolve_group_snapshots_and_validates() {
        let config = Config::default();

        let snapshot = config.resolve_group("file").unwrap();
        assert_eq!(snapshot.group, "file");
        assert_eq!(snapshot.group_dir, "file");
        assert_eq!(snapshot.chunk_size, config.chunk_size);

        // 含下划线的分组名会让 savedPath 解码错位 —— 一律拒绝
        assert!(matches!(
            config.resolve_group("my_file"),
            Err(Error::InvalidOperation)
        ));
        // 未配置的分组
        assert!(matches!(
            config.resolve_group("video"),
            Err(Error::InvalidOperation)
        ));
        assert!(matches!(
            config.resolve_group(""),
            Err(Error::InvalidOperation)
        ));
    }

    #[test]
    fn subdir_rule_parsing_falls_back_to_month() {
        assert_eq!(SubdirRule::parse("year"), SubdirRule::Year);
        assert_eq!(SubdirRule::parse("date"), SubdirRule::Date);
        assert_eq!(SubdirRule::parse("const"), SubdirRule::Const);
        assert_eq!(SubdirRule::parse("month"), SubdirRule::Month);
        assert_eq!(SubdirRule::parse("whatever"), SubdirRule::Month);
    }
}
