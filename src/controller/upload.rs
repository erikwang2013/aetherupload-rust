// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传入口（`preprocess` + `saveChunk`）—— 内核的用例层。
//!
//! 失败一律是 [`Problem`] 问题响应，成功是 [`PreprocessOk`] / [`SaveChunkOk`]：
//! 2.0 起没有「200 兜底 + error 字段」。
//!
//! 安全语义自 1.x 照搬，2.0 明确了几处「可恢复」与「整份作废」的分界：
//! **可恢复**（报问题响应、绝不 cleanup，客户端重传即可）＝ `.part`/断点缺失或不可
//! 解析（409）、序号不连续（409，唯一接受 `index == last + 1`）、分块本体缺失；
//! 其余失败（参数/分组/大小/类型/md5/IO）按 1.x 语义**整份作废**（连 `.part` 与断点
//! 一起清）。客户端可控的路径分量一律过白名单，安全检查只针对**派生出的扩展名**
//! （`resource_name` 从不作为路径分量落盘），正常中文/空格文件名不受影响。
//! 大小校验是增量的（已落盘 + 本块），外加单块 ≤ `chunk_size`；白名单为空时额外硬拒
//! 可执行扩展名；末块：大小上限 → 真实 MIME → 整份 md5 与客户端声明比对（非 lax）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result, error_problem};
use crate::instant::InstantIndex;
use crate::json::Problem;
use crate::partial::{ChunkSource, PartialResource};
use crate::resource::Resource;
use crate::runtime::Runtime;
use crate::saved_path::SavedPath;
use crate::util::{self, get_file_name, is_safe_path_component};

/// 200 响应体的拼装住在 [`crate::json`]，这里原样再导出（调用方按
/// `controller::upload_ok_to_json` 用，路径与 1.x 习惯一致）。
pub use crate::json::{save_chunk_ok_to_json, upload_ok_to_json};

/// `chunk_total` 上限：防「切一万块」式的资源耗尽。
const MAX_CHUNK_TOTAL: u64 = 10_000;

/// 白名单为空时的兜底黑名单：即使 `forbidden_extensions` 被清空，这些「明显可执行」
/// 的扩展名仍然硬拒（`php` 等是文件扩展名，不是语言引用）。
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
}

/// `preprocess` 的成功体（HTTP 200 的上传结果，snake_case）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreprocessOk {
    pub chunk_size: u64,
    pub resource_temp_base_name: String,
    pub group_sub_dir: String,
    pub resource_ext: String,
    /// 秒传命中时非空，否则空串。
    pub saved_path: String,
}

/// `saveChunk` 的成功体；`saved_path` 非空 = 末块落盘或秒传命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveChunkOk {
    pub saved_path: String,
}

