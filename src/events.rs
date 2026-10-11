// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传完成事件出口 —— 开关是分组配置项 `event_before_upload_complete` /
//! `event_upload_complete`（1.x 既有部署沿用同名配置键）。
//!
//! 事件出口做成 trait：宿主给自己的 sink 装上回调，同步执行，不需要注册表。
//! 触发时机（内核固定，见 [`crate::controller::UploadController`]）：
//!
//! - `before_upload_complete`：末块通过大小与 MIME 校验之后、整份 md5 比对之前
//!   —— 此时 `*.part` 还是临时文件，可读可改写；
//! - `upload_complete`：改名落盘、秒传索引写入之后 —— 拿到的是成品 [`Resource`]。
//!
//! 监听器抛错**不影响上传结果** —— 回调不参与成功/失败判定，它的失败只属于它自己。
//! Rust 侧按此隔离（[`crate::controller::UploadController`] 用 `catch_unwind` 包住回调），
//! panic 消息由标准 panic hook 打到 stderr。

use crate::partial::PartialResource;
use crate::resource::Resource;

/// 事件出口。默认实现 [`NoopEvents`] 什么都不做。
pub trait EventSink: Send + Sync {
    /// 上传即将完成：`partial` 是尚未改名的临时资源。
    fn before_upload_complete(&self, _partial: &PartialResource) {}

    /// 上传已完成：`resource` 是落地后的成品。
    fn upload_complete(&self, _resource: &Resource) {}
}

/// 默认事件出口：全部空实现（分组未开启事件时的行为）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEvents;

impl EventSink for NoopEvents {}
