// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 原生请求守卫：**不依赖任何框架**的入口（手写服务器、尚未适配的框架都用它）。
//!
//! 八个框架适配器（Axum / Actix Web / Rocket / Poem / Salvo / Warp / bee-rust / e-cat）做的都是
//! 同一件事：把宿主请求翻成 [`FormData`]、把内核响应翻成宿主响应。这里把这两件事收成
//! 一个不依赖任何框架的 [`Guard`] —— 手写服务器（hyper / tiny-http / 自研 TCP 服务）
//! 拿它接线即可，与八个适配器同权。
//!
//! ```
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Guard, Runtime};
//!
//! # let base = std::env::temp_dir().join(format!("aetherupload-guard-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(base.join("storage/app/aetherupload/file"))?;
//! # std::fs::create_dir_all(base.join("storage/app/aetherupload/_header"))?;
//! let runtime = Arc::new(Runtime::new(Config::default(), &base)?);
//! let guard = Guard::new(runtime);
//!
//! // 手写服务器拿到一条请求后：先问「这是哪条路由」
//! match guard.routes().classify("GET", "/aetherupload/display/file_202610_abc.jpg") {
//!     Some(aetherupload::GuardRoute::Display { saved_path }) => {
//!         let response = guard.display(&saved_path);   // ResourceResponse，含状态码与响应头
//!         assert_eq!(response.status(), 404);          // 文件不存在：404 问题响应
//!     }
//!     _ => unreachable!(),
//! }
//! # std::fs::remove_dir_all(&base).ok();
//! # Ok::<(), aetherupload::Error>(())
//! ```

use std::sync::Arc;

use crate::controller::{
    ResourceController, ResourceResponse, UploadController, save_chunk_ok_to_json, upload_ok_to_json,
};
use crate::form::FormData;
use crate::json::Problem;
use crate::runtime::Runtime;

/// 上传接口的响应：HTTP 200 + 上传结果，或问题响应（RFC 9457 problem+json）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonBody {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl JsonBody {
    /// 上传结果（200 + `application/json`），`body` 就是前端 `aetherupload-core.js` 读的那串。
    pub fn ok(body: String) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body,
        }
    }

    /// 问题响应：状态码与 content-type 都取自 [`Problem`]（宿主不许改写）。
    ///
    /// 宿主自己挡下的请求（multipart / urlencoded 读不动、缺 `Content-Type`）也用同一
    /// 形态回：`JsonBody::problem(&Problem::invalid_request_body(locale))`。
    pub fn problem(problem: &Problem) -> Self {
        Self {
            status: problem.status(),
            content_type: problem.content_type(),
            body: problem.to_json(),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.body.into_bytes()
    }
}

/// 四条路由的路径（宿主自己分发时用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRoutes {
    pub preprocess: String,
    pub uploading: String,
    pub display_prefix: String,
    pub download_prefix: String,
}

/// `classify()` 的判定结果：这条请求该走哪个入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardRoute {
    /// `POST` 预处理
    Preprocess,
    /// `POST` 分块写入
    Uploading,
    /// `GET` 展示（路径参数已剥掉前缀）
    Display { saved_path: String },
    /// `GET` 下载（路径参数已剥掉前缀；`new_name` 可能为空串）
    Download {
        saved_path: String,
        new_name: String,
    },
}

