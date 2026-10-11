// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Web 框架适配层：只做接线。
//!
//! 内核（[`crate::controller::UploadController`] / [`crate::controller::ResourceController`]）
//! 不感知任何框架；每个适配器负责三件事：**注册四条路由、把请求体（urlencoded / multipart）
//! 翻译成入参、把内核的响应翻译成框架的响应对象**。业务逻辑一行都不重复。
//!
//! | feature | crate | 接线机制 |
//! |---------|-------|----------|
//! | `axum`  | axum 0.8 | `Router` + `Multipart` 提取器 |
//! | `actix` | actix-web 4 | `web::Scope` + `Multipart`（actix-multipart） |
//! | `rocket`| rocket 0.5 | `Route::new` 手工注册 + `Form`/`TempFile`（multipart 表单） |
//! | `poem`  | poem 3 | `Route` + `Multipart` 提取器 |
//! | `salvo` | salvo 1 | `Router` + `FormData` 提取器 |
//! | `warp`  | warp 0.4 | `Filter` 组合 + `multipart` 过滤器 |
//! | `bee-rust` | bee_router 1 | axum 之上的路由封装（复用 axum 适配层） |
//! | `ecat`  | ecat 4 | e-cat 的 HTTP 传输即 axum `Router`（复用 axum 适配层） |
//!
//! 响应形态（2.0 协议）：两个上传入口成功是 200 + `application/json`（snake_case
//! 上传结果）；失败一律 RFC 9457 问题响应 —— 内核的 [`Problem`] 经 [`problem_response`]
//! 原样映射（状态码不吞成 200、文案不替换），适配层自身的解析失败走
//! [`invalid_request_body_response`]，框架层替内核报的参数错误走
//! [`invalid_parameter_response`]，单块超过 [`body_limit`] 走 [`payload_too_large_response`]。
//!
//! 前端字段名与 1.x 一致（`resource_name` / `resource_chunk` / `chunk_index` …）。
//!
//! 表单编码两种都收：`preprocess` 认 urlencoded（1.x 既有部署的编码）与 multipart
//! —— 库里自带的脚本（[`crate::assets`]）发 multipart，两种都翻成同一个 [`FormData`]；
//! 分块路由只可能是 multipart。

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

use crate::json::Problem;

/// 问题响应三元组：`(HTTP 状态码, Content-Type, 响应体)`。
///
/// 八家适配器用它把内核的 [`Problem`] 就地翻成各自的响应类型 —— 状态码原样、
/// `application/problem+json` 体、文案不动（本地化在内核一处完成）。
pub fn problem_response(problem: &Problem) -> (u16, &'static str, String) {
    (problem.status(), problem.content_type(), problem.to_json())
}

/// 适配层自身解析失败（multipart / urlencoded 坏体、缺 `Content-Type`）的问题响应：
/// 400 `invalid_request_body`。`locale` 能从已解析出的字段里读到就传，否则传 `None`。
pub fn invalid_request_body_response(locale: Option<&str>) -> (u16, &'static str, String) {
    problem_response(&Problem::invalid_request_body(locale))
}

/// 框架层在提取期就替内核判出「参数缺失 / 非法」时的问题响应：400 `invalid_parameter`
/// （与内核自身参数校验同一 code、同一张 i18n 表）。
pub fn invalid_parameter_response(locale: Option<&str>) -> (u16, &'static str, String) {
    problem_response(&Problem::invalid_parameter(locale))
}

/// 请求体超过 [`body_limit`]、或框架自己判定的 413 的问题响应：413 `payload_too_large`。
pub fn payload_too_large_response(locale: Option<&str>) -> (u16, &'static str, String) {
    problem_response(&Problem::payload_too_large(locale))
}

/// 文件端点打开文件失败的问题响应：404 `resource_not_found`（display / download 路径
/// 没有 `locale` 参数，标题用默认语种）。
pub fn resource_not_found_response() -> (u16, &'static str, String) {
    problem_response(&Problem::resource_not_found(None))
}

/// 成功体三元组：200 + `application/json`（上传结果，见协议 §2）。
pub fn ok_response(body: String) -> (u16, &'static str, String) {
    (200, "application/json", body)
}

/// 适配层解析 multipart / urlencoded 失败的载体。
///
/// 失败常发生在读到一半（比如分块字段后面还有 `locale`）：带着已解析出的字段回来，
/// 问题标题就能按请求的 `locale` 本地化。`over_limit` 为真表示框架报的是请求体超限
/// （413 [`payload_too_large_response`]），否则是坏体（400 [`invalid_request_body_response`]）。
pub struct ParseFailure {
    form: FormData,
    over_limit: bool,
}

impl ParseFailure {
    pub fn new(form: FormData, over_limit: bool) -> Self {
        Self { form, over_limit }
    }

    /// 该失败对应的三元组。
    pub fn response(&self) -> (u16, &'static str, String) {
        let locale = self.form.text("locale");

        if self.over_limit {
            payload_too_large_response(locale)
        } else {
            invalid_request_body_response(locale)
        }
    }
}

/// 请求体上限的建议值：分块大小 + 1MB 的 multipart 头部与字段余量。
///
/// 各框架默认的 body 上限（axum 2MB、actix 256KB…）都小于一次上传的分块，
/// 不放开的话第一个分块就会被框架自己拒掉。适配器用这个值设置路由的 body 限制，
/// 超限统一回 [`payload_too_large_response`]。
pub fn body_limit(runtime: &crate::runtime::Runtime) -> usize {
    (runtime.config().chunk_size as usize).saturating_add(1_048_576)
}

/// `Content-Type` 是否是 urlencoded 表单（`; charset=…` 这类参数忽略）。
///
/// `preprocess` 要按它分流：1.x 既有客户端（zepto / jQuery 的默认编码）发 urlencoded，
/// 自带脚本与手工构造的请求发 multipart（分块上传则一律是 multipart）。
pub fn is_urlencoded(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("application/x-www-form-urlencoded")
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
