// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! S3 兼容对象存储驱动 —— 对应 PHP 版 `Storage/S3Storage.php` + `S3Client.php`。
//!
//! 只做本包需要的五个操作：PUT / multipart / HEAD / DELETE / 预签名 GET。
//!
//! **对象键与本地布局同构**：`{prefix}{group_dir}/{subdir}/{name}`（`root_dir` 不泄漏进桶）。
//!
//! **传输以 trait 注入**（与 [`crate::instant::InstantStore`] 同一取向）：本 crate
//! 不依赖任何 HTTP 客户端。宿主把自家的客户端（`reqwest`、`ureq`、curl 绑定……）包一层
//! 实现 [`HttpTransport`]，用 [`S3Storage::with_transport`] 注入；没接的同学拿到的是
//! [`NullTransport`] —— 一调用就报错，而不是悄悄失败。
//!
//! **错误归一**（照抄 PHP）：[`Storage::publish`] 的任何底层失败都对外报
//! [`Error::RenameResourceFail`]、删本地失败报 [`Error::DeleteResourceFail`]；
//! [`Storage::exists`] / [`Storage::delete`] 让底层错误原样上行。底层错误消息里的
//! 凭据一律擦除（见 `fail()`），secret_key 从不出现在任何消息里。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{PayloadSigning, S3Config};
use crate::error::{Error, Result};
use crate::mime::{MagicBytesDetector, MimeDetector};
use crate::storage::Storage;
use crate::storage::sigv4::{self, Signer, UNSIGNED_PAYLOAD};

/// 固定分片大小：非末片恒为 64MB（≥ S3 的 5MB 下限），末片可小。
const PART_SIZE: u64 = 67_108_864;

/// 幂等操作的重试上限（网络异常 / 5xx / 429；4xx 不重试）。
const MAX_ATTEMPTS: u32 = 3;

/// 重试退避基数（第 n 次重试前睡 `n * 200ms`，与 PHP 的 `RETRY_SLEEP_US` 逐值一致）。
const RETRY_SLEEP_MILLIS: u64 = 200;

/// 预签名 URL 有效期（秒）—— 下载是「302 后立即取」，300s 足够；与 PHP 的 `PRESIGN_TTL` 一致。
pub const PRESIGN_TTL: u64 = 300;

/// 请求体。
#[derive(Debug, Clone)]
pub enum HttpBody {
    /// 无 body（HEAD / DELETE / 预签名 GET）。
    Empty,
    /// 内存里的完整 body（multipart 的 complete XML 等）。
    Bytes(Vec<u8>),
    /// 本地文件的 `[offset, offset + len)` 切片 —— multipart 分片零拷贝：不整读进内存，
    /// 且 payload 哈希正好覆盖**实际发送的字节**（PHP 的 `bodyFile`/`bodyOffset`/`bodySize`）。
    File {
        /// 本地文件路径。
        path: PathBuf,
        /// 切片起点（字节）。
        offset: u64,
        /// 切片长度（字节）。
        len: u64,
    },
}

/// 待发送的签名请求。重试会原样重发（同一 `x-amz-date` 与签名）。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP 方法（大写）。
    pub method: String,
    /// 完整 URL（含查询串）。
    pub url: String,
    /// 头（含 `Host` / `x-amz-content-sha256` / `x-amz-date` / `Authorization`）。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: HttpBody,
}

/// 响应。头名按 HTTP 语义大小写不敏感（[`HttpResponse::header`]）。
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应头。
    pub headers: Vec<(String, String)>,
    /// 响应体。
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 头名大小写不敏感读取。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// S3 传输端口：本 crate 不绑定任何 HTTP 客户端，宿主实现它。
///
/// 网络异常返回 `Err`（会触发重试）；HTTP 层的非 2xx **不是** `Err`，按状态码返回。
pub trait HttpTransport: Send + Sync {
    /// 发一个已签名的请求。
    fn send(&self, request: HttpRequest) -> Result<HttpResponse>;
}

/// 未接传输的默认实现：一调用就报错（与 [`crate::instant::NullInstantStore`] 同一取向）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullTransport;