/// 已校验的分块参数（`save_chunk` 解析后传进内部流程，避免长参数表）。
struct ChunkParams<'a> {
    chunk_index: u64,
    chunk_total: u64,
    temp_base_name: &'a str,
    resource_ext: &'a str,
    group_subdir: &'a str,
    group: &'a str,
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
    pub fn preprocess(
        &self,
        request: &PreprocessRequest,
    ) -> std::result::Result<PreprocessOk, Problem> {
        let locale = request.locale.as_deref();
        let (Some(resource_name), Some(resource_size), Some(group)) = (
            request.resource_name.as_deref(),
            request.resource_size.as_deref(),
            request.group.as_deref(),
        ) else {
            return Err(bad_parameter(locale));
        };

        // `resource_size` 从严：非十进制数字与 0 都在参数层拒（1.x 折成 0 再报大小非法）
        let Some(resource_size) = parse_resource_size(resource_size) else {
            return Err(bad_parameter(locale));
        };

        let mut partial: Option<PartialResource> = None;

        match self.run_preprocess(
            resource_name,
            resource_size,
            group,
            request.resource_hash.as_deref(),
            &mut partial,
        ) {
            Ok(ok) => Ok(ok),
            Err(err) => {
                // 整份作废：参数/分组/类型/大小/内部故障都把临时文件与断点一起清
                if let Some(partial) = &partial {
                    partial.cleanup();
                }
                Err(error_problem(&err, locale))
            }
        }
    }

    fn run_preprocess(
        &self,
        resource_name: &str,
        resource_size: u64,
        group: &str,
        resource_hash: Option<&str>,
        partial_slot: &mut Option<PartialResource>,
    ) -> Result<PreprocessOk> {
        // 分组解析（含「含下划线一律拒」）：未配置/名字非法都是分组问题，不是内部故障
        let snapshot = self
            .runtime
            .config()
            .resolve_group(group)
            .map_err(|_| Error::InvalidGroup)?;

        let temp_base_name = util::generate_temp_name();
        let resource_ext = util::extension_of(resource_name);
        let group_sub_dir = util::generate_sub_dir_name(snapshot.resource_subdir_rule);

        // 扩展名先过安全字符集（`resource_name` 整体不进安全检查，见模块文档）
        if !is_safe_path_component(&resource_ext, false, Some(64)) {
            return Err(Error::InvalidResourceParams);
        }
        // 声明大小超分组上限 → 参数问题（累计/单块超限到分块阶段才是 413）
        if snapshot.resource_maxsize != 0 && resource_size > snapshot.resource_maxsize {
            return Err(Error::InvalidResourceParams);
        }

        // 构造即入槽：之后任何 `?` 都由调用方清干净（连 `.part` 与断点一起清）
        *partial_slot = Some(PartialResource::new(
            self.runtime.upload_root(),
            &snapshot,
            &temp_base_name,
            &resource_ext,
            &group_sub_dir,
        ));
        let partial = partial_slot.as_ref().expect("刚刚放入");
        partial.filter_by_extension(&resource_ext)?;

        // hash 是客户端可控值：非空时必须是 32 位十六进制（`lax_mode` 下不校验；空串＝
        // 客户端不提供 hash，保持既有「跳过比对」语义，不算错）
        let resource_hash = resource_hash.filter(|hash| !hash.is_empty());
        if let Some(hash) = resource_hash
            && !snapshot.lax_mode
            && !is_md5_hex(hash)
        {
            return Err(Error::InvalidResourceParams);
        }

        let mut ok = PreprocessOk {
            chunk_size: snapshot.chunk_size,
            resource_temp_base_name: temp_base_name,
            group_sub_dir,
            resource_ext,
            saved_path: String::new(),
        };

        // 秒传判定：命中即返回既有路径，一个分块都不用传
        if snapshot.instant_completion
            && let Some(saved_path) =
                resource_hash.and_then(|hash| self.instant_lookup(group, hash))
        {
            ok.saved_path = saved_path;
            return Ok(ok);
        }

        partial.create()?;
        partial.set_chunk_index(0)?;

        Ok(ok)
    }

    /// 分块写入：校验 → 追加 → 回写断点；末块走完整校验并落盘。
    pub fn save_chunk(
        &self,
        request: &SaveChunkRequest,
    ) -> std::result::Result<SaveChunkOk, Problem> {
        let locale = request.locale.as_deref();
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
            return Err(bad_parameter(locale));
        };

        // 客户端可控的路径分量白名单
        for component in [group_subdir, temp_base_name, resource_ext] {
            if !is_safe_path_component(component, false, Some(64)) {
                return Err(bad_parameter(locale));
            }
        }
        // group_subdir 参与 savedPath 拼接，含下划线会让解码错位、资源永久 404
        if group_subdir.contains('_') {
            return Err(bad_parameter(locale));
        }
        // 分块序号：纯数字、≥1、总数有上限、序号不超过总数
        let (Some(chunk_index), Some(chunk_total)) =
            (parse_positive(chunk_index), parse_positive(chunk_total))
        else {
            return Err(bad_parameter(locale));
        };
        if chunk_total > MAX_CHUNK_TOTAL || chunk_index > chunk_total {
            return Err(bad_parameter(locale));
        }

        let params = ChunkParams {
            chunk_index,
            chunk_total,
            temp_base_name,
            resource_ext,
            group_subdir,
            group,
        };
        let mut partial_slot: Option<PartialResource> = None;

        match self.run_save_chunk(
            &params,
            request.resource_hash.as_deref(),
            request.chunk.as_ref(),
            locale,
            &mut partial_slot,
        ) {
            Ok(ChunkFlow::Accepted(ok)) => Ok(ok),
            // 可恢复的失败：报问题响应但**保留**进度（客户端可按 `code` 重传或推进）
            Ok(ChunkFlow::Recoverable(problem)) => Err(problem),
            Err(err) => {
                // 整份作废：类型/大小/md5/IO 这类失败连 `.part` 与断点一起清
                if let Some(partial) = &partial_slot {
                    partial.cleanup();
                }
                Err(error_problem(&err, locale))
            }
        }
    }

    fn run_save_chunk(
        &self,
        params: &ChunkParams<'_>,
        resource_hash: Option<&str>,
        chunk: Option<&ChunkBody>,
        locale: Option<&str>,
        partial_slot: &mut Option<PartialResource>,
    ) -> Result<ChunkFlow> {
        let ChunkParams {
            chunk_index,
            chunk_total,
            temp_base_name,
            resource_ext,
            group_subdir,
            group,
        } = *params;
        let snapshot = self
            .runtime
            .config()
            .resolve_group(group)
            .map_err(|_| Error::InvalidGroup)?;

        // 事件开关与白名单在本请求内快照（中途改配置不影响这一次上传）
        let event_before = snapshot.event_before_upload_complete;
        let event_complete = snapshot.event_upload_complete;
        let instant_completion = snapshot.instant_completion;
        let lax_mode = snapshot.lax_mode;

        // hash 是客户端可控值：非空时必须是 32 位十六进制（`lax_mode` 下不校验）
        let resource_hash = resource_hash.filter(|hash| !hash.is_empty());
        if let Some(hash) = resource_hash
            && !lax_mode
            && !is_md5_hex(hash)
        {
            return Err(Error::InvalidResourceParams);
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

        // 秒传判定放在存在性检查之前：完成后再重发末块是幂等的
        if instant_completion
            && let Some(saved_path) =
                resource_hash.and_then(|hash| self.instant_lookup(group, hash))
        {
            if partial.exists() {
                partial.cleanup();
            }
            return Ok(ChunkFlow::Accepted(SaveChunkOk { saved_path }));
        }

        // 可恢复状态：`.part` 没了（已完成/已清理）或断点文件缺失 → 409，绝不 cleanup
        if !partial.exists() || !partial.header.exists() {
            return Ok(ChunkFlow::Recoverable(upload_not_prepared(locale)));
        }
        // 分块本体缺失：参数问题，但**不清理**（客户端重发该块即可）
        let Some(chunk) = chunk else {
            return Ok(ChunkFlow::Recoverable(bad_parameter(locale)));
        };

        // 同一临时名的分块写入串行化：整段「读断点 → 校验 → 追加 → 回写断点」都在一把
        // 排他锁里。只给单次读写加锁不够 —— 两个并发请求会双双读到同一个 last、双双
        // 通过检查再各追加一次，分块多一块，整份上传在末块大小校验处失败被 cleanup。
        let guard = partial.header.lock_exclusive()?;
        let last_chunk_index = match guard.read_index() {
            Ok(index) => index,
            Err(Error::ReadHeaderFail) => {
                // 断点内容空/不可解析 → 可恢复的 409；只有真 IO 故障（权限、磁盘）才 500
                if header_io_fault(&partial.header.real_path) {
                    return Err(Error::ReadHeaderFail);
                }
                return Ok(ChunkFlow::Recoverable(upload_not_prepared(locale)));
            }
            Err(err) => return Err(err),
        };

        // 序号：唯一被接受的是「服务端还没收到的那一块」（last + 1）。跳号、末块提前
        // 到达、重发已落块（index <= last）一律 409 + expected_index；前端按它区分
        // 「已落，静默推进」与「服务端状态丢了，从最近可续处退出」。
        let expected_index = last_chunk_index.saturating_add(1);
        if chunk_index != expected_index {
            return Ok(ChunkFlow::Recoverable(Problem::localized(
                409,
                "chunk_out_of_order",
                locale,
                vec![("expected_index", expected_index.to_string())],
            )));
        }

        // 大小：单块 ≤ `chunk_size`，且累计（已落盘 + 本块）不超分组上限 —— 413
        let chunk_len = match chunk {
            ChunkBody::Path(path) => path.metadata().map(|meta| meta.len()).unwrap_or(0),
            ChunkBody::Bytes(bytes) => bytes.len() as u64,
        };
        if chunk_len > snapshot.chunk_size {
            return Err(Error::InvalidResourceSize);
        }
        let landed = partial.path.metadata().map(|meta| meta.len()).unwrap_or(0);
        if partial.max_size != 0 && landed.saturating_add(chunk_len) > partial.max_size {
            return Err(Error::InvalidResourceSize);
        }

        partial.append(chunk.as_source())?;
        guard.write_index(chunk_index)?;

        if chunk_index != chunk_total {
            return Ok(ChunkFlow::Accepted(SaveChunkOk {
                saved_path: String::new(),
            }));
        }

        // —— 末块：大小上限 → 真实 MIME → 整份 md5 ——
        // 真实大小为 0 不单独报错：非 lax 时下面的 md5 比对会以 422 拒掉；`lax_mode`
        // 下接受（lax 的代价，见文档）。
        let real_size = partial
            .path
            .metadata()
            .map_err(|_| Error::UploadError)?
            .len();
        if partial.max_size != 0 && real_size > partial.max_size {
            return Err(Error::InvalidResourceSize);
        }
        partial.check_mime_type(self.runtime.detector(), &snapshot.extra_mime_types)?;

        if event_before {
            emit_safely(|| self.runtime.events().before_upload_complete(partial));
        }

        let real_hash = partial.calculate_hash()?;
        // 完整性校验：客户端声明的 hash 与真实内容不符 → 整份丢弃
        if !lax_mode && resource_hash != Some(real_hash.as_str()) {
            return Err(Error::HashMismatch);
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

        // 删断点文件；先放锁再删，免得并发重发者拿着刚被 unlink 的 inode 的锁
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

        Ok(ChunkFlow::Accepted(SaveChunkOk { saved_path }))
    }

    /// 秒传查询：任何失败（键非法、后端不可用、未命中）都视作「没有命中」——
    /// 绝不因为索引故障让正常上传失败。
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
    /// 成功：中间块 `saved_path` 为空；末块落盘或秒传命中时带 `saved_path`。
    Accepted(SaveChunkOk),
    /// 可恢复的失败：报问题响应但保留进度（客户端重传该块或推进）。
    Recoverable(Problem),
}

/// 参数缺失/非法（400 `invalid_parameter`）—— 入口处出现多次，收个小名。
fn bad_parameter(locale: Option<&str>) -> Problem {
    Problem::invalid_parameter(locale)
}

/// 未预处理 / 临时资源或断点已失效（409）。**调用点必须保证此路不 cleanup**。
fn upload_not_prepared(locale: Option<&str>) -> Problem {
    Problem::localized(409, "upload_not_prepared", locale, Vec::new())
}

/// `resource_hash` 的格式要求：32 位十六进制（1.x 只做了「安全字符集」检查，2.0 收紧）。
fn is_md5_hex(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 严格正整数：分块序号必须是十进制数字且 ≥ 1。
fn parse_positive(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u64>().ok().filter(|n| *n >= 1)
}

/// `resource_size` 从严解析：只认十进制正整数（1.x 把非数字折成 0 再报大小非法）。
fn parse_resource_size(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u64>().ok().filter(|n| *n > 0)
}

/// 断点读取失败时区分「真 IO 故障」（打不开/读不出 → 500）与「缺失/内容不可解析」
/// （可恢复的 409）：再读一次文件，只看错误种类。
fn header_io_fault(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(_) => false,
        Err(err) => err.kind() != std::io::ErrorKind::NotFound,
    }
}

/// 事件回调的隔离执行：监听器 panic 不得影响上传响应（监听器抛异常不冒泡、上传照常
/// 成功是 1.x 的可观察行为）。panic 消息仍会经标准 panic hook 打到 stderr。
fn emit_safely<F: FnOnce()>(callback: F) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_are_strict() {
        assert_eq!(parse_positive("1"), Some(1));
        assert_eq!(parse_positive(" 42 "), Some(42));
        for bad in ["0", "1.5", "-1", ""] {
            assert_eq!(parse_positive(bad), None, "{bad}");
        }

        assert_eq!(parse_resource_size("2048"), Some(2048));
        for bad in ["0", "5.5", "1e3", "-3", ""] {
            assert_eq!(parse_resource_size(bad), None, "{bad}");
        }

        assert!(is_md5_hex("d41d8cd98f00b204e9800998ecf8427e"));
        assert!(!is_md5_hex("zzz"));
        assert!(!is_md5_hex(""));
    }

    /// 断点读取失败的两分：缺失 → 可恢复；真 IO 故障 → 500。
    #[test]
    fn header_io_fault_only_flags_real_io_errors() {
        let missing = std::env::temp_dir().join("aetherupload-no-such-header");
        assert!(!header_io_fault(&missing), "缺失是可恢复状态，不是 IO 故障");
        // 目录当文件读 → IsADirectory 之类的真 IO 故障
        assert!(header_io_fault(&std::env::temp_dir()));
    }
}
