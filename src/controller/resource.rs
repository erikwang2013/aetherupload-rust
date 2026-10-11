// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 展示与下载入口 —— 内核的读路径。
//!
//! 三条成功路径（顺序与 1.x 完全一致）：
//!
//! 1. **s3 驱动**：302 到预签名 URL（`x_accel_redirect` 是本地 nginx 概念，s3 下不适用）；
//! 2. **`x_accel_redirect = true`**：返回内部重定向路径交给前置服务器直发
//!    （只由服务端已校验的数据拼出，不含客户端原始输入）；
//! 3. **默认**：交回本地文件路径，由宿主框架的文件响应下发。
//!
//! 失败一律是 [`ResourceResponse::Problem`]，且 2.0 把 1.x 折成一句 404 文本的几种
//! 情况拆开：寻址串解码失败 → 400 `invalid_parameter`；**分组未配置/不可解析 → 404
//! `resource_not_found`**（标识符合法但指不到资源，顺带不外泄分组配置）；文件不存在
//! → 404 `resource_not_found`；**存储/IO 故障 → 500 `internal_error`**（读不了盘不能
//! 被说成「文件不存在」）。
//!
//! 一条安全语义：**可内联渲染的扩展名（svg/html/js/…）强制转附件**，防止上传的
//! SVG/HTML 在同源下变成 XSS；成功路径都带 `X-Content-Type-Options: nosniff`。

use std::path::PathBuf;
use std::sync::Arc;

use crate::json::Problem;
use crate::resource::Resource;
use crate::runtime::Runtime;
use crate::saved_path::SavedPath;
use crate::util::{extension_of, get_file_name, sanitize_download_name};

/// 可内联渲染、需强制转附件的扩展名（与 1.x 的列表逐项一致）。
const INLINE_BLOCKED_EXTENSIONS: &[&str] = &[
    "svg", "svgz", "html", "htm", "xml", "xhtml", "xht", "xsl", "js", "mjs",
];

/// `x_accel_redirect` 约定使用的内部前缀，需与宿主配置里的 location 一致。
pub const ACCEL_PREFIX: &str = "/internal-aetherupload/";

/// 读路径的响应数据。适配器据此构造宿主框架的响应对象：
/// 先匹配 `Problem` 臂，再对三个成功变体用 [`ResourceResponse::status`] /
/// [`ResourceResponse::headers`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceResponse {
    /// 本地文件：`download_name` 为 `Some` 时按附件下发。
    ServeFile {
        path: PathBuf,
        download_name: Option<String>,
    },
    /// 302 到预签名 URL。
    Redirect { location: String },
    /// 内部重定向：宿主把 `X-Accel-Redirect`（或等价机制）透给前置服务器。
    AccelRedirect {
        internal_path: String,
        download_name: Option<String>,
    },
    /// 失败：问题响应（400 / 404 / 500）。
    Problem(Problem),
}

impl ResourceResponse {
    /// 状态码；`Problem` 臂原样用它自己的状态码。
    pub fn status(&self) -> u16 {
        match self {
            Self::ServeFile { .. } | Self::AccelRedirect { .. } => 200,
            Self::Redirect { .. } => 302,
            Self::Problem(problem) => problem.status(),
        }
    }

    /// 必须附加的响应头；`Problem` 臂返回空表（content-type 由 [`Problem::content_type`] 给）。
    pub fn headers(&self) -> Vec<(String, String)> {
        match self {
            // 可内联渲染的类型：本地文件与加速重定向都要转附件
            Self::Problem(_) => Vec::new(),
            Self::ServeFile { .. } => nosniff(),
            Self::Redirect { location } => {
                let mut headers = nosniff();
                headers.push(("Location".to_string(), location.clone()));
                headers
            }
            Self::AccelRedirect {
                internal_path,
                download_name,
            } => {
                let mut headers = nosniff();
                headers.push(("X-Accel-Redirect".to_string(), internal_path.clone()));
                if let Some(name) = download_name {
                    headers.push((
                        "Content-Disposition".to_string(),
                        attachment_disposition(name),
                    ));
                }
                headers
            }
        }
    }
}