impl HttpTransport for NullTransport {
    fn send(&self, _request: HttpRequest) -> Result<HttpResponse> {
        Err(Error::Backend(
            "未为此宿主配置 S3 传输：请实现 HttpTransport 并用 S3Storage::with_transport 注入"
                .to_string(),
        ))
    }
}

/// S3 兼容存储驱动。
pub struct S3Storage {
    config: S3Config,
    transport: Arc<dyn HttpTransport>,
}

impl S3Storage {
    /// 按 `storage.s3` 配置构造（`upload_root` 与对象键无关 —— S3 的键不含本地根，
    /// 保留该参数只是为了让 `Runtime` 的两个驱动共用同一段装配代码）。
    ///
    /// fail-fast：`region` / `bucket` / `access_key` / `secret_key` 缺一即报
    /// [`Error::Backend`]，消息只点名配置键、绝不回显值。
    /// `endpoint` 可空 = AWS 默认（按 region 推导）。
    pub fn new(config: S3Config, _upload_root: &Path) -> Result<Self> {
        for (key, value) in [
            ("region", &config.region),
            ("bucket", &config.bucket),
            ("access_key", &config.access_key),
            ("secret_key", &config.secret_key),
        ] {
            if value.is_empty() {
                return Err(Error::Backend(format!(
                    "S3 配置缺少 storage.s3.{key}（S3 驱动必需）"
                )));
            }
        }

        Ok(Self {
            config,
            transport: Arc::new(NullTransport),
        })
    }

    /// 注入传输实现（构造期完成，链式）。
    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// 对象键：`{prefix}[／]{group_dir}/{subdir}/{name}`（前缀末尾无 `/` 时补一个，不出现双斜杠）。
    pub fn key(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> String {
        let prefix = self.config.prefix.as_str();

        if prefix.is_empty() {
            return object_key(group_dir, group_sub_dir, name);
        }

        let separator = if prefix.ends_with('/') { "" } else { "/" };

        format!(
            "{prefix}{separator}{}",
            object_key(group_dir, group_sub_dir, name)
        )
    }

    /// 对象 URL 三元组 `(scheme, host, path)` —— 对应 PHP `S3Client::objectUrl()`。
    fn object_url(&self, key: &str) -> (String, String, String) {
        let bucket = &self.config.bucket;

        if self.config.endpoint.is_empty() {
            let region = &self.config.region;

            return if self.config.path_style {
                (
                    "https".to_string(),
                    format!("s3.{region}.amazonaws.com"),
                    format!("/{bucket}/{key}"),
                )
            } else {
                (
                    "https".to_string(),
                    format!("{bucket}.s3.{region}.amazonaws.com"),
                    format!("/{key}"),
                )
            };
        }

        // 自托管：保留端点里的端口与子路径（MinIO/Ceph 挂在反向代理子路径下的情形）
        let (scheme, authority, base) = split_endpoint(&self.config.endpoint);

        if self.config.path_style {
            (scheme, authority, format!("{base}/{bucket}/{key}"))
        } else {
            (
                scheme,
                format!("{bucket}.{authority}"),
                format!("{base}/{key}"),
            )
        }
    }

    /// 发一个已签名请求 —— 对应 PHP `S3Client::call()`。
    ///
    /// 请求在重试循环**之前**构造一次：三次重试沿用同一 `x-amz-date` 与签名（PHP 同）。
    fn call(
        &self,
        method: &str,
        key: &str,
        query: &str,
        headers: Vec<(String, String)>,
        body: HttpBody,
    ) -> Result<HttpResponse> {
        let (scheme, host, path) = self.object_url(key);
        let payload_hash = self.payload_hash(&body)?;
        let amz_date = amz_date_now();

        // 只签三个自产头：host / x-amz-content-sha256 / x-amz-date（Content-Type 等不签，S3 允许）
        let signed = [
            ("host".to_string(), host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let signer = Signer::new(
            &self.config.access_key,
            &self.config.secret_key,
            &self.config.region,
            "s3",
        );
        let authorization =
            signer.sign_request(method, &path, query, &signed, &payload_hash, &amz_date);

        let mut headers = headers;
        headers.push(("Host".to_string(), host.clone()));
        headers.push(("x-amz-content-sha256".to_string(), payload_hash));
        headers.push(("x-amz-date".to_string(), amz_date));
        headers.push(("Authorization".to_string(), authorization));

        let request = HttpRequest {
            method: method.to_string(),
            url: if query.is_empty() {
                format!("{scheme}://{host}{path}")
            } else {
                format!("{scheme}://{host}{path}?{query}")
            },
            headers,
            body,
        };

        let mut attempt = 1;
        loop {
            match self.transport.send(request.clone()) {
                Ok(response) => {
                    if (response.status >= 500 || response.status == 429) && attempt < MAX_ATTEMPTS
                    {
                        retry_sleep(attempt);
                        attempt += 1;
                        continue;
                    }

                    return Ok(response);
                }
                Err(error) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(Error::Backend(format!(
                            "S3 {method} 失败：传输错误：{}（key={key}）",
                            error_detail(&error)
                        )));
                    }

                    retry_sleep(attempt);
                    attempt += 1;
                }
            }
        }
    }

    /// 签名用的 payload 哈希位。
    ///
    /// 必须等于**实际发送的字节**：multipart 分片是文件切片，用整文件哈希签分片会被
    /// MinIO/AWS 以 `XAmzContentSHA256Mismatch` 拒绝。`payload_signing = unsigned` 时
    /// 改发占位符（华为云 OBS 等只接受该形式，顺带省一次全文件读）。
    fn payload_hash(&self, body: &HttpBody) -> Result<String> {
        if self.config.payload_signing == PayloadSigning::Unsigned {
            return Ok(UNSIGNED_PAYLOAD.to_string());
        }

        match body {
            // HEAD / DELETE 无 body：空串的 sha256
            HttpBody::Empty => Ok(sigv4::sha256_hex(&[])),
            HttpBody::Bytes(bytes) => Ok(sigv4::sha256_hex(bytes)),
            HttpBody::File { path, offset, len } => sha256_hex_file_slice(path, *offset, *len),
        }
    }

    /// HEAD：200 → true，404 → false，其余状态报错（403 不能当成「不存在」）。
    fn head(&self, key: &str) -> Result<bool> {
        let response = self.call("HEAD", key, "", Vec::new(), HttpBody::Empty)?;

        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(self.fail("HEAD", key, &response)),
        }
    }

