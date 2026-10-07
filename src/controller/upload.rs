// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传入口 —— 对应 PHP 版 `UploadController.php`（`preprocess` + `saveChunk`）。
//!
//! 两个入口的**每一条安全语义都照搬**，因为它们是这套协议多年补出来的：
//!
//! - 客户端可控的路径分量一律过白名单（`is_safe_path_component`），
//!   `group_subdir` 额外禁止下划线（会破坏 `savedPath` 三段式解码）；
//! - 分块序号必须是纯数字、≥1，总数上限 10000（防刷）；
//! - 有效分块失败（网络截断）**不清理**已拼好的进度，让客户端重传；
//!   序号跳变同样报错但不清理；
//! - 增量大小校验（已落盘 + 本块），不是只在末块校验；
//! - 白名单为空时额外硬拒可执行扩展名；
//! - 末块：大小 → 真实 MIME → 整份 md5 与客户端声明比对（`lax_mode` 关闭时），
//!   不一致即整份丢弃。
//!
//! 与 PHP 的一处**有意差异**：PHP 的 `fail()` 把已知异常统一折叠成 `upload_error`
//! （它比较的是「译文」与「错误键」，实际不相等），Rust 版按错误种类给出具体文案。
//! 客户端只判断 `error` 的真值、不解析内容，因此协议兼容；详见 README「与 PHP 版的差异」。

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::i18n::DEFAULT_LOCALE;
use crate::instant::InstantIndex;
use crate::json::{PreprocessResult, SaveChunkResult};
use crate::partial::{ChunkSource, PartialResource};
use crate::resource::Resource;
use crate::runtime::Runtime;
use crate::saved_path::SavedPath;
use crate::util::{self, get_file_name, is_safe_path_component};

/// `chunk_total` 上限：防「切一万块」式的资源耗尽（与 PHP 一致）。
const MAX_CHUNK_TOTAL: u64 = 10_000;

/// 白名单为空时的兜底黑名单：即使 `forbidden_extensions` 被清空，
/// 这些「明显可执行」的扩展名仍然硬拒（与 PHP 的内联列表逐项一致）。
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "php", "phtml", "php3", "php4", "php5", "phps", "pht", "shtml", "shtm", "jsp", "asp", "aspx",
    "cgi", "sh",
];

/// `preprocess` 的入参。字段名与前端表单字段一致。
#[derive(Debug, Clone, Default)]
pub struct PreprocessRequest {
    pub resource_name: Option<String>,
    pub resource_size: Option<String>,
    pub group: Option<String>,
    pub resource_hash: Option<String>,
    pub locale: Option<String>,
}

/// `saveChunk` 的入参。
#[derive(Debug, Clone, Default)]
pub struct SaveChunkRequest {
    pub chunk_total: Option<String>,
    pub chunk_index: Option<String>,
    pub resource_temp_basename: Option<String>,
    pub resource_ext: Option<String>,
    pub group_subdir: Option<String>,
    pub group: Option<String>,
    pub resource_hash: Option<String>,
    pub locale: Option<String>,
    /// 分块本体：适配器已把请求体落成临时文件或读进内存。
    pub chunk: Option<ChunkBody>,
}

/// 分块数据来源（拥有版；[`ChunkSource`] 是它的借用视图）。
#[derive(Debug, Clone)]
pub enum ChunkBody {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

impl ChunkBody {
    fn as_source(&self) -> ChunkSource<'_> {
        match self {
            Self::Path(path) => ChunkSource::Path(path),
            Self::Bytes(bytes) => ChunkSource::Bytes(bytes),
        }
    }