fn nosniff() -> Vec<(String, String)> {
    vec![("X-Content-Type-Options".to_string(), "nosniff".to_string())]
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
        let resource = match self.locate(uri) {
            Ok(resource) => resource,
            Err(problem) => return ResourceResponse::Problem(problem),
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

        self.respond(&resource, inline_blocked, response_params)
    }

    /// 下载资源（可改文件名，扩展名仍是原资源的）。
    pub fn download(&self, uri: &str, new_name: &str) -> ResourceResponse {
        // 客户端可控的下载名：空串（URL 里没有该段）、含 `/` `\` 或控制字符都拒；
        // 其余消毒后使用（防头注入），扩展名取原资源的
        if !is_valid_new_name(new_name) {
            return ResourceResponse::Problem(bad_parameter());
        }

        let resource = match self.locate(uri) {
            Ok(resource) => resource,
            Err(problem) => return ResourceResponse::Problem(problem),
        };

        let sanitized = sanitize_download_name(new_name);
        let download_name = get_file_name(&sanitized, &extension_of(&resource.name));

        let response_params = vec![(
            "response-content-disposition".to_string(),
            attachment_disposition(&download_name),
        )];

        match self.respond(&resource, true, response_params) {
            ResourceResponse::ServeFile { path, .. } => ResourceResponse::ServeFile {
                path,
                download_name: Some(download_name),
            },
            other => other,
        }
    }

    /// 三条成功路径的选择：s3 → 加速重定向 → 本地文件。
    fn respond(
        &self,
        resource: &Resource,
        inline_blocked: bool,
        response_params: Vec<(String, String)>,
    ) -> ResourceResponse {
        // 存储驱动的签名/寻址失败是内部故障（500），不是「文件不存在」
        let url = match resource.url(self.runtime.storage(), &response_params) {
            Ok(url) => url,
            Err(_) => return ResourceResponse::Problem(internal_error()),
        };

        if let Some(location) = url {
            return ResourceResponse::Redirect { location };
        }

        if self.runtime.config().x_accel_redirect {
            // 只由服务端已校验的数据拼出；前缀与宿主 nginx 的 alias 对齐
            let internal_path = format!(
                "{}{}/{}/{}",
                ACCEL_PREFIX, resource.group_dir, resource.group_sub_dir, resource.name
            );

            return ResourceResponse::AccelRedirect {
                internal_path,
                download_name: inline_blocked.then(|| resource.name.clone()),
            };
        }

        ResourceResponse::ServeFile {
            path: resource.path.clone(),
            download_name: inline_blocked.then(|| resource.name.clone()),
        }
    }

    /// 寻址 + 存在性检查：解码失败 → 400（请求本身不合法）；**分组未配置/不可解析
    /// → 404**（标识符合法但指不到任何资源 —— 与「文件不存在」同一结局，顺带不向外
    /// 泄露本部署配了哪些分组）；文件不存在 → 404；存储驱动故障 → 500。
    fn locate(&self, uri: &str) -> std::result::Result<Resource, Problem> {
        let params = SavedPath::decode(uri).map_err(|_| bad_parameter())?;
        let snapshot = self
            .runtime
            .config()
            .resolve_group(&params.group)
            .map_err(|_| Problem::resource_not_found(None))?;

        let resource = Resource::new(
            self.runtime.upload_root(),
            &params.group,
            &snapshot.group_dir,
            &params.group_sub_dir,
            &params.resource_name,
        );

        match resource.exists(self.runtime.storage()) {
            Ok(true) => Ok(resource),
            Ok(false) => Err(Problem::resource_not_found(None)),
            Err(_) => Err(internal_error()),
        }
    }
}

/// 读路径的参数问题（400 `invalid_parameter`）。
fn bad_parameter() -> Problem {
    Problem::invalid_parameter(None)
}

/// 存储 / IO 故障（500 `internal_error`）。
fn internal_error() -> Problem {
    Problem::localized(500, "internal_error", None, Vec::new())
}

/// 下载名合法性：空串、含 `/` `\` 或控制字符 → 非法。
fn is_valid_new_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

fn is_inline_blocked(name: &str) -> bool {
    let extension = extension_of(name);

    INLINE_BLOCKED_EXTENSIONS
        .iter()
        .any(|blocked| *blocked == extension)
}

/// `Content-Disposition`（含 RFC 5987 双写法）：ASCII 回退 + `filename*=UTF-8''…`。
/// `name` 已消毒（无控制字符）。
///
/// 注意：`filename="…"` 那一份**保留原始 UTF-8 字节**（HTTP/1.1 的 obs-text），严格说
/// 不是纯 ASCII。现代浏览器优先读 `filename*`，行为与 1.x 逐字节一致；适配器若要用
/// `to_str()` 读这个头，请改用 `as_bytes()`。
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

