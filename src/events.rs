// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传完成事件 —— 对应 PHP 版配置里的 `event_before_upload_complete` /
//! `event_upload_complete`（webman 下经 `config/event.php` 绑定处理类）。
//!
//! Rust 版把「事件处理类」换成 trait 实现：宿主给自己的 sink 装上回调，同步执行。
//! 触发时机与 PHP 完全一致：
//!
//! - `before_upload_complete`：末块通过大小与 MIME 校验之后、整份 md5 比对之前
//!   —— 此时 `*.part` 还是临时文件，可读可改写；
//! - `upload_complete`：改名落盘、秒传索引写入之后 —— 拿到的是成品 [`Resource`]。
//!
//! 事件里抛错等同于上传失败（与 PHP 一致：异常会被控制器的 catch 接住并清理临时文件）。

use crate::partial::PartialResource;
use crate::resource::Resource;

/// 事件出口。默认实现 [`NoopEvents`] 什么都不做。
pub trait EventSink: Send + Sync {
    /// 上传即将完成：`partial` 是尚未改名的临时资源。
    fn before_upload_complete(&self, _partial: &PartialResource) {}

    /// 上传已完成：`resource` 是落地后的成品。
    fn upload_complete(&self, _resource: &Resource) {}
}

/// 默认事件出口：全部空实现（对应 PHP 未配置事件时的行为）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEvents;

impl EventSink for NoopEvents {}
