// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 原生请求守卫：**不依赖任何框架**的入口（对应 PHP 版的「原生 PHP（无框架）」适配器）。
//!
//! 八个框架适配器（Axum / Actix Web / Rocket / Poem / Salvo / Warp / bee-rust / e-cat）做的都是
//! 同一件事：把宿主请求翻成 [`FormData`]、把内核响应翻成宿主响应。这里把这两件事收成
//! 一个不依赖任何框架的 [`Guard`] —— 手写服务器（hyper / tiny-http / 自研 TCP 服务，
//! 或尚未适配的框架）拿它接线即可，与八个适配器同权。
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
//!         assert_eq!(response.status(), 404);          // 文件不存在
//!     }
//!     _ => unreachable!(),
//! }
//! # std::fs::remove_dir_all(&base).ok();
//! # Ok::<(), aetherupload::Error>(())
//! ```

use std::sync::Arc;

use crate::controller::{ResourceController, ResourceResponse, UploadController};
use crate::form::FormData;
use crate::runtime::Runtime;

/// 上传接口的响应：HTTP 200 + JSON，`body` 就是前端 `aetherupload-core.js` 读的那串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonBody {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl JsonBody {
    /// 与 PHP 版 `json($result)` 一致的响应形态。
    fn json(body: String) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body,
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
    /// 方法不匹配（例如对上传路由发 GET）同样返回 `None` —— 交回宿主按 404/405 处理，
    /// 与 PHP 版把方法交给路由层裁决一致。
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
        let result =
            UploadController::new(self.runtime.clone()).preprocess(&form.to_preprocess_request());

        JsonBody::json(result.to_json())
    }

    /// 分块写入：校验 → 追加 → 回写断点；末块走完整校验并落盘。
    pub fn save_chunk(&self, form: &FormData) -> JsonBody {
        let result =
            UploadController::new(self.runtime.clone()).save_chunk(&form.to_save_chunk_request());

        JsonBody::json(result.to_json())
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
        let base =
            std::env::temp_dir().join(format!("aetherupload-guard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let runtime = Arc::new(Runtime::new(Config::default(), &base).unwrap());

        (base, Guard::new(runtime))
    }

    #[test]
    fn routes_classify_the_four_endpoints() {
        let (base, guard) = guard("routes");
        let routes = guard.routes();

        assert_eq!(
            routes.classify("POST", "/aetherupload/preprocess"),
            Some(GuardRoute::Preprocess)
        );
        assert_eq!(
            routes.classify("POST", "/aetherupload/uploading"),
            Some(GuardRoute::Uploading)
        );
        assert_eq!(
            routes.classify("GET", "/aetherupload/display/file_202610_abc.txt"),
            Some(GuardRoute::Display {
                saved_path: "file_202610_abc.txt".into()
            })
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
            Some(GuardRoute::Display {
                saved_path: "file_202610_abc.txt".into()
            })
        );

        // 方法不匹配 / 无关路径 → 交回宿主
        assert_eq!(routes.classify("GET", "/aetherupload/preprocess"), None);
        assert_eq!(routes.classify("POST", "/aetherupload/display/x"), None);
        assert_eq!(routes.classify("GET", "/healthz"), None);
        // 没有 new_name 的下载路径也算下载（内核会用原名）
        assert_eq!(
            routes.classify("GET", "/aetherupload/download/file_202610_a.txt"),
            Some(GuardRoute::Download {
                saved_path: "file_202610_a.txt".into(),
                new_name: String::new()
            })
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preprocess_reports_missing_group_dir_like_php() {
        let (base, guard) = guard("preprocess");

        // 没建分组目录：预处理应当失败（内核不递归建目录），响应体是 JSON
        let mut form = FormData::new();
        form.push_text("resource_name", "a.txt");
        form.push_text("resource_size", "10");
        form.push_text("group", "file");
        form.push_text("locale", "zh");

        let response = guard.preprocess(&form);

        assert_eq!(
            response.status, 200,
            "上传接口一律 200，错误在 JSON 的 error 字段里"
        );
        assert_eq!(response.content_type, "application/json; charset=utf-8");
        assert!(
            response
                .body
                .contains("\"error\":\"错误：创建子文件夹失败\""),
            "{}",
            response.body
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn display_reports_404_for_missing_resource() {
        let (base, guard) = guard("display");

        assert_eq!(guard.display("file_202610_missing.txt").status(), 404);
        assert_eq!(guard.download("file_202610_missing.txt", "x").status(), 404);

        let _ = std::fs::remove_dir_all(&base);
    }
}
