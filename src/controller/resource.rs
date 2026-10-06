// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 展示与下载入口 —— 对应 PHP 版 `ResourceController.php`。
//!
//! 三条读路径（顺序与 PHP 完全一致）：
//!
//! 1. **s3 驱动**：302 到预签名 URL（`x_accel_redirect` 是本地 nginx 概念，s3 下不适用）；
//! 2. **`x_accel_redirect = true`**：返回内部重定向路径交给前置服务器直发
//!    （只由服务端已校验的数据拼出，不含客户端原始输入）；
//! 3. **默认**：交回本地文件路径，由宿主框架的文件响应下发。
//!
//! 一条安全语义：**可内联渲染的扩展名（svg/html/js/…）强制转附件**，防止上传的
//! SVG/HTML 在同源下变成 XSS；无论走哪条路径都带 `X-Content-Type-Options: nosniff`。

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Result;
use crate::resource::Resource;
use crate::runtime::Runtime;
use crate::saved_path::SavedPath;
use crate::util::{extension_of, get_file_name, sanitize_download_name};

/// 可内联渲染、需强制转附件的扩展名（与 PHP 常量逐项一致）。
const INLINE_BLOCKED_EXTENSIONS: &[&str] = &[
    "svg", "svgz", "html", "htm", "xml", "xhtml", "xht", "xsl", "js", "mjs",
];

/// `x_accel_redirect` 约定使用的内部前缀，需与宿主配置里的 location 一致。
pub const ACCEL_PREFIX: &str = "/internal-aetherupload/";

/// 读路径的响应数据。适配器据此构造宿主框架的响应对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceResponse {
    /// 400/404 级别的失败：`display fail` / `download fail`（PHP 原文）。
    NotFound(&'static str),
    /// 302 到预签名 URL。
    Redirect { location: String },
    /// 内部重定向：宿主把 `X-Accel-Redirect`（或等价机制）透给前置服务器。
    AccelRedirect {
        path: String,
        content_disposition: Option<String>,
    },
    /// 本地文件：`download_name` 为 `Some` 时按附件下发。
    ServeFile {
        path: PathBuf,
        download_name: Option<String>,
    },
}

impl ResourceResponse {
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Redirect { .. } => 302,
            Self::AccelRedirect { .. } | Self::ServeFile { .. } => 200,
        }
    }

    /// 必须附加的响应头。适配器把这份清单并进自己的响应即可。
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("X-Content-Type-Options".to_string(), "nosniff".to_string())];

        match self {
            Self::Redirect { location } => {
                headers.push(("Location".to_string(), location.clone()));
            }
            Self::AccelRedirect {
                path,
                content_disposition,
            } => {
                headers.push(("X-Accel-Redirect".to_string(), path.clone()));
                if let Some(disposition) = content_disposition {
                    headers.push(("Content-Disposition".to_string(), disposition.clone()));
                }
            }
            Self::NotFound(_) | Self::ServeFile { .. } => {}
        }

        headers
    }
}

pub struct ResourceController {
    runtime: Arc<Runtime>,
}

impl ResourceController {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// 展示资源（内联预览；被拉黑的扩展名会转成附件下载）。
    pub fn display(&self, uri: &str) -> ResourceResponse {
        let Ok(resource) = self.locate(uri) else {
            return ResourceResponse::NotFound("display fail");
        };

        let inline_blocked = is_inline_blocked(&resource.name);

        let response_params = if inline_blocked {
            vec![(
                "response-content-disposition".to_string(),
                attachment_disposition(&resource.name),
            )]
        } else {
            Vec::new()
        };

        self.respond(&resource, inline_blocked, response_params, "display fail")
    }

    /// 下载资源（可改文件名，扩展名仍是原资源的）。
    pub fn download(&self, uri: &str, new_name: &str) -> ResourceResponse {
        let Ok(resource) = self.locate(uri) else {
            return ResourceResponse::NotFound("download fail");
        };

        // 客户端可控的下载名：控制字符/引号/斜杠一律替换，防头注入
        let sanitized = sanitize_download_name(new_name);
        let download_name = get_file_name(&sanitized, &extension_of(&resource.name));

        let response_params = vec![(
            "response-content-disposition".to_string(),
            attachment_disposition(&download_name),
        )];

        match self.respond(&resource, true, response_params, "download fail") {
            ResourceResponse::ServeFile { path, .. } => ResourceResponse::ServeFile {
                path,
                download_name: Some(download_name),
            },
            other => other,
        }
    }

    /// 三段路径选择：s3 → 加速重定向 → 本地文件。
    fn respond(
        &self,
        resource: &Resource,
        inline_blocked: bool,
        response_params: Vec<(String, String)>,
        failure: &'static str,
    ) -> ResourceResponse {
        let url = match resource.url(self.runtime.storage(), &response_params) {
            Ok(url) => url,
            Err(_) => return ResourceResponse::NotFound(failure),
        };

        if let Some(location) = url {
            return ResourceResponse::Redirect { location };
        }

        if self.runtime.config().x_accel_redirect {
            // 只由服务端已校验的数据拼出；前缀与宿主 nginx 的 alias 对齐
            let path = format!(
                "{}{}/{}/{}",
                ACCEL_PREFIX, resource.group_dir, resource.group_sub_dir, resource.name
            );

            return ResourceResponse::AccelRedirect {
                path,
                content_disposition: inline_blocked.then(|| attachment_disposition(&resource.name)),
            };
        }

        ResourceResponse::ServeFile {
            path: resource.path.clone(),
            download_name: inline_blocked.then(|| resource.name.clone()),
        }
    }