/// `A-Za-z0-9-_.~` 之外全部按 UTF-8 字节百分号编码（大写十六进制）。
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
    use crate::error::Error;

    fn runtime(tag: &str) -> (PathBuf, Arc<Runtime>) {
        let root =
            std::env::temp_dir().join(format!("aetherupload-resctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // 成品落在 upload_root（= base_path/root_dir）下
        let group_dir = root.join("storage/app/aetherupload/file/202610");
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(group_dir.join("abc.jpg"), b"jpeg-bytes").unwrap();
        std::fs::write(group_dir.join("evil.svg"), b"<svg/>").unwrap();

        (
            root.clone(),
            Arc::new(Runtime::new(Config::default(), &root).unwrap()),
        )
    }

    fn problem_of(response: ResourceResponse) -> Problem {
        match response {
            ResourceResponse::Problem(problem) => problem,
            other => panic!("预期问题响应，实际 {other:?}"),
        }
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
        let runtime = Arc::new(
            Runtime::new(
                Config {
                    x_accel_redirect: true,
                    ..Config::default()
                },
                &root,
            )
            .unwrap(),
        );
        let controller = ResourceController::new(runtime);

        match controller.display("file_202610_abc.jpg") {
            ResourceResponse::AccelRedirect {
                internal_path,
                download_name,
            } => {
                assert_eq!(internal_path, "/internal-aetherupload/file/202610/abc.jpg");
                assert!(download_name.is_none());
            }
            other => panic!("预期加速重定向，实际 {other:?}"),
        }

        // 被拉黑类型带上附件头
        let headers = controller.display("file_202610_evil.svg").headers();
        assert!(
            headers.iter().any(
                |(key, value)| key == "Content-Disposition" && value.starts_with("attachment;")
            )
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 合法名字的 sanitize 语义：`"` 转 `_`，扩展名取原资源的
    ///（含 `/` `\` 的名字已被 400 拒，见下一条）。
    #[test]
    fn download_sanitizes_name_and_keeps_extension() {
        let (root, runtime) = runtime("download");
        let controller = ResourceController::new(runtime);

        match controller.download("file_202610_abc.jpg", "我的 \"报告\" v2") {
            ResourceResponse::ServeFile { download_name, .. } => {
                assert_eq!(download_name.as_deref(), Some("我的 _报告_ v2.jpg"));
            }
            other => panic!("预期本地文件响应，实际 {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 下载名非法：空串、含 `/` `\`、含控制字符 → 400（不管资源是否存在）。
    #[test]
    fn download_rejects_invalid_new_name() {
        let (root, runtime) = runtime("bad-name");
        let controller = ResourceController::new(runtime);

        for bad in ["", "a/b", "a\\b", "a\nb"] {
            let problem = problem_of(controller.download("file_202610_abc.jpg", bad));
            assert_eq!(
                (problem.status(), problem.code),
                (400, "invalid_parameter"),
                "new_name={bad:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 失败三态：解码失败 400 / 分组未配置 404 / 文件不存在 404；`headers()` 为空表。
    #[test]
    fn failures_are_problem_responses() {
        let (root, runtime) = runtime("fail");
        let controller = ResourceController::new(runtime);

        let response = controller.display("file_202610_missing.jpg");
        assert_eq!(response.status(), 404);
        assert!(response.headers().is_empty());
        let problem = problem_of(response);
        assert_eq!(problem.code, "resource_not_found");
        assert_eq!(problem.content_type(), "application/problem+json");
        assert!(
            problem
                .to_json()
                .contains(r#""title":"Error: resource not found""#)
        );

        let problem = problem_of(controller.display("bad_path"));
        assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"));

        let problem = problem_of(controller.display("nope_202610_x.jpg"));
        // 未知分组在**读路径**与「文件不存在」同归 404：标识符合法但指不到资源。
        // （上传端点上的未知分组才是 400 invalid_group —— 请求要求往不存在的分组里写。）
        assert_eq!(
            (problem.status(), problem.code),
            (404, "resource_not_found")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 全部动作都失败的存储驱动：验证「读不了盘」走 500 而不是 404。
    struct FailingStorage;

    impl crate::storage::Storage for FailingStorage {
        fn publish(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &str,
            _: &str,
        ) -> crate::error::Result<()> {
            Err(Error::RenameResourceFail)
        }
        fn exists(&self, _: &str, _: &str, _: &str) -> crate::error::Result<bool> {
            Err(Error::Io(std::io::Error::other("disk gone")))
        }
        fn delete(&self, _: &str, _: &str, _: &str) -> crate::error::Result<()> {
            Err(Error::DeleteResourceFail)
        }
        fn url(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[(String, String)],
        ) -> crate::error::Result<Option<String>> {
            Err(Error::Io(std::io::Error::other("sign fail")))
        }
    }

    #[test]
    fn storage_failures_are_500() {
        let (root, _) = runtime("storage");
        let runtime = Arc::new(
            Runtime::new(Config::default(), &root)
                .unwrap()
                .with_storage(Arc::new(FailingStorage)),
        );
        let controller = ResourceController::new(runtime);

        let problem = problem_of(controller.display("file_202610_abc.jpg"));
        assert_eq!((problem.status(), problem.code), (500, "internal_error"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disposition_uses_rfc5987_double_form() {
        assert_eq!(
            attachment_disposition("报告.pdf"),
            "attachment; filename=\"报告.pdf\"; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf"
        );
        assert!(attachment_disposition("a\"b\\c").contains("a\\\"b\\\\c"));
    }
}