    /// 单次 PUT（`Content-Type` 按本地内容探测）。
    fn put(&self, key: &str, local_path: &Path, content_type: &str) -> Result<()> {
        let body = HttpBody::File {
            path: local_path.to_path_buf(),
            offset: 0,
            len: file_len(local_path)?,
        };
        let headers = vec![("Content-Type".to_string(), content_type.to_string())];
        let response = self.call("PUT", key, "", headers, body)?;

        if response.status != 200 {
            return Err(self.fail("PUT", key, &response));
        }

        Ok(())
    }

    /// 删除对象。S3 删除是幂等的：204；个别实现 404，同样放过。
    fn delete_object(&self, key: &str) -> Result<()> {
        let response = self.call("DELETE", key, "", Vec::new(), HttpBody::Empty)?;

        if matches!(response.status, 200 | 204 | 404) {
            return Ok(());
        }

        Err(self.fail("DELETE", key, &response))
    }

    /// 按阈值选路：`filesize > multipart_threshold` 走 multipart（**严格大于**，与 PHP 同）。
    fn upload_multipart_or_put(
        &self,
        key: &str,
        local_path: &Path,
        content_type: &str,
    ) -> Result<()> {
        if file_len(local_path)? > self.config.multipart_threshold {
            return self.multipart(key, local_path, content_type);
        }

        self.put(key, local_path, content_type)
    }

