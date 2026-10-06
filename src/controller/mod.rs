// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 两个入口控制器 —— 对应 PHP 版 `UploadController.php` 与 `ResourceController.php`。
//!
//! 内核里只有这两个「用例层」：其余模块都是它们与适配器共用的零件。

pub mod resource;
pub mod upload;

pub use resource::{ACCEL_PREFIX, ResourceController, ResourceResponse, attachment_disposition};
pub use upload::{ChunkBody, PreprocessRequest, SaveChunkRequest, UploadController};
