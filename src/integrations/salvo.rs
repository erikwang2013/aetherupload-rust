// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Salvo 集成：一个 [`Router`]，四条路由一次挂好。
//!
//! 分块走 salvo 的 multipart 表单解析：**文件字段会先落到临时文件**，这里再读回
//! 内存交给内核（默认分块 1MB）。内核同样接受 `ChunkBody::Path`，想省掉这次拷贝
//! 可把 `part.path()` 直接传进去。
//!
//! 本地文件下发用 [`NamedFile`]：超过 1MB 走 `ChunkedFile` 流式读，不把整份文件
//! 读进内存（1MB 以内由 salvo 预读，省掉小块读的系统调用），并且**顺带支持 HTTP
//! Range** —— 比 axum / poem 适配层多这一项能力，不需要关掉。
//!
//! 请求体上限：salvo 解析表单时按 `Request::secure_max_size()`（进程默认 64KB）限量，
//! 一次分块会被它先拒掉；这里用 `form_data_max_size(body_limit(&runtime))` 放开到
//! 「一个分块 + 1MB 的 multipart 头部余量」。
//!
//! runtime 通过一个 hoop（[`InjectRuntime`]）注入 [`Depot`] —— salvo 没有 axum
//! `State` 那样的提取器，hoop + `depot.get_typed()` 是它的等价做法。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Runtime};
//!
//! let runtime = Arc::new(Runtime::new(Config::default(), ".").unwrap());
//! let router = aetherupload::integrations::salvo::routes(runtime);
//! // let service = Service::new(router);
//! // Server::new(TcpListener::new("0.0.0.0:8080").bind().await).serve(service).await
//! ```

use std::sync::Arc;

use salvo::async_trait;
use salvo::fs::NamedFile;
use salvo::http::header::CONTENT_DISPOSITION;
use salvo::http::{HeaderName, HeaderValue, Mime, StatusCode};
use salvo::{Depot, FlowCtrl, Handler, Request, Response, Router, handler};

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{FormData, body_limit, display_route, download_route};
use crate::runtime::Runtime;

/// 注册四条路由（路径取自配置的 `route_*`）。
pub fn routes(runtime: Arc<Runtime>) -> Router {
    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();

    Router::new()
        // hoop 对整个路由树生效：四条路由都能从 depot 里取到 runtime
        .hoop(InjectRuntime(runtime.clone()))
        .push(Router::with_path(preprocess_path).post(preprocess))
        .push(Router::with_path(uploading_path).post(save_chunk))
        .push(Router::with_path(display_route(&runtime)).get(display))
        .push(Router::with_path(download_route(&runtime)).get(download))
}

/// 把 [`Runtime`] 放进 [`Depot`]，供四条路由取用。
struct InjectRuntime(Arc<Runtime>);

#[async_trait]
impl Handler for InjectRuntime {
    async fn handle(
        &self,
        _req: &mut Request,
        depot: &mut Depot,
        _res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        depot.insert_typed(self.0.clone());
    }
}

#[handler]
async fn preprocess(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(runtime) = runtime_of(depot) else {
        return internal_error(res);
    };

    let Ok(form) = parse_multipart(req, body_limit(&runtime)).await else {
        return bad_request(res, "invalid multipart body");
    };

    let result = UploadController::new(runtime).preprocess(&form.to_preprocess_request());

    let _ = res.write_body(result.to_json());
}

#[handler]
async fn save_chunk(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(runtime) = runtime_of(depot) else {
        return internal_error(res);
    };

    let Ok(form) = parse_multipart(req, body_limit(&runtime)).await else {
        return bad_request(res, "invalid multipart body");
    };

    let result = UploadController::new(runtime).save_chunk(&form.to_save_chunk_request());

    let _ = res.write_body(result.to_json());
}

#[handler]
async fn display(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(saved_path) = req.param::<String>("saved_path") else {
        return not_found(res, "display fail");
    };

    let Some(runtime) = runtime_of(depot) else {
        return internal_error(res);
    };

    let response = ResourceController::new(runtime).display(&saved_path);

    serve(response, req, res).await;
}

#[handler]
async fn download(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (Some(saved_path), Some(new_name)) = (
        req.param::<String>("saved_path"),
        req.param::<String>("new_name"),
    ) else {
        return not_found(res, "download fail");
    };

    let Some(runtime) = runtime_of(depot) else {
        return internal_error(res);
    };

    let response = ResourceController::new(runtime).download(&saved_path, &new_name);

    serve(response, req, res).await;
}

/// 把 multipart 表单读成 [`FormData`]：文本字段搬进内存，分块字段（salvo 已落临时
/// 文件）读回内存 —— 与 axum / poem 适配层保持同一种「分块走内存」的取舍。
async fn parse_multipart(req: &mut Request, limit: usize) -> Result<FormData, String> {
    let form_data = req
        .form_data_max_size(limit)
        .await
        .map_err(|err| err.to_string())?;

    let mut form = FormData::new();

    for (name, value) in form_data.fields.iter() {
        form.push_text(name.clone(), value.clone());
    }

    if let Some(part) = form_data.files.get("resource_chunk") {
        let bytes = std::fs::read(part.path()).map_err(|err| err.to_string())?;
        form.push_file("resource_chunk", ChunkBody::Bytes(bytes));
    }

    Ok(form)
}

/// 内核响应 → salvo 响应。
pub async fn serve(response: ResourceResponse, req: &Request, res: &mut Response) {
    // 内核给的头（含 `X-Content-Type-Options: nosniff`，重定向还带 Location）原样并进来
    for (name, value) in response.headers() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            res.headers_mut().insert(name, value);
        }
    }

    match response {
        ResourceResponse::NotFound(body) => {
            res.status_code(StatusCode::NOT_FOUND);
            let _ = res.write_body(body);
        }

        ResourceResponse::Redirect { .. } => {
            res.status_code(StatusCode::FOUND);
        }

        ResourceResponse::AccelRedirect { .. } => {
            res.status_code(StatusCode::OK);
        }

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            let Ok(mut file) = NamedFile::open(&path).await else {
                return not_found(res, "not found");
            };

            // 内核的 MIME 表（与上传校验同一张）优先于 salvo 自己的推断
            if let Some(content_type) = content_type_for(&path)
                && let Ok(mime) = content_type.parse::<Mime>()
            {
                file.set_content_type(mime);
            }

            // 附件名用内核的 RFC 5987 双写法头；NamedFile 见已有 Content-Disposition 就不覆盖
            if let Some(name) = download_name
                && let Ok(value) =
                    HeaderValue::from_str(&crate::controller::attachment_disposition(&name))
            {
                res.headers_mut().insert(CONTENT_DISPOSITION, value);
            }

            file.send(req.headers(), res).await;
        }
    }
}

/// 从 depot 取 runtime（hoop 已注入；缺失只可能是路由忘了挂 hoop）。
fn runtime_of(depot: &Depot) -> Option<Arc<Runtime>> {
    depot.get_typed::<Arc<Runtime>>().ok().cloned()
}

fn bad_request(res: &mut Response, message: &'static str) {
    res.status_code(StatusCode::BAD_REQUEST);
    let _ = res.write_body(message);
}

fn not_found(res: &mut Response, message: &'static str) {
    res.status_code(StatusCode::NOT_FOUND);
    let _ = res.write_body(message);
}

fn internal_error(res: &mut Response) {
    res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
    let _ = res.write_body("missing runtime");
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与 PHP 的文件响应同一张表）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