    /// multipart 编排：create → parts（固定 64MB 切片）→ complete。
    ///
    /// 两条失败兜底（照抄 PHP，都是实测踩过的坑）：
    /// - complete 的响应丢了（网络异常）但对象其实已写入 → HEAD 确认后视为成功；
    /// - HTTP 200 但体内嵌 `<Error>` 仍是失败（部分实现的怪癖）→ HEAD 确认后 abort。
    fn multipart(&self, key: &str, local_path: &Path, content_type: &str) -> Result<()> {
        let headers = vec![("Content-Type".to_string(), content_type.to_string())];
        let init = self.call("POST", key, "uploads=", headers.clone(), HttpBody::Empty)?;

        if init.status != 200 {
            return Err(self.fail("CreateMultipartUpload", key, &init));
        }

        let init_body = String::from_utf8_lossy(&init.body).into_owned();
        let Some(upload_id) = xml_tag(&init_body, "UploadId").map(xml_unescape) else {
            return Err(self.fail("CreateMultipartUpload", key, &init));
        };

        let file_size = file_len(local_path)?;
        let part_count = file_size.div_ceil(PART_SIZE);
        let mut parts: Vec<(u64, String)> = Vec::new();

        for number in 1..=part_count {
            let offset = (number - 1) * PART_SIZE;
            let size = PART_SIZE.min(file_size - offset);
            let query = format!(
                "partNumber={number}&uploadId={}",
                sigv4::uri_encode(upload_id.as_bytes(), true)
            );
            let body = HttpBody::File {
                path: local_path.to_path_buf(),
                offset,
                len: size,
            };
            let response = self.call("PUT", key, &query, headers.clone(), body)?;
            let etag = response.header("etag").map(str::to_string);

            if response.status != 200 || etag.is_none() {
                self.abort(key, &upload_id);
                return Err(self.fail(&format!("UploadPart#{number}"), key, &response));
            }

            parts.push((number, etag.expect("上面刚判过 is_none")));
        }

        let mut xml = String::from("<CompleteMultipartUpload>");
        for (number, etag) in &parts {
            // ENT_NOQUOTES：ETag 的双引号是值的一部分，不做实体化（&<> 仍要转义）
            xml.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
                xml_escape_text(etag)
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");

        let query = format!("uploadId={}", sigv4::uri_encode(upload_id.as_bytes(), true));
        let complete = match self.call(
            "POST",
            key,
            &query,
            vec![("Content-Type".to_string(), "application/xml".to_string())],
            HttpBody::Bytes(xml.into_bytes()),
        ) {
            Ok(response) => response,
            Err(error) => {
                // complete 可能已在服务端成功、只是响应没回来；HEAD 失败不掩盖原始异常
                if matches!(self.head(key), Ok(true)) {
                    return Ok(());
                }

                self.abort(key, &upload_id);
                return Err(error);
            }
        };

        // 经典陷阱：HTTP 200 但体内嵌 <Error> 仍是失败
        let body = String::from_utf8_lossy(&complete.body);
        if complete.status == 200 && !body.to_lowercase().contains("<error") {
            return Ok(());
        }

        // 失败路径同样先 HEAD 确认（防「实际成功但响应异常」）
        if self.head(key)? {
            return Ok(());
        }

        self.abort(key, &upload_id);
        Err(self.fail("CompleteMultipartUpload", key, &complete))
    }

    /// AbortMultipartUpload，best-effort：清理失败不掩盖原始异常。
    fn abort(&self, key: &str, upload_id: &str) {
        let query = format!("uploadId={}", sigv4::uri_encode(upload_id.as_bytes(), true));
        let _ = self.call("DELETE", key, &query, Vec::new(), HttpBody::Empty);
    }

    /// 预签名 GET —— 对应 PHP `S3Client::presign()`。纯本地计算，不发任何请求。
    fn presign(&self, key: &str, ttl: u64, response_params: &[(String, String)]) -> Result<String> {
        let (scheme, host, path) = self.object_url(key);

        let mut query = String::new();
        for (name, value) in response_params {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&sigv4::uri_encode(name.as_bytes(), true));
            query.push('=');
            query.push_str(&sigv4::uri_encode(value.as_bytes(), true));
        }

        let signer = Signer::new(
            &self.config.access_key,
            &self.config.secret_key,
            &self.config.region,
            "s3",
        );
        let signed = signer.presign(
            "GET",
            &path,
            &query,
            &[("host".to_string(), host.clone())],
            &amz_date_now(),
            ttl,
        );

        Ok(format!("{scheme}://{host}{path}?{signed}"))
    }

