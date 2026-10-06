// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! e-cat 集成：**e-cat 4 的 HTTP 传输层就是 axum `Router`**，复用 axum 适配层的同一套 handler。
//!
//! e-cat 的 HTTP 服务端（`ecat-transport-http` 的 `HttpServer`，源码里 `use axum::Router;`）
//! 构造时接收一个 axum `Router`；family 里 `ecat = ["axum"]` 也正是这个意思。因此本 crate
//! 不为 e-cat 引入额外依赖：四条路由仍由 [`crate::integrations::axum`] 注册，
//! [`routes`] 的返回值直接交给 `.router(...)` 即可。
//!
//! ```text
//! // e-cat 侧（宿主自行添加 ecat-transport-http 依赖）：
//! HttpServer::new("0.0.0.0:8080")
//!     .router(aetherupload::integrations::ecat::routes(runtime))
//!     .start()
//!     .await?;
//! ```
//!
//! e-cat 侧的自定义中间件（鉴权、限流等）按 axum 的 `layer` 语义叠在 [`routes`] 外层；
//! body 上限已经按 `chunk_size` 放开（见 [`crate::integrations::body_limit`]），
//! 不用再调 e-cat 的 body 限制。

use std::sync::Arc;

use axum::Router;

use crate::runtime::Runtime;

/// 注册四条路由（路径取自配置的 `route_*`），返回可直接交给 e-cat `HttpServer::router` 的
/// axum `Router`。
///
/// 实现即 [`crate::integrations::axum::routes`] —— e-cat 的 HTTP 层就是 axum，
/// 同一套 handler 两个框架共用。
pub fn routes(runtime: Arc<Runtime>) -> Router {
    crate::integrations::axum::routes(runtime)
}
