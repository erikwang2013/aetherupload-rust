// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! # AetherUpload for Rust
//!
//! ```text
//!             ▲
//!             │              以太兽 · Aether Beast
//!         .-~~~-.
//!       .'  ● ●  '.          头顶箭头 = 整份文件（向上）
//!      /    ‿‿‿    \         腹部进度条 = 已追加的分块
//!     (  ▓▓▓▓░░░░░  )        浅色段 = 还没到的那几块
//!      '.__.___.__.'    ▣    右侧方块 = 正飞来的下一个分块
//! ```
//!
//! 上面是项目宠物「以太兽 · Aether Beast」的 ASCII 版，它就住在 [`pet`] 模块里：
//! 形象文件 `docs/pet.svg` 以常量内嵌（[`pet::SVG`]），`cargo run --example pet`
//! 是同款示例 —— 示例页的 favicon 与标题图标、四张架构图里的吉祥物、CLI 的
//! `aetherupload pet` 用的都是这一份形象。
//!
//! 浏览器把大文件切片，逐块追加到服务端的一个临时文件，落盘时用文件内容的 md5 命名。
//! **上传、断线续传、秒传、去重、完整性校验共用同一套机制** —— 全程不把整个文件读进
//! 内存，也不需要为「文件在哪」维护一张数据库表。
//!
//! 本 crate 是 PHP 包 [aetherupload-webman](https://github.com/erikwang2013/aetherupload-webman)
//! 的 Rust 移植：内核（分块、续传、秒传、校验、寻址）与宿主框架解耦，同一份代码在
//! Axum、Actix Web、Rocket、Poem、Salvo、Warp、bee-rust、e-cat 下可用（feature 门控，
//! 见 [`integrations`]）。协议与 PHP 版逐字段一致，前端 `aetherupload-all.js` 不用改。
//!
//! ## 三个入口
//!
//! - [`UploadController::preprocess`]：预处理（校验、生成临时名、秒传判定、建 `.part`）
//! - [`UploadController::save_chunk`]：分块写入（校验 → 追加 → 回写断点；末块完整校验落盘）
//! - [`ResourceController::display`] / [`ResourceController::download`]：展示与下载
//!
//! ## 快速开始
//!
//! ```
//! use aetherupload::{Config, PreprocessRequest, Runtime, UploadController};
//! use std::sync::Arc;
//!
//! # let base = std::env::temp_dir().join(format!("aetherupload-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(base.join("storage/app/aetherupload/file"))?;
//! # std::fs::create_dir_all(base.join("storage/app/aetherupload/_header"))?;
//! let runtime = Arc::new(Runtime::new(Config::default(), &base)?);
//! let uploader = UploadController::new(runtime);
//!
//! // 1) 预处理：拿到分块大小与临时名
//! let pre = uploader.preprocess(&PreprocessRequest {
//!     resource_name: Some("photo.jpg".into()),
//!     resource_size: Some("2048".into()),
//!     group: Some("file".into()),
//!     resource_hash: None,
//!     locale: Some("zh".into()),
//! });
//! assert!(pre.error.is_none(), "预处理应当成功");
//! assert_eq!(pre.chunk_size, 1_000_000);
//!
//! # std::fs::remove_dir_all(&base).ok();
//! # Ok::<(), aetherupload::Error>(())
//! ```
//!
//! ## 设计取向
//!
//! - **默认 feature 零第三方依赖**：MD5（RFC 1321 向量锚定）、JSON 输出、目录地址
//!   解析都是内置实现；Redis 客户端与 S3 传输都以 trait 注入。框架 feature 只带各自
//!   的框架依赖。
//! - **同步内核**：与 PHP 语义一致，`cargo test` 不需要运行时。大文件末块要算整份
//!   md5，宿主若在意 worker 占用，把调用放进 `tokio::task::spawn_blocking` 即可。
//! - **没有全局状态**：配置、分组快照、语种全部显式传参；常驻进程与协程下天然不串组。

pub mod assets;
pub mod config;
pub mod console;
pub mod controller;
pub mod error;
pub mod events;
pub mod form;
pub mod guard;
pub mod header;
pub mod i18n;
pub mod instant;
pub mod json;
pub mod md5;
pub mod mime;
pub mod partial;
pub mod pet;
pub mod resource;
pub mod runtime;
pub mod saved_path;
pub mod storage;
pub mod util;

#[cfg(any(
    feature = "axum",
    feature = "actix",
    feature = "rocket",
    feature = "poem",
    feature = "salvo",
    feature = "warp",
    feature = "bee-rust",
    feature = "ecat"
))]
pub mod integrations;

pub use config::{
    Config, GroupConfig, GroupSnapshot, PayloadSigning, S3Config, StorageDriver, SubdirRule,
};
pub use controller::{
    ChunkBody, PreprocessRequest, ResourceController, ResourceResponse, SaveChunkRequest,
    UploadController,
};
pub use error::{Error, Result};
pub use events::{EventSink, NoopEvents};
pub use form::FormData;
pub use guard::{Guard, GuardRoute, GuardRoutes, JsonBody};
pub use header::Header;
pub use instant::{InstantIndex, InstantStore, MemoryInstantStore, NullInstantStore};
pub use json::{PreprocessResult, SaveChunkResult};
pub use md5::{Md5, md5_file, md5_hex};
pub use mime::{MagicBytesDetector, MimeDetector};
pub use partial::{ChunkSource, PartialResource};
pub use resource::Resource;
pub use runtime::Runtime;
pub use saved_path::SavedPath;
pub use storage::{LocalStorage, Storage};

#[cfg(feature = "s3")]
pub use storage::s3::{HttpRequest, HttpResponse, HttpTransport, NullTransport, S3Storage};