    /// 失败诊断消息 —— 对应 PHP `S3Client::fail()`。
    ///
    /// 错误体只留诊断信息，凭据标识一律擦掉：既擦本驱动 `access_key` 的裸串
    /// （网关错误页可能回显 Authorization），也擦 `<AWSAccessKeyId>` 标签的内容
    /// （回显的可能不是本配置的 key）。secret_key 从不参与。
    fn fail(&self, op: &str, key: &str, response: &HttpResponse) -> Error {
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        let access_key = self.config.access_key.as_str();

        if access_key.len() >= 8 {
            body = body.replace(access_key, "***");
        }
        body = redact_access_key_id(&body);

        let code = xml_tag(&body, "Code").unwrap_or_default();
        let message = match xml_tag(&body, "Message") {
            Some(message) => message.to_string(),
            None => body.chars().take(200).collect(),
        };

        Error::Backend(format!(
            "S3 {op} 失败：HTTP {} {code} {message}（key={key}）",
            response.status
        ))
    }
}

impl std::fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 刻意不打印 secret_key：调试输出同样可能落日志
        f.debug_struct("S3Storage")
            .field("endpoint", &self.config.endpoint)
            .field("region", &self.config.region)
            .field("bucket", &self.config.bucket)
            .field("access_key", &self.config.access_key)
            .field("secret_key", &"***")
            .field("path_style", &self.config.path_style)
            .field("prefix", &self.config.prefix)
            .field("multipart_threshold", &self.config.multipart_threshold)
            .field("payload_signing", &self.config.payload_signing)
            .finish()
    }
}

impl Storage for S3Storage {
    fn publish(
        &self,
        local_path: &Path,
        group_dir: &str,
        group_sub_dir: &str,
        name: &str,
    ) -> Result<()> {
        let key = self.key(group_dir, group_sub_dir, name);

        let uploaded = (|| {
            // 去重：目标已存在（同 hash）则不重复写
            if self.head(&key)? {
                return Ok(());
            }

            self.upload_multipart_or_put(&key, local_path, &detect_content_type(local_path))
        })();

        if uploaded.is_err() {
            // 对外统一为上传链路的既有错误键（PHP 把底层原因挂在 previous 上；
            // 本 crate 的错误枚举承载不了 cause，底层细节见 fail() 的擦除后消息）
            return Err(Error::RenameResourceFail);
        }

        std::fs::remove_file(local_path).map_err(|_| Error::DeleteResourceFail)
    }

    fn exists(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> Result<bool> {
        self.head(&self.key(group_dir, group_sub_dir, name))
    }

    fn delete(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> Result<()> {
        self.delete_object(&self.key(group_dir, group_sub_dir, name))
    }

    fn url(
        &self,
        group_dir: &str,
        group_sub_dir: &str,
        name: &str,
        response_params: &[(String, String)],
    ) -> Result<Option<String>> {
        Ok(Some(self.presign(
            &self.key(group_dir, group_sub_dir, name),
            PRESIGN_TTL,
            response_params,
        )?))
    }
}

/// 对象键主体：`group_dir/group_sub_dir/name`，空段跳过 —— 与本地布局同构且不出现双斜杠。
fn object_key(group_dir: &str, group_sub_dir: &str, name: &str) -> String {
    let mut key = String::with_capacity(group_dir.len() + group_sub_dir.len() + name.len() + 2);

    for segment in [group_dir, group_sub_dir, name] {
        if segment.is_empty() {
            continue;
        }
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(segment);
    }

    key
}

/// 拆分 `endpoint`：`scheme://host[:port][/base]`（缺 scheme 时按 https 补全）。
///
/// 返回 `(scheme, host[:port], base)`；`base` 要么是空串、要么以 `/` 开头（无尾斜杠）。
fn split_endpoint(endpoint: &str) -> (String, String, String) {
    let with_scheme = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    };
    let (scheme, rest) = with_scheme.split_once("://").expect("上面刚补过 scheme");

    let (authority, base) = match rest.split_once('/') {
        Some((authority, path)) => {
            let trimmed = path.trim_end_matches('/');
            (
                authority,
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!("/{trimmed}")
                },
            )
        }
        None => (rest, String::new()),
    };

    (scheme.to_string(), authority.to_string(), base)
}

