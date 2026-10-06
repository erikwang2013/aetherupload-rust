// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! bee-rust（crate 名 `bee_router`）集成：**bee 的 HTTP 传输层就是 axum**，复用 axum 适配层的同一套 handler。
//!
//! `bee_router::Router` 是 axum `Router` 的声明式构建器：`ns` 分组与 `get`/`post` 注册的
//! 都是 axum handler，`build()` / `with_state()` 产出的就是 `axum::Router`（见
//! bee_router 1.2.3 的 `src/router.rs`）。因此这里不写第二套接线 —— 四条路由仍由
//! [`crate::integrations::axum`] 注册，用 `merge` 并进 bee 应用即可。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Runtime};
//!
//! async fn health() -> &'static str {
//!     "OK"
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = Arc::new(Runtime::new(Config::default(), ".")?);
//!
//! // bee 自己的路由 + 上传路由合并成同一个 axum Router
//! let app = bee_router::Router::new()
//!     .ns("/api/v1", |ns| ns.get("/health", health))
//!     .build()
//!     .merge(aetherupload::integrations::bee_rust::routes(runtime));
//! // axum::serve(listener, app).await
//! # let _ = app;
//! # Ok(())
//! # }
//! ```
//!
//! 与 bee 默认行为的两点关系：
//!
//! - `bee_router::Router::build()` 只给它自己注册的路由套 2 MiB body 上限 + 30 秒超时；
//!   `merge` 进来的四条路由带的是按 `chunk_size` 算出的上限
//!   （[`crate::integrations::body_limit`]），不受 bee 的 2 MiB 影响。
//! - bee 的 `Controller` / `Filter`（`Context` 请求管线）作用于 bee 自己注册的路由，
//!   与这四条 axum handler 无关；上传前要做鉴权，请用 axum 的 `route_layer`
//!   包在 [`routes`] 的产品外层。

use std::sync::Arc;

use axum::Router;

use crate::runtime::Runtime;

/// 注册四条路由（路径取自配置的 `route_*`），返回可直接 `merge` 进 bee 应用的 axum `Router`。
///
/// 实现即 [`crate::integrations::axum::routes`] —— bee 的 HTTP 层就是 axum，
/// 同一套 handler 两个框架共用。
pub fn routes(runtime: Arc<Runtime>) -> Router {
    crate::integrations::axum::routes(runtime)
}
