// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Rocket 0.5 集成：把四条路由按配置的路径挂上。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Runtime};
//!
//! let runtime = Arc::new(Runtime::new(Config::default(), ".").unwrap());
//! let rocket = aetherupload::integrations::rocket::mount(rocket::build(), runtime);
//! // rocket.launch().await
//! ```
//!
//! **为什么不用 `routes!` 宏**：`#[post]` 这类宏路由的路径是编译期字面量，而这四条
//! 路由的路径来自运行期配置（`Config::route_*`），且 `mount()` 只是字符串拼接，
//! 宏路由表达不了。所以这里用 [`Route::new`] 手工注册 + 一条 [`Handler`] 实现；
//! 手工注册不经过宏的参数注入，运行时就由 handler 自己拿着（不依赖 `.manage()`）。
//!
//! 表单用 `Form<TempFile>` 提取：分块由 rocket 落成临时文件，内核以 `ChunkBody::Path`
//! 追加，全程不进内存。
//!
//! **请求体上限**：rocket 对 multipart 整条流用的是 `data-form` 限额（默认 2 MiB），
//! 比一次上传的分块大不了多少 —— [`mount`] 会把它放开到 [`body_limit`]（分块大小 + 1MB 余量）。
//!
//! 本地文件下发用 `rocket::fs` 那套流式响应（`sized_body` 挂 tokio 文件，边读边发），
//! 整份文件不进内存；**不实现 HTTP Range** —— 与 PHP/webman 版一致，需要断点下载 /
//! 视频拖动请开 `x_accel_redirect` 交给前置服务器。

use std::io::Cursor;
use std::sync::Arc;

use rocket::data::{Data, FromData};
use rocket::form::{Form, FromForm};
use rocket::fs::TempFile;
use rocket::http::{Method, Status};
use rocket::request::Request;
use rocket::response::Response;
use rocket::route::{Handler, Outcome, Route};
use rocket::{Build, Rocket};

use crate::controller::{
    ChunkBody, PreprocessRequest, ResourceController, ResourceResponse, SaveChunkRequest,
    UploadController,
};
use crate::integrations::{FormData, body_limit};
use crate::runtime::Runtime;

/// 挂载四条路由（路径取自配置的 `route_*`），顺带放开 multipart 的请求体上限。
///
/// 每条路由挂在**自己的**配置路径上，路由 URI 相对于挂载点是 `/`、`/<saved_path>` 或
/// `/<saved_path>/<new_name>`（`<..>` 是 rocket 的动态段语法，别写成别的框架的 `{..}`），
/// 这样 [`Request::param`] 的下标就是「挂载点之后的第几段」，不随配置路径的层数漂移。
pub fn mount(rocket: Rocket<Build>, runtime: Arc<Runtime>) -> Rocket<Build> {
    let figment = rocket
        .figment()
        .clone()
        .merge(("limits.data-form", body_limit(&runtime) as u64));

    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();
    let display_path = runtime.config().route_display.clone();
    let download_path = runtime.config().route_download.clone();

    rocket
        .configure(figment)
        .mount(
            preprocess_path.as_str(),
            vec![Route::new(
                Method::Post,
                "/",
                Endpoint::new(&runtime, Kind::Preprocess),
            )],
        )
        .mount(
            uploading_path.as_str(),
            vec![Route::new(
                Method::Post,
                "/",
                Endpoint::new(&runtime, Kind::SaveChunk),
            )],
        )
        .mount(
            display_path.as_str(),
            vec![Route::new(
                Method::Get,
                "/<saved_path>",
                Endpoint::new(&runtime, Kind::Display),
            )],
        )
        .mount(
            download_path.as_str(),
            vec![Route::new(
                Method::Get,
                "/<saved_path>/<new_name>",
                Endpoint::new(&runtime, Kind::Download),
            )],
        )
}

/// 四条路由的行为分支。
#[derive(Debug, Clone, Copy)]
enum Kind {
    Preprocess,
    SaveChunk,
    Display,
    Download,
}

/// 手工路由的 handler：路径来自运行期配置，handler 就得自己带着运行时。
#[derive(Clone)]
struct Endpoint {
    runtime: Arc<Runtime>,
    kind: Kind,
}

impl Endpoint {
    fn new(runtime: &Arc<Runtime>, kind: Kind) -> Self {
        Self {
            runtime: Arc::clone(runtime),
            kind,
        }
    }
}