/// 与 PHP `mime_content_type()` 对齐：按内容探测；探测不到回落到 octet-stream。
fn detect_content_type(path: &Path) -> String {
    MagicBytesDetector
        .detect(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// 本地文件大小（对应 PHP `filesize()`）。
fn file_len(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path).map_err(Error::from)?.len())
}

/// 流式计算文件切片 `[offset, offset + len)` 的 sha256（不整读进内存）。
///
/// `offset = 0` 且 `len` 等于文件大小时即整文件哈希 —— PHP 的 `hash_file` 快路径在此
/// 不是必需的优化（都是流式读一遍），少一个分支少一处不一致。
fn sha256_hex_file_slice(path: &Path, offset: u64, len: u64) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(Error::from)?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).map_err(Error::from)?;
    }

    sigv4::sha256_hex_reader(&mut file.take(len)).map_err(Error::from)
}

fn retry_sleep(attempt: u32) {
    std::thread::sleep(Duration::from_millis(
        RETRY_SLEEP_MILLIS * u64::from(attempt),
    ));
}

/// 传输错误的内层消息（`Backend` 直接取，其余走 `Display`）—— 避免消息里套一层通用文案。
fn error_detail(error: &Error) -> String {
    match error {
        Error::Backend(message) => message.clone(),
        other => other.to_string(),
    }
}

/// 取第一个 `<name>…</name>` 之间的文本。
fn xml_tag<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = start + body[start..].find(&close)?;

    Some(&body[start..end])
}

/// XML 文本节点转义（PHP 的 `ENT_NOQUOTES`）：`&<>` 转义，引号保持原样。
fn xml_escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML 实体解码（PHP 的 `html_entity_decode(…, ENT_QUOTES | ENT_XML1)`）：
/// 五个具名实体 + `&#NN;` / `&#xHH;` 数字引用，其余原样保留。
fn xml_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());

    for (index, part) in value.split('&').enumerate() {
        if index == 0 {
            out.push_str(part);
            continue;
        }

        let Some((entity, rest)) = part.split_once(';') else {
            out.push('&');
            out.push_str(part);
            continue;
        };

        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => match entity.strip_prefix('#').and_then(decode_numeric) {
                Some(decoded) => out.push(decoded),
                None => {
                    out.push('&');
                    out.push_str(entity);
                    out.push(';');
                }
            },
        }
        out.push_str(rest);
    }

    out
}

fn decode_numeric(entity: &str) -> Option<char> {
    let code = match entity.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => entity.parse::<u32>().ok()?,
    };

    char::from_u32(code)
}

/// 把 `<AWSAccessKeyId>…</AWSAccessKeyId>` 的内容擦成 `***`。
fn redact_access_key_id(body: &str) -> String {
    const OPEN: &str = "<AWSAccessKeyId>";
    const CLOSE: &str = "</AWSAccessKeyId>";

    let mut out = String::with_capacity(body.len());
    let mut rest = body;

    while let Some(start) = rest.find(OPEN) {
        let after_open = start + OPEN.len();
        let Some(end) = rest[after_open..].find(CLOSE) else {
            break;
        };

        out.push_str(&rest[..after_open]);
        out.push_str("***");
        rest = &rest[after_open + end..];
    }

    out.push_str(rest);
    out
}

/// 当前 UTC 时刻的 `YYYYMMDDTHHMMSSZ`（对应 PHP `gmdate('Ymd\THis\Z')`）。
fn amz_date_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    format_amz_date(seconds)
}