    /// 寻址 + 存在性检查（PHP 的 try/catch 在此等价于 `Result`）。
    fn locate(&self, uri: &str) -> Result<Resource> {
        let params = SavedPath::decode(uri)?;
        let snapshot = self.runtime.config().resolve_group(&params.group)?;

        let resource = Resource::new(
            self.runtime.upload_root(),
            &params.group,
            &snapshot.group_dir,
            &params.group_sub_dir,
            &params.resource_name,
        );

        if !resource.exists(self.runtime.storage())? {
            return Err(crate::error::Error::InvalidOperation);
        }

        Ok(resource)
    }
}

fn is_inline_blocked(name: &str) -> bool {
    let extension = extension_of(name);

    INLINE_BLOCKED_EXTENSIONS
        .iter()
        .any(|blocked| *blocked == extension)
}

/// `Content-Disposition`（含 RFC 5987 双写法）—— 与 PHP `attachmentDisposition()` 一致：
/// ASCII 回退 + `filename*=UTF-8''…`。`name` 已消毒（无控制字符）。
///
/// 注意：`filename="…"` 那一份与 PHP 一样**保留原始 UTF-8 字节**（HTTP/1.1 的 obs-text），
/// 严格说不是纯 ASCII。现代浏览器优先读 `filename*`，行为与 PHP 版逐字节一致；
/// 适配器若需要用 `to_str()` 读这个头，请改用 `as_bytes()`。
pub fn attachment_disposition(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '"' || c == '\\' {
            escaped.push('\\');
        }
        escaped.push(c);
    }

    format!(
        "attachment; filename=\"{escaped}\"; filename*=UTF-8''{}",
        percent_encode(name)
    )
}

/// `rawurlencode`：除 `A-Za-z0-9-_.~` 外全部按 UTF-8 字节百分号编码（大写十六进制）。
fn percent_encode(value: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";

    let mut out = String::with_capacity(value.len());

    for byte in value.as_bytes() {
        if UNRESERVED.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn runtime(tag: &str) -> (PathBuf, Arc<Runtime>) {
        let root =
            std::env::temp_dir().join(format!("aetherupload-resctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // 成品落在 upload_root（= base_path/root_dir）下
        let group_dir = root.join("storage/app/aetherupload/file/202610");
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(group_dir.join("abc.jpg"), b"jpeg-bytes").unwrap();
        std::fs::write(group_dir.join("evil.svg"), b"<svg/>").unwrap();

        let runtime = Runtime::new(Config::default(), &root).unwrap();
        (root, Arc::new(runtime))
    }

    #[test]
    fn display_serves_local_file_with_nosniff() {
        let (root, runtime) = runtime("display");
        let controller = ResourceController::new(runtime);

        let response = controller.display("file_202610_abc.jpg");
        assert_eq!(response.status(), 200);
        assert!(matches!(response, ResourceResponse::ServeFile { .. }));
        assert!(
            response
                .headers()
                .contains(&("X-Content-Type-Options".to_string(), "nosniff".to_string()))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// svg/html/js 这类可内联渲染的扩展名必须转附件，否则同源 XSS。
    #[test]
    fn display_forces_attachment_for_inline_blocked_types() {
        let (root, runtime) = runtime("inline");
        let controller = ResourceController::new(runtime);

        match controller.display("file_202610_evil.svg") {
            ResourceResponse::ServeFile { download_name, .. } => {
                assert_eq!(download_name.as_deref(), Some("evil.svg"));
            }
            other => panic!("预期本地文件响应，实际 {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accel_redirect_mode_returns_internal_path() {
        let (root, _) = runtime("accel");
        let config = Config {
            x_accel_redirect: true,
            ..Config::default()
        };
        let runtime = Arc::new(Runtime::new(config, &root).unwrap());
        let controller = ResourceController::new(runtime);

        match controller.display("file_202610_abc.jpg") {
            ResourceResponse::AccelRedirect {
                path,
                content_disposition,
            } => {
                assert_eq!(path, "/internal-aetherupload/file/202610/abc.jpg");
                assert!(content_disposition.is_none());
            }
            other => panic!("预期加速重定向，实际 {other:?}"),
        }

        // 被拉黑类型带上附件头
        match controller.display("file_202610_evil.svg") {
            ResourceResponse::AccelRedirect {
                content_disposition,
                ..
            } => assert!(content_disposition.unwrap().starts_with("attachment;")),
            other => panic!("预期加速重定向，实际 {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn download_sanitizes_name_and_keeps_extension() {
        let (root, runtime) = runtime("download");
        let controller = ResourceController::new(runtime);

        match controller.download("file_202610_abc.jpg", "我的 \"报告\"/v2") {
            ResourceResponse::ServeFile { download_name, .. } => {
                assert_eq!(download_name.as_deref(), Some("我的 _报告__v2.jpg"));
            }
            other => panic!("预期本地文件响应，实际 {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failures_are_404_text() {
        let (root, runtime) = runtime("fail");
        let controller = ResourceController::new(runtime);

        assert_eq!(
            controller.display("file_202610_missing.jpg"),
            ResourceResponse::NotFound("display fail")
        );
        assert_eq!(
            controller.download("bad_path", "x.jpg"),
            ResourceResponse::NotFound("download fail")
        );
        assert_eq!(controller.display("nope_202610_x.jpg").status(), 404);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disposition_matches_php_format() {
        assert_eq!(
            attachment_disposition("报告.pdf"),
            "attachment; filename=\"报告.pdf\"; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf"
        );
        assert!(attachment_disposition("a\"b\\c").contains("a\\\"b\\\\c"));
    }
}
