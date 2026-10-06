// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Web 框架适配层：只做接线。
//!
//! 内核（[`crate::controller::UploadController`] / [`crate::controller::ResourceController`]）
//! 不感知任何框架；每个适配器负责三件事：**注册四条路由、把 multipart 表单翻译成入参、
//! 把内核的响应数据翻译成框架的响应对象**。业务逻辑一行都不重复。
//!
//! | feature | crate | 接线机制 |
//! |---------|-------|----------|
//! | `axum`  | axum 0.8 | `Router` + `Multipart` 提取器 |
//! | `actix` | actix-web 4 | `web::Scope` + `Multipart`（actix-multipart） |
//! | `rocket`| rocket 0.5 | `routes!` 宏 + `Form`/`TempFile`（multipart 表单） |
//! | `poem`  | poem 3 | `Route` + `Multipart` 提取器 |
//! | `salvo` | salvo 1 | `Router` + `FormData` 提取器 |
//! | `warp`  | warp 0.4 | `Filter` 组合 + `multipart` 过滤器 |
//! | `bee-rust` | bee_router 1 | axum 之上的路由封装（复用 axum 适配层） |
//! | `ecat`  | ecat 4 | e-cat 的 HTTP 传输即 axum `Router`（复用 axum 适配层） |
//!
//! 前端字段名与 PHP 版一致（`resource_name` / `resource_chunk` / `chunk_index` …），
//! 因此 PHP 版的前端脚本 `aetherupload-all.js` 不用改一行就能对着 Rust 端跑。

#[cfg(feature = "actix")]
pub mod actix;
#[cfg(feature = "axum")]
pub mod axum;
#[cfg(feature = "bee-rust")]
pub mod bee_rust;
#[cfg(feature = "ecat")]
pub mod ecat;
#[cfg(feature = "poem")]
pub mod poem;
#[cfg(feature = "rocket")]
pub mod rocket;
#[cfg(feature = "salvo")]
pub mod salvo;
#[cfg(feature = "warp")]
pub mod warp;

// 表单载体在内核里（原生 Rust 宿主也要用），适配器从这里取同一个类型
pub use crate::form::FormData;

/// 请求体上限的建议值：分块大小 + 1MB 的 multipart 头部与字段余量。
///
/// 各框架默认的 body 上限（axum 2MB、actix 256KB…）都小于一次上传的分块，
/// 不放开的话第一个分块就会被框架自己拒掉。适配器用这个值设置路由的 body 限制。
pub fn body_limit(runtime: &crate::runtime::Runtime) -> usize {
    (runtime.config().chunk_size as usize).saturating_add(1_048_576)
}

/// 路径模板：`/aetherupload/display/{saved_path}`。
pub fn display_route(runtime: &crate::runtime::Runtime) -> String {
    format!("{}/{{saved_path}}", runtime.config().route_display)
}

/// 路径模板：`/aetherupload/download/{saved_path}/{new_name}`。
pub fn download_route(runtime: &crate::runtime::Runtime) -> String {
    format!(
        "{}/{{saved_path}}/{{new_name}}",
        runtime.config().route_download
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_limit_leaves_room_for_multipart_overhead() {
        let config = crate::config::Config::default();
        let root =
            std::env::temp_dir().join(format!("aetherupload-formdata-{}", std::process::id()));
        let runtime = crate::runtime::Runtime::new(config, &root).unwrap();

        assert_eq!(body_limit(&runtime), 1_000_000 + 1_048_576);
        assert_eq!(
            display_route(&runtime),
            "/aetherupload/display/{saved_path}"
        );
        assert_eq!(
            download_route(&runtime),
            "/aetherupload/download/{saved_path}/{new_name}"
        );
    }
}