/// Unix 秒 → `YYYYMMDDTHHMMSSZ`（纯算术，不引第三方日期库）。
fn format_amz_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// 天数（1970-01-01 为 0）→ `(年, 月, 日)` —— Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> S3Config {
        S3Config {
            endpoint: "http://ep:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "b".to_string(),
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            path_style: true,
            prefix: "up".to_string(),
            multipart_threshold: 104_857_600,
            payload_signing: PayloadSigning::Hash,
        }
    }

    fn storage(config: S3Config) -> S3Storage {
        S3Storage::new(config, Path::new("/nonexistent")).unwrap()
    }

    #[test]
    fn constructor_requires_credentials_and_never_echoes_them() {
        for missing in ["region", "bucket", "access_key", "secret_key"] {
            let mut config = config();
            match missing {
                "region" => config.region.clear(),
                "bucket" => config.bucket.clear(),
                "access_key" => config.access_key.clear(),
                _ => config.secret_key.clear(),
            }

            let error = S3Storage::new(config, Path::new("/nonexistent")).unwrap_err();
            let message = format!("{error}");

            assert!(
                message.contains(&format!("storage.s3.{missing}")),
                "{message}"
            );
            assert!(!message.contains("wJalrXUtnFEMI"), "错误消息不得回显凭据");
        }

        // endpoint 可空 = AWS 默认
        let mut config = config();
        config.endpoint.clear();
        assert!(S3Storage::new(config, Path::new("/nonexistent")).is_ok());
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let rendered = format!("{:?}", storage(config()));

        assert!(rendered.contains("***"));
        assert!(!rendered.contains("wJalrXUtnFEMI"));
        assert!(
            rendered.contains("multipart_threshold"),
            "诊断字段仍应可见：{rendered}"
        );
    }

    #[test]
    fn key_joins_prefix_without_double_slashes() {
        assert_eq!(
            storage(config()).key("gd", "sub", "name.gif"),
            "up/gd/sub/name.gif"
        );

        let mut trailing = config();
        trailing.prefix = "up/".to_string();
        assert_eq!(
            storage(trailing).key("gd", "sub", "name.gif"),
            "up/gd/sub/name.gif"
        );

        let mut empty = config();
        empty.prefix = String::new();
        assert_eq!(
            storage(empty).key("gd", "sub", "name.gif"),
            "gd/sub/name.gif"
        );
    }

    #[test]
    fn object_url_follows_endpoint_path_style_and_region() {
        let cases = [
            // [配置覆盖, 期望 (scheme, host, path)]，自托管必须保端点前缀（端口/子路径）
            (
                ("", "eu-west-1", true),
                ("https", "s3.eu-west-1.amazonaws.com", "/b/gd/sub/a.gif"),
            ),
            (
                ("", "eu-west-1", false),
                ("https", "b.s3.eu-west-1.amazonaws.com", "/gd/sub/a.gif"),
            ),
            (
                ("http://h:9900/base", "us-east-1", true),
                ("http", "h:9900", "/base/b/gd/sub/a.gif"),
            ),
            (
                ("http://h:9900/base", "us-east-1", false),
                ("http", "b.h:9900", "/base/gd/sub/a.gif"),
            ),
            (
                ("minio.local:9000", "us-east-1", true),
                ("https", "minio.local:9000", "/b/gd/sub/a.gif"),
            ),
            (
                ("https://h/", "us-east-1", true),
                ("https", "h", "/b/gd/sub/a.gif"),
            ),
        ];

        for (label, (overrides, expected)) in cases.into_iter().enumerate() {
            let (endpoint, region, path_style) = overrides;
            let mut config = config();
            config.endpoint = endpoint.to_string();
            config.region = region.to_string();
            config.path_style = path_style;

            let (scheme, host, path) = storage(config).object_url("gd/sub/a.gif");
            assert_eq!(
                (scheme.as_str(), host.as_str(), path.as_str()),
                expected,
                "用例 #{label}"
            );
        }
    }

    #[test]
    fn payload_hash_covers_exactly_the_bytes_sent() {
        let signed = storage(config());
        let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

        assert_eq!(signed.payload_hash(&HttpBody::Empty).unwrap(), empty_hash);
        assert_eq!(
            signed
                .payload_hash(&HttpBody::Bytes(b"abc".to_vec()))
                .unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // 分片哈希必须只覆盖切下的字节，而不是整个文件
        let dir = std::env::temp_dir().join(format!("aetherupload-s3-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("slice.bin");
        std::fs::write(&file, b"0123456789").unwrap();

        assert_eq!(
            signed
                .payload_hash(&HttpBody::File {
                    path: file.clone(),
                    offset: 2,
                    len: 3,
                })
                .unwrap(),
            sigv4::sha256_hex(b"234")
        );

        // unsigned：哈希位换成占位符
        let mut unsigned = config();
        unsigned.payload_signing = PayloadSigning::Unsigned;
        assert_eq!(
            storage(unsigned)
                .payload_hash(&HttpBody::File {
                    path: file.clone(),
                    offset: 0,
                    len: 10,
                })
                .unwrap(),
            UNSIGNED_PAYLOAD
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_type_is_detected_from_content_like_php_mime_content_type() {
        let dir = std::env::temp_dir().join(format!("aetherupload-s3-mime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let gif = dir.join("a.gif");
        std::fs::write(&gif, b"GIF89a\x01\x00\x01\x00").unwrap();
        assert_eq!(detect_content_type(&gif), "image/gif");

        let unknown = dir.join("blob.bin");
        std::fs::write(&unknown, [0x00u8, 0x01, 0x02, 0x03]).unwrap();
        assert_eq!(detect_content_type(&unknown), "application/octet-stream");

        // 探测不到（文件不存在）时也要回落到 octet-stream，而不是让 mime 决定成败
        assert_eq!(
            detect_content_type(&dir.join("missing")),
            "application/octet-stream"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn u64_epoch_formats_as_utc_amz_date() {
        assert_eq!(format_amz_date(0), "19700101T000000Z");
        assert_eq!(format_amz_date(1_234_567_890), "20090213T233130Z");
        assert_eq!(format_amz_date(1_500_000_000), "20170714T024000Z");
        assert_eq!(format_amz_date(1_759_680_000), "20251005T160000Z");
        assert_eq!(format_amz_date(4_102_444_800), "21000101T000000Z");

        // 时钟格式必须与签名要求的 `YYYYMMDDTHHMMSSZ` 严丝合缝
        let now = amz_date_now();
        assert_eq!(now.len(), 16);
        assert!(now.ends_with('Z') && now.as_bytes()[8] == b'T');
    }

    #[test]
    fn xml_helpers_round_trip_the_multipart_payloads() {
        assert_eq!(xml_tag("<UploadId>u-1</UploadId>", "UploadId"), Some("u-1"));
        assert_eq!(xml_tag("<Code>X</Code>x</Code>", "Code"), Some("X"));
        assert_eq!(xml_tag("no tags", "UploadId"), None);

        assert_eq!(xml_unescape("u&amp;1"), "u&1");
        assert_eq!(xml_unescape("&#65;&#x42;"), "AB");
        // 非实体原样保留
        assert_eq!(xml_unescape("a&b"), "a&b");
        assert_eq!(xml_unescape("&unknown;"), "&unknown;");

        // ETag 的双引号是值的一部分，必须原样进 XML
        assert_eq!(xml_escape_text("\"e1\""), "\"e1\"");
        assert_eq!(xml_escape_text("a&b<c"), "a&amp;b&lt;c");
    }

    #[test]
    fn failure_messages_redact_credentials_but_keep_diagnostics() {
        let storage = storage(config());
        let response = HttpResponse {
            status: 403,
            headers: Vec::new(),
            body: b"<Error><Code>SignatureDoesNotMatch</Code><Message>Our services are not \
                    available AKIDEXAMPLE for you</Message>\
                    <AWSAccessKeyId>AKIAIOSFODNN7EXAMPLE</AWSAccessKeyId></Error>"
                .to_vec(),
        };

        let message = storage
            .fail("PUT", "up/gd/sub/a.gif", &response)
            .to_string();

        assert!(message.contains("HTTP 403"));
        assert!(message.contains("SignatureDoesNotMatch"));
        assert!(message.contains("up/gd/sub/a.gif"));
        // 本配置的 key 与任何 AWSAccessKeyId 标签都不外泄
        assert!(!message.contains("AKIDEXAMPLE"));
        assert!(!message.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(message.contains("***"));
        // secret 从不参与任何消息
        assert!(!message.contains(&storage.config.secret_key));
    }

    #[test]
    fn failure_message_falls_back_to_the_body_prefix() {
        let storage = storage(config());
        let response = HttpResponse {
            status: 500,
            headers: Vec::new(),
            body: b"<html>gateway error</html>".to_vec(),
        };

        let message = format!("{}", storage.fail("HEAD", "k", &response));
        assert!(message.contains("<html>gateway error</html>"), "{message}");
    }
}