impl GuardRoutes {
    /// 判定一条请求属于哪个入口；不属于这四条路由时返回 `None`。
    ///
    /// 方法不匹配（例如对上传路由发 GET）同样返回 `None` —— 交回宿主按 404/405 处理。
    pub fn classify(&self, method: &str, path: &str) -> Option<GuardRoute> {
        let method = method.to_ascii_uppercase();
        let path = path.split(['?', '#']).next().unwrap_or(path);

        match (method.as_str(), path) {
            ("POST", p) if p == self.preprocess => Some(GuardRoute::Preprocess),
            ("POST", p) if p == self.uploading => Some(GuardRoute::Uploading),
            ("GET", p) => {
                if let Some(rest) = p
                    .strip_prefix(&self.display_prefix)
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    return Some(GuardRoute::Display {
                        saved_path: rest.to_string(),
                    });
                }

                if let Some(rest) = p
                    .strip_prefix(&self.download_prefix)
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    let (saved_path, new_name) = match rest.split_once('/') {
                        Some((saved_path, new_name)) => (saved_path, new_name),
                        None => (rest, ""),
                    };

                    return Some(GuardRoute::Download {
                        saved_path: saved_path.to_string(),
                        new_name: new_name.to_string(),
                    });
                }

                None
            }
            _ => None,
        }
    }
}

/// 原生请求守卫：四个入口 + 一张路由表。
///
/// 与八个框架适配器同权 —— 它们内部也是调这四个入口，只是把参数从宿主请求里取出来。
/// `Guard` 只持有 `Arc<Runtime>`，克隆是两次原子计数，`Send + Sync`，适合每请求克隆。
#[derive(Debug, Clone)]
pub struct Guard {
    runtime: Arc<Runtime>,
}

impl Guard {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// 四条路由的路径与判定表（宿主自己分发时用）。
    pub fn routes(&self) -> GuardRoutes {
        let config = self.runtime.config();

        GuardRoutes {
            preprocess: config.route_preprocess.clone(),
            uploading: config.route_uploading.clone(),
            display_prefix: config.route_display.clone(),
            download_prefix: config.route_download.clone(),
        }
    }

    /// 预处理：校验参数与分组 → 生成临时名 → 秒传判定 → 建 `.part` 与断点文件。
    pub fn preprocess(&self, form: &FormData) -> JsonBody {
        let controller = UploadController::new(self.runtime.clone());

        match controller.preprocess(&form.to_preprocess_request()) {
            Ok(ok) => JsonBody::ok(upload_ok_to_json(&ok)),
            Err(problem) => JsonBody::problem(&problem),
        }
    }

    /// 分块写入：校验 → 追加 → 回写断点；末块走完整校验并落盘。
    pub fn save_chunk(&self, form: &FormData) -> JsonBody {
        let controller = UploadController::new(self.runtime.clone());

        match controller.save_chunk(&form.to_save_chunk_request()) {
            Ok(ok) => JsonBody::ok(save_chunk_ok_to_json(&ok)),
            Err(problem) => JsonBody::problem(&problem),
        }
    }

    /// 展示资源（可内联渲染的扩展名会强制转附件）。
    pub fn display(&self, saved_path: &str) -> ResourceResponse {
        ResourceController::new(self.runtime.clone()).display(saved_path)
    }