#[rocket::async_trait]
impl Handler for Endpoint {
    async fn handle<'r>(&self, req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r> {
        match self.kind {
            Kind::Preprocess => preprocess(&self.runtime, req, data).await,
            Kind::SaveChunk => save_chunk(&self.runtime, req, data).await,
            Kind::Display => display(&self.runtime, req).await,
            Kind::Download => download(&self.runtime, req).await,
        }
    }
}

/// `preprocess` 的表单字段（与前端一致，缺字段交给内核报参数错误）。
#[derive(FromForm)]
struct PreprocessForm {
    resource_name: Option<String>,
    resource_size: Option<String>,
    group: Option<String>,
    resource_hash: Option<String>,
    locale: Option<String>,
}

/// `saveChunk` 的表单字段；分块本体是文件字段。
#[derive(FromForm)]
struct SaveChunkForm<'r> {
    chunk_total: Option<String>,
    chunk_index: Option<String>,
    resource_temp_basename: Option<String>,
    resource_ext: Option<String>,
    group_subdir: Option<String>,
    group: Option<String>,
    resource_hash: Option<String>,
    locale: Option<String>,
    resource_chunk: Option<TempFile<'r>>,
}

impl PreprocessForm {
    fn to_request(&self) -> PreprocessRequest {
        let mut form = FormData::new();

        push_text(&mut form, "resource_name", self.resource_name.as_deref());
        push_text(&mut form, "resource_size", self.resource_size.as_deref());
        push_text(&mut form, "group", self.group.as_deref());
        push_text(&mut form, "resource_hash", self.resource_hash.as_deref());
        push_text(&mut form, "locale", self.locale.as_deref());

        form.to_preprocess_request()
    }
}

impl SaveChunkForm<'_> {
    /// 借用式转换：handler 必须把表单（尤其是 [`TempFile`]）持有到内核读完分块为止。
    fn to_request(&self) -> SaveChunkRequest {
        let mut form = FormData::new();

        push_text(&mut form, "chunk_total", self.chunk_total.as_deref());
        push_text(&mut form, "chunk_index", self.chunk_index.as_deref());
        push_text(
            &mut form,
            "resource_temp_basename",
            self.resource_temp_basename.as_deref(),
        );
        push_text(&mut form, "resource_ext", self.resource_ext.as_deref());
        push_text(&mut form, "group_subdir", self.group_subdir.as_deref());
        push_text(&mut form, "group", self.group.as_deref());
        push_text(&mut form, "resource_hash", self.resource_hash.as_deref());
        push_text(&mut form, "locale", self.locale.as_deref());

        // 分块：正常是磁盘上的临时文件（rocket 边收边落盘）；没落盘的走内存分支。
        // `TempFile` 一被 drop 就会删掉临时文件，所以这里只能借出路径 ——
        // 内核读的时候表单还得活着（这正是 `to_request` 不取 `self` 的原因）。
        match &self.resource_chunk {
            Some(TempFile::Buffered { content }) => {
                form.push_file("resource_chunk", ChunkBody::Bytes(content.to_vec()));
            }
            Some(file) => {
                if let Some(path) = file.path() {
                    form.push_file("resource_chunk", ChunkBody::Path(path.to_path_buf()));
                }
            }
            None => {}
        }

        form.to_save_chunk_request()
    }
}

fn push_text(form: &mut FormData, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        form.push_text(name, value);
    }
}

/// 解析表单；读不出来时把 rocket 的判定原样交回去 —— Forward 归 Forward，
/// Error 带它自己的状态码（字段非法 → 422，超过 `data-form` 限额 → 413）。
///
/// rocket 的 `Outcome` 本身就大（含 `Data` / `Response`），为讨好 lint 而 Box 一层没有意义。
#[allow(clippy::result_large_err)]
async fn parse<'r, T: FromForm<'r>>(
    req: &'r Request<'_>,
    data: Data<'r>,
) -> Result<Form<T>, Outcome<'r>> {
    use rocket::data::Outcome as DataOutcome;

    match Form::<T>::from_data(req, data).await {
        DataOutcome::Success(form) => Ok(form),
        DataOutcome::Forward((data, status)) => Err(Outcome::Forward((data, status))),
        DataOutcome::Error((status, _)) => Err(Outcome::Error(status)),
    }
}