    fn len(&self) -> u64 {
        match self {
            Self::Path(path) => std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

pub struct UploadController {
    runtime: Arc<Runtime>,
}

impl UploadController {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// 预处理：校验参数与分组 → 生成临时名 → 判定秒传 → 建 `.part` 与断点文件。
    pub fn preprocess(&self, request: &PreprocessRequest) -> PreprocessResult {
        let locale = request.locale.as_deref().unwrap_or(DEFAULT_LOCALE);

        // 必填缺失：直接报参数错误（PHP 的 validatedWithError 分支）
        let (Some(resource_name), Some(resource_size), Some(group)) = (
            request.resource_name.as_deref(),
            request.resource_size.as_deref(),
            request.group.as_deref(),
        ) else {
            return PreprocessResult::fail(Some(Error::InvalidResourceParams.localized(locale)));
        };

        let mut partial: Option<PartialResource> = None;

        match self.run_preprocess(
            resource_name,
            resource_size,
            group,
            request.resource_hash.as_deref(),
            &mut partial,
        ) {
            Ok(response) => response,
            Err(err) => {
                // 出错要把已建的临时文件与断点清掉（PHP 的 catch 分支）
                if let Some(partial) = &partial {
                    partial.cleanup();
                }

                PreprocessResult::fail(Some(err.localized(locale)))
            }
        }
    }

    fn run_preprocess(
        &self,
        resource_name: &str,
        resource_size: &str,
        group: &str,
        resource_hash: Option<&str>,
        partial_slot: &mut Option<PartialResource>,
    ) -> Result<PreprocessResult> {
        // 分组解析（含「含下划线一律拒」）—— 对应 applyGroupConfig
        let snapshot = self.runtime.config().resolve_group(group)?;

        let temp_base_name = util::generate_temp_name();
        let resource_ext = util::extension_of(resource_name);
        let group_sub_dir = util::generate_sub_dir_name(snapshot.resource_subdir_rule);

        // 构造即入槽：PHP 在 `new PartialResource(...)` 之后才做这些校验，
        // 失败由 catch 统一 cleanup（连 `.part` 与 header 一起清）。位置对齐，
        // 之后任何 `?` 都会让调用方清干净。
        *partial_slot = Some(PartialResource::new(
            self.runtime.upload_root(),
            &snapshot,
            &temp_base_name,
            &resource_ext,
            &group_sub_dir,
        ));

        let partial = partial_slot.as_ref().expect("刚刚放入");

        // 先做参数侧校验，再决定是否落盘（PHP 的顺序一致）
        partial.filter_by_size(parse_size(resource_size))?;
        partial.filter_by_extension(&resource_ext)?;

        let mut response = PreprocessResult {
            error: None,
            chunk_size: snapshot.chunk_size,
            group_sub_dir: group_sub_dir.clone(),
            resource_temp_base_name: temp_base_name.clone(),
            resource_ext: resource_ext.clone(),
            saved_path: String::new(),
        };

        // 秒传判定：命中即返回既有路径，一个分块都不用传
        if snapshot.instant_completion
            && let Some(saved_path) = resource_hash
                .filter(|hash| !hash.is_empty())
                .and_then(|hash| self.instant_lookup(group, hash))
        {
            response.saved_path = saved_path;
            return Ok(response);
        }

        partial.create()?;
        partial.set_chunk_index(0)?;

        Ok(response)
    }

    /// 分块写入：校验 → 追加 → 回写断点；末块走完整校验并落盘。
    pub fn save_chunk(&self, request: &SaveChunkRequest) -> SaveChunkResult {
        let locale = request.locale.as_deref().unwrap_or(DEFAULT_LOCALE);

        // 必填缺失 → 参数错误（不涉及清理）
        let (
            Some(chunk_total),
            Some(chunk_index),
            Some(temp_base_name),
            Some(resource_ext),
            Some(group_subdir),
            Some(group),
        ) = (
            request.chunk_total.as_deref(),
            request.chunk_index.as_deref(),
            request.resource_temp_basename.as_deref(),
            request.resource_ext.as_deref(),
            request.group_subdir.as_deref(),
            request.group.as_deref(),
        )
        else {
            return SaveChunkResult::fail(Some(Error::InvalidResourceParams.localized(locale)));
        };

        // 客户端可控的路径分量白名单（PHP 的三个 isSafePathComponent 检查）
        for component in [group_subdir, temp_base_name, resource_ext] {
            if !is_safe_path_component(component, false, Some(64)) {
                return SaveChunkResult::fail(Some(Error::InvalidResourceParams.localized(locale)));
            }
        }

        // group_subdir 参与 savedPath 拼接，含下划线会让解码错位、资源永久 404
        if group_subdir.contains('_') {
            return SaveChunkResult::fail(Some(Error::InvalidResourceParams.localized(locale)));
        }

        // 分块序号：纯数字、≥1、总数有上限
        let (Some(chunk_index), Some(chunk_total)) =
            (parse_positive(chunk_index), parse_positive(chunk_total))
        else {
            return SaveChunkResult::fail(Some(Error::InvalidResourceParams.localized(locale)));
        };

        if chunk_total > MAX_CHUNK_TOTAL {
            return SaveChunkResult::fail(Some(Error::InvalidResourceParams.localized(locale)));
        }

        let mut partial_slot: Option<PartialResource> = None;

        match self.run_save_chunk(
            chunk_index,
            chunk_total,
            temp_base_name,
            resource_ext,
            group_subdir,
            group,
            request.resource_hash.as_deref(),
            request.chunk.as_ref(),
            locale,
            &mut partial_slot,
        ) {
            Ok(ChunkFlow::Accepted(saved_path)) => SaveChunkResult {
                error: None,
                saved_path,
            },
            // 「无效分块/序号跳变」这类：报错但**保留**进度，客户端可重传该块
            Ok(ChunkFlow::Rejected(message)) => SaveChunkResult::fail(Some(message)),
            Err(err) => {
                if let Some(partial) = &partial_slot {
                    partial.cleanup();
                }

                SaveChunkResult::fail(Some(err.localized(locale)))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_save_chunk(
        &self,
        chunk_index: u64,
        chunk_total: u64,
        temp_base_name: &str,
        resource_ext: &str,
        group_subdir: &str,
        group: &str,
        resource_hash: Option<&str>,
        chunk: Option<&ChunkBody>,
        locale: &str,
        partial_slot: &mut Option<PartialResource>,
    ) -> Result<ChunkFlow> {
        let snapshot = self.runtime.config().resolve_group(group)?;

        // 事件开关与白名单在本请求内快照（PHP 注释里的「快照 group 配置」）
        let event_before = snapshot.event_before_upload_complete;
        let event_complete = snapshot.event_upload_complete;
        let instant_completion = snapshot.instant_completion;
        let lax_mode = snapshot.lax_mode;

        // hash 是客户端可控值：非空时必须是安全字符集（宽松模式下允许为空）
        let resource_hash = resource_hash.filter(|hash| !hash.is_empty());
        if let Some(hash) = resource_hash
            && !is_safe_path_component(hash, false, Some(64))
        {
            return Err(Error::InvalidOperation);
        }

        *partial_slot = Some(PartialResource::new(
            self.runtime.upload_root(),
            &snapshot,
            temp_base_name,
            resource_ext,
            group_subdir,
        ));

        let partial = partial_slot.as_ref().expect("刚刚放入");

        // resource_ext 来自客户端：与预处理同样要过一遍扩展名过滤
        partial.filter_by_extension(resource_ext)?;

        // 白名单为空时，黑名单可能被使用者清空 —— 可执行扩展名再硬拒一层
        if snapshot.resource_extensions.is_empty() && EXECUTABLE_EXTENSIONS.contains(&resource_ext)
        {
            return Err(Error::InvalidResourceType);
        }

        // 秒传判定放在 exists() 之前：完成后再重发末块是幂等的
        if instant_completion
            && let Some(saved_path) =
                resource_hash.and_then(|hash| self.instant_lookup(group, hash))
        {
            if partial.exists() {
                partial.cleanup();
            }

            return Ok(ChunkFlow::Accepted(saved_path));
        }

        if !partial.exists() {
            return Err(Error::InvalidOperation);
        }

        // 分块本体缺失/不可读：报上传错误但**不清理**（PHP 的 reportError 分支）
        let Some(chunk) = chunk else {
            return Ok(ChunkFlow::Rejected(Error::UploadError.localized(locale)));
        };

        // 同一临时名的分块写入串行化：整段「读断点 → 校验 → 追加 → 回写断点」都在
        // 一把排他锁里。只给单次读写加锁不够 —— 两个并发请求会双双读到同一个 last、
        // 双双通过「序号 == last + 1」检查，然后各追加一次（分块多一块 → 末块大小
        // 校验失败 → 整份上传被 cleanup，实测 400 轮里 74.5%）。
        let guard = partial.header.lock_exclusive()?;

        // 断点比对：重发已收块幂等跳过；跳变（中间缺块）报错且不清理
        let last_chunk_index = guard.read_index()?;

        if chunk_index <= last_chunk_index {
            return Ok(ChunkFlow::Accepted(String::new()));
        }

        if chunk_index > last_chunk_index + 1 {
            return Ok(ChunkFlow::Rejected(Error::UploadError.localized(locale)));
        }

        // 增量大小校验：已落盘 + 本块
        let landed = std::fs::metadata(&partial.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        partial.filter_by_size(landed + chunk.len())?;

        partial.append(chunk.as_source())?;
        guard.write_index(chunk_index)?;

        if chunk_index != chunk_total {
            return Ok(ChunkFlow::Accepted(String::new()));
        }

        // —— 末块：完整校验 + 落盘 ——
        partial.check_size()?;
        partial.check_mime_type(self.runtime.detector(), &snapshot.extra_mime_types)?;

        if event_before {
            emit_safely(|| self.runtime.events().before_upload_complete(partial));
        }

        let real_hash = partial.calculate_hash()?;

        // 完整性校验：客户端声明的 hash 与真实内容不符 → 整份丢弃
        if !lax_mode && resource_hash != Some(real_hash.as_str()) {
            return Err(Error::UploadError);
        }

        let complete_name = get_file_name(&real_hash, resource_ext);
        partial.publish(self.runtime.storage(), &complete_name)?;

        let saved_path = SavedPath::encode(group, group_subdir, &complete_name);

        // 秒传索引：只在客户端提供了 hash 时写入（空 hash 会污染后续上传）
        if instant_completion
            && let Some(hash) = resource_hash
            && let Ok(key) = InstantIndex::key(group, hash)
        {
            let _ = self.runtime.instant_index().set(&key, &saved_path);
        }

        // 删断点文件（PHP 的 unset $partialResource->chunkIndex）；
        // 先放锁再删，免得并发重发者拿着刚被 unlink 的 inode 的锁（无实害，更干净）
        drop(guard);
        let _ = partial.clear_chunk_index();

        if event_complete {
            let resource = Resource::new(
                self.runtime.upload_root(),
                group,
                &partial.group_dir,
                group_subdir,
                &complete_name,
            );
            emit_safely(|| self.runtime.events().upload_complete(&resource));
        }

        Ok(ChunkFlow::Accepted(saved_path))
    }

    /// 秒传查询：任何失败（键非法、后端不可用、未命中）都视作「没有命中」——
    /// 与 PHP 里那段 `try { ... } catch { $savedPath = null; }` 一致。
    fn instant_lookup(&self, group: &str, hash: &str) -> Option<String> {
        let key = InstantIndex::key(group, hash).ok()?;

        self.runtime
            .instant_index()
            .get(&key)
            .ok()
            .flatten()
            .filter(|saved_path| !saved_path.is_empty())
    }
}

/// 分块流程的两种「没抛错」结局。
enum ChunkFlow {
    /// 被接受：末块或秒传命中时带 `savedPath`，中间块为空串。
    Accepted(String),
    /// 明确拒绝：报错但保留进度（客户端重传该块即可）。
    Rejected(String),
}

/// `(int)` 语义：非数字与负数一律折成 0（PHP 里 `(int)"abc" === 0`）。
fn parse_size(value: &str) -> u64 {
    value
        .trim()
        .parse::<i64>()
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
}

/// 严格正整数：PHP 用 `ctype_digit` + `(int) >= 1` 判分块序号。
fn parse_positive(value: &str) -> Option<u64> {
    let trimmed = value.trim();

    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    trimmed.parse::<u64>().ok().filter(|n| *n >= 1)
}

/// 事件回调的隔离执行：监听器 panic 不得影响上传响应（PHP 的可观察行为 ——
/// 监听器抛异常不冒泡，上传照常成功）。panic 消息仍会走标准 panic hook 打到 stderr，
/// 不是完全静默。
fn emit_safely<F: FnOnce()>(callback: F) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
}