    /// 下载资源（可改文件名，扩展名保持原样）。
    pub fn download(&self, saved_path: &str, new_name: &str) -> ResourceResponse {
        ResourceController::new(self.runtime.clone()).download(saved_path, new_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn guard(tag: &str) -> (std::path::PathBuf, Guard) {
        let base = std::env::temp_dir().join(format!("aetherupload-guard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let runtime = Arc::new(Runtime::new(Config::default(), &base).unwrap());

        (base, Guard::new(runtime))
    }

    fn text_form(pre: bool) -> FormData {
        let mut form = FormData::new();
        form.push_text("resource_name", "a.txt");
        form.push_text("resource_size", "10");
        form.push_text("group", "file");
        form.push_text("locale", "zh");

        if !pre {
            form.push_text("chunk_total", "1");
            form.push_text("chunk_index", "1");
            form.push_text("resource_temp_basename", "0011223344556677");
            form.push_text("resource_ext", "txt");
            form.push_text("group_subdir", "202610");
        }

        form
    }

    #[test]
    fn routes_classify_the_four_endpoints() {
        let (base, guard) = guard("routes");
        let routes = guard.routes();

        assert_eq!(routes.classify("POST", "/aetherupload/preprocess"), Some(GuardRoute::Preprocess));
        assert_eq!(routes.classify("POST", "/aetherupload/uploading"), Some(GuardRoute::Uploading));
        assert_eq!(
            routes.classify("GET", "/aetherupload/display/file_202610_abc.txt"),
            Some(GuardRoute::Display { saved_path: "file_202610_abc.txt".into() })
        );
        assert_eq!(
            routes.classify("GET", "/aetherupload/download/file_202610_abc.txt/新名字"),
            Some(GuardRoute::Download {
                saved_path: "file_202610_abc.txt".into(),
                new_name: "新名字".into()
            })
        );

        // 查询串与 Fragment 不影响判定
        assert_eq!(
            routes.classify("GET", "/aetherupload/display/file_202610_abc.txt?x=1#y"),
            Some(GuardRoute::Display { saved_path: "file_202610_abc.txt".into() })
        );

        // 方法不匹配 / 无关路径 → 交回宿主
        assert_eq!(routes.classify("GET", "/aetherupload/preprocess"), None);
        assert_eq!(routes.classify("POST", "/aetherupload/display/x"), None);
        assert_eq!(routes.classify("GET", "/healthz"), None);

        // 没有 new_name 的下载路径也算下载（内核用空串 → 400 invalid_parameter）
        assert_eq!(
            routes.classify("GET", "/aetherupload/download/file_202610_a.txt"),
            Some(GuardRoute::Download { saved_path: "file_202610_a.txt".into(), new_name: String::new() })
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 成功的 preprocess 是 200 上传结果（snake_case），不再是 200 + error 字段。
    #[test]
    fn preprocess_success_is_200_upload_result() {
        let (base, guard) = guard("preprocess-ok");
        std::fs::create_dir_all(base.join("storage/app/aetherupload/file")).unwrap();
        std::fs::create_dir_all(base.join("storage/app/aetherupload/_header")).unwrap();

        let response = guard.preprocess(&text_form(true));

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json; charset=utf-8");
        assert!(response.body.contains(r#""chunk_size":1000000"#), "{}", response.body);
        assert!(response.body.contains(r#""saved_path":""#), "{}", response.body);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 分组目录缺失是运维/内部故障 → 500 问题响应（`title` 用请求里的语种）。
    #[test]
    fn preprocess_failure_is_a_problem_response() {
        let (base, guard) = guard("preprocess");

        let response = guard.preprocess(&text_form(true));

        assert_eq!(response.status, 500);
        assert_eq!(response.content_type, "application/problem+json");
        assert!(response.body.contains(r#""code":"internal_error""#), "{}", response.body);
        assert!(response.body.contains(r#""title":"错误：服务器内部错误""#), "{}", response.body);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 适配层用的便利构造：宿主自己挡下的请求也回同一形态，不许自造文案。
    #[test]
    fn problem_bodies_use_the_frozen_constructors() {
        let body = JsonBody::problem(&Problem::invalid_request_body(Some("en")));
        assert_eq!(body.status, 400);
        assert_eq!(body.content_type, "application/problem+json");
        assert!(body.body.contains(r#""code":"invalid_request_body""#));

        let body = JsonBody::problem(&Problem::payload_too_large(None));
        assert_eq!(body.status, 413);

        let body = JsonBody::problem(&Problem::invalid_parameter(Some("en")));
        assert_eq!(body.status, 400);
        assert!(body.body.contains(r#""code":"invalid_parameter""#));

        assert_eq!(JsonBody::problem(&Problem::resource_not_found(None)).status, 404);
    }

    #[test]
    fn file_endpoints_report_problem_responses() {
        let (base, guard) = guard("display");

        // 文件不存在 → 404 问题响应；下载名缺失（空串）→ 400
        let response = guard.display("file_202610_missing.txt");
        assert_eq!(response.status(), 404);
        assert!(matches!(response, ResourceResponse::Problem(_)));

        let response = guard.download("file_202610_missing.txt", "");
        assert_eq!(response.status(), 400);

        let _ = std::fs::remove_dir_all(&base);
    }
}