async fn preprocess<'r>(
    runtime: &Arc<Runtime>,
    req: &'r Request<'_>,
    data: Data<'r>,
) -> Outcome<'r> {
    let form = match parse::<PreprocessForm>(req, data).await {
        Ok(form) => form,
        Err(outcome) => return outcome,
    };

    let result = UploadController::new(Arc::clone(runtime)).preprocess(&form.to_request());

    json_response(result.to_json())
}

async fn save_chunk<'r>(
    runtime: &Arc<Runtime>,
    req: &'r Request<'_>,
    data: Data<'r>,
) -> Outcome<'r> {
    let form = match parse::<SaveChunkForm<'_>>(req, data).await {
        Ok(form) => form,
        Err(outcome) => return outcome,
    };

    // 表单要活到这一行之后：`resource_chunk` 的临时文件随表单一起销毁
    let result = UploadController::new(Arc::clone(runtime)).save_chunk(&form.to_request());

    json_response(result.to_json())
}

async fn display<'r>(runtime: &Arc<Runtime>, req: &'r Request<'_>) -> Outcome<'r> {
    let Some(Ok(saved_path)) = req.param::<String>(0) else {
        return rejection();
    };

    serve(ResourceController::new(Arc::clone(runtime)).display(&saved_path)).await
}

async fn download<'r>(runtime: &Arc<Runtime>, req: &'r Request<'_>) -> Outcome<'r> {
    let (Some(Ok(saved_path)), Some(Ok(new_name))) =
        (req.param::<String>(0), req.param::<String>(1))
    else {
        return rejection();
    };

    serve(ResourceController::new(Arc::clone(runtime)).download(&saved_path, &new_name)).await
}

/// 内核响应 → rocket 响应。
async fn serve<'r>(response: ResourceResponse) -> Outcome<'r> {
    let headers = response.headers();

    match response {
        ResourceResponse::NotFound(body) => {
            let mut response: Response<'r> = Response::new();
            response.set_status(Status::NotFound);
            response.set_sized_body(body.len(), Cursor::new(body));

            Outcome::Success(response)
        }

        ResourceResponse::Redirect { location } => {
            let mut builder: rocket::response::Builder<'r> = Response::build();
            builder.status(Status::Found);
            builder.raw_header("Location", location);
            apply_headers(&mut builder, headers);

            Outcome::Success(builder.finalize())
        }

        ResourceResponse::AccelRedirect { .. } => {
            let mut builder: rocket::response::Builder<'r> = Response::build();
            builder.status(Status::Ok);
            apply_headers(&mut builder, headers);

            Outcome::Success(builder.finalize())
        }

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            let Ok(file) = rocket::tokio::fs::File::open(&path).await else {
                return not_found();
            };

            let Ok(meta) = rocket::tokio::fs::metadata(&path).await else {
                return not_found();
            };

            let mut builder: rocket::response::Builder<'r> = Response::build();
            builder.status(Status::Ok);
            apply_headers(&mut builder, headers);

            if let Some(content_type) = content_type_for(&path) {
                builder.raw_header("Content-Type", content_type);
            }

            if let Some(name) = download_name {
                builder.raw_header(
                    "Content-Disposition",
                    crate::controller::attachment_disposition(&name),
                );
            }

            Outcome::Success(builder.sized_body(meta.len() as usize, file).finalize())
        }
    }
}

fn json_response<'r>(body: String) -> Outcome<'r> {
    let body = body.into_bytes();

    let mut response: Response<'r> = Response::new();
    response.set_status(Status::Ok);
    response.set_raw_header("Content-Type", "application/json");
    response.set_sized_body(body.len(), Cursor::new(body));

    Outcome::Success(response)
}

fn rejection<'r>() -> Outcome<'r> {
    Outcome::Error(Status::BadRequest)
}

fn not_found<'r>() -> Outcome<'r> {
    let mut response: Response<'r> = Response::new();
    response.set_status(Status::NotFound);
    response.set_sized_body(9, Cursor::new(b"not found".to_vec()));

    Outcome::Success(response)
}

/// 内核响应附带的安全头（nosniff / Location / X-Accel-Redirect / Content-Disposition）。
fn apply_headers<'r>(builder: &mut rocket::response::Builder<'r>, headers: Vec<(String, String)>) {
    for (name, value) in headers {
        builder.raw_header(name, value);
    }
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与 PHP 的文件响应同一张表）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
