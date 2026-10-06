// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传中的分块文件 —— 对应 PHP 版 `PartialResource.php`。
//!
//! 磁盘上的形态是 `<root>/<group_dir>/<subdir>/<tempName>.<ext>.part`：
//! 一个空文件 + 逐块追加。文件校验（大小、MIME、整份 md5）都发生在末块，
//! 通过后由 [`PartialResource::publish`] 交给存储驱动改名落地。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::GroupSnapshot;
use crate::error::{Error, Result};
use crate::header::Header;
use crate::md5;
use crate::mime::{self, MimeDetector};
use crate::storage::Storage;
use crate::util::get_file_name;

/// 分块数据的来源：适配器把请求体交进来时既可能是临时文件（流式落盘过），
/// 也可能是内存里的字节（框架已把 multipart 解进内存）。
#[derive(Debug, Clone, Copy)]
pub enum ChunkSource<'a> {
    Path(&'a Path),
    Bytes(&'a [u8]),
}

#[derive(Debug)]
pub struct PartialResource {
    pub group: String,
    pub group_dir: String,
    pub group_sub_dir: String,
    /// 临时名：`<tempBaseName>.<ext>`（不含 `.part` 后缀）。
    pub temp_name: String,
    /// 绝对路径：`<upload_root>/<group_dir>/<subdir>/<temp_name>.part`。
    pub path: PathBuf,
    pub header: Header,
    pub max_size: u64,
    pub allowed_extensions: Vec<String>,
    pub forbidden_extensions: Vec<String>,
}

impl PartialResource {
    /// 组装一个临时资源。分组相关配置来自本次调用的快照（`GroupSnapshot`），
    /// 不从任何全局状态读取 —— 对应 PHP 构造函数里的那句「快照 group 配置」。
    pub fn new(
        upload_root: &Path,
        snapshot: &GroupSnapshot,
        temp_base_name: &str,
        extension: &str,
        group_sub_dir: &str,
    ) -> Self {
        let temp_name = get_file_name(temp_base_name, extension);
        let path = upload_root
            .join(&snapshot.group_dir)
            .join(group_sub_dir)
            .join(format!("{temp_name}.part"));

        Self {
            group: snapshot.group.clone(),
            group_dir: snapshot.group_dir.clone(),
            group_sub_dir: group_sub_dir.to_string(),
            temp_name,
            path,
            header: Header::new(upload_root, temp_base_name),
            max_size: snapshot.resource_maxsize,
            allowed_extensions: snapshot.resource_extensions.clone(),
            forbidden_extensions: snapshot.forbidden_extensions.clone(),
        }
    }

    /// 建分组子目录 + 建空的 `.part` 文件（预处理的落地动作）。
    pub fn create(&self) -> Result<()> {
        self.create_group_sub_dir()?;

        File::create(&self.path).map_err(|_| Error::CreateResourceFail)?;

        Ok(())
    }

    /// 追加一块数据。
    pub fn append(&self, chunk: ChunkSource<'_>) -> Result<()> {
        let mut target = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|_| Error::WriteResourceFail)?;

        target.lock().map_err(|_| Error::WriteResourceFail)?;

        // 错误码与 PHP 对齐：分块源打不开 → upload_error；写目标失败 → write_resource_fail
        let written = match chunk {
            ChunkSource::Path(source) => match File::open(source) {
                Ok(mut source) => {
                    std::io::copy(&mut source, &mut target).map_err(|_| Error::WriteResourceFail)
                }
                Err(_) => Err(Error::UploadError),
            },
            ChunkSource::Bytes(bytes) => target
                .write_all(bytes)
                .map(|()| bytes.len() as u64)
                .map_err(|_| Error::WriteResourceFail),
        };

        let _ = target.unlock();

        written.map(|_| ())
    }

    pub fn delete(&self) -> Result<()> {
        std::fs::remove_file(&self.path).map_err(|_| Error::DeleteResourceFail)
    }

    /// 清理：临时文件与断点文件一起删，**错误全部忽略** ——
    /// 它出现在错误路径上，清理失败不应掩盖原始异常（PHP 用 `@unlink` 的同一取向）。
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);

        if self.header.exists() {
            let _ = self.header.delete();
        }
    }

    /// 交给存储驱动落地（local = rename，s3 = 上传对象）。
    pub fn publish(&self, storage: &dyn Storage, complete_name: &str) -> Result<()> {
        storage.publish(
            &self.path,
            &self.group_dir,
            &self.group_sub_dir,
            complete_name,
        )
    }

    /// 声明大小校验 —— 对应 PHP `filterBySize()`：
    /// `0` 一律拒绝（客户端没算出大小或算错了），超过分组上限（上限非 0 时）也拒绝。
    pub fn filter_by_size(&self, resource_size: u64) -> Result<()> {
        if resource_size == 0 || (self.max_size != 0 && resource_size > self.max_size) {
            return Err(Error::InvalidResourceSize);
        }

        Ok(())
    }

    /// 扩展名校验 —— 对应 PHP `filterByExtension()`：
    /// 空扩展名拒绝；白名单非空时必须在白名单内；命中黑名单一律拒绝。
    pub fn filter_by_extension(&self, resource_ext: &str) -> Result<()> {
        let not_whitelisted = !self.allowed_extensions.is_empty()
            && !self.allowed_extensions.iter().any(|e| e == resource_ext);
        let blacklisted = self.forbidden_extensions.iter().any(|e| e == resource_ext);

        if resource_ext.is_empty() || not_whitelisted || blacklisted {
            return Err(Error::InvalidResourceType);
        }

        Ok(())
    }

    /// 落盘后的真实大小校验（末块第一步）。
    pub fn check_size(&self) -> Result<()> {
        let size = std::fs::metadata(&self.path)
            .map_err(|_| Error::UploadError)?
            .len();

        self.filter_by_size(size)
    }

    /// 内容类型复核（末块第二步）：按**真实内容**反查扩展名，再走一遍白/黑名单。
    pub fn check_mime_type(
        &self,
        detector: &dyn MimeDetector,
        extra_mime_types: &[(String, String)],
    ) -> Result<()> {
        let detected = detector.detect(&self.path)?.ok_or(Error::MissingMimetype)?;
        let extension = mime::search(&detected, extra_mime_types).ok_or(Error::MissingMimetype)?;

        self.filter_by_extension(&extension)
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// 断点：最后一块成功的序号。
    pub fn chunk_index(&self) -> Result<u64> {
        self.header.read_index()
    }

    pub fn set_chunk_index(&self, index: u64) -> Result<()> {
        self.header.write(&index.to_string())
    }

    /// 删除断点文件（上传完成后的收尾动作）。
    pub fn clear_chunk_index(&self) -> Result<()> {
        self.header.delete()
    }

    /// 整份内容 md5 —— 秒传索引键与成品文件名都来自它。
    pub fn calculate_hash(&self) -> Result<String> {
        md5::md5_file(&self.path).map_err(Error::Io)
    }

    /// 读取头部若干字节（测试与诊断用）。
    pub fn read_prefix(&self, limit: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; limit];
        let mut file = File::open(&self.path).map_err(|_| Error::UploadError)?;
        let read = file.read(&mut buf).map_err(|_| Error::UploadError)?;
        buf.truncate(read);

        Ok(buf)
    }

    /// 分组子目录的绝对路径（`<root>/<group_dir>/<subdir>`）。
    pub fn group_sub_dir_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.path.clone())
    }

    /// 子目录下的成品路径（改名前的目标位置）。
    pub fn complete_path(&self, name: &str) -> PathBuf {
        self.group_sub_dir_path().join(name)
    }

    /// 建分组子目录 —— 三条语义与 PHP `createGroupSubDir()` 一致：
    ///
    /// 1. **父目录（分组目录）必须已存在**，缺失直接失败。这是 `aetherupload:groups`
    ///    存在的原因：内核用非递归 `mkdir`，父目录没建好时报的是笼统的上传错误，
    ///    很难看出是目录问题。
    /// 2. 子目录已存在即成功（幂等）。
    /// 3. 并发首传时两个请求抢建同一个目录：抢输的一方 `mkdir` 报错，
    ///    **失败后再查一次**，目录在了就算成功。
    pub fn create_group_sub_dir(&self) -> Result<()> {
        let sub_dir = self.group_sub_dir_path();

        let Some(group_dir) = sub_dir.parent() else {
            return Err(Error::CreateSubfolderFail);
        };

        if !group_dir.is_dir() {
            return Err(Error::CreateSubfolderFail);
        }

        if !sub_dir.is_dir() {
            match std::fs::create_dir(&sub_dir) {
                Ok(()) => {}
                Err(_) if sub_dir.is_dir() => {}
                Err(_) => return Err(Error::CreateSubfolderFail),
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mime::MagicBytesDetector;

    fn setup(tag: &str) -> (PathBuf, GroupSnapshot) {
        let root =
            std::env::temp_dir().join(format!("aetherupload-partial-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // 分组目录与 _header 必须先建好（真实部署里由 `aetherupload groups` 建）
        std::fs::create_dir_all(root.join("file")).unwrap();
        std::fs::create_dir_all(root.join(crate::header::HEADER_DIR)).unwrap();

        let snapshot = Config::default().resolve_group("file").unwrap();
        (root, snapshot)
    }

    #[test]
    fn create_append_and_cleanup() {
        let (root, snapshot) = setup("append");
        let partial = PartialResource::new(&root, &snapshot, "0123456789abcdef", "txt", "202610");

        partial.create().unwrap();
        assert!(partial.exists());
        assert_eq!(std::fs::metadata(&partial.path).unwrap().len(), 0);

        partial.append(ChunkSource::Bytes(b"hello ")).unwrap();
        partial.append(ChunkSource::Bytes(b"world")).unwrap();
        assert_eq!(std::fs::read(&partial.path).unwrap(), b"hello world");

        // 断点读写
        partial.set_chunk_index(2).unwrap();
        assert_eq!(partial.chunk_index().unwrap(), 2);
        partial.clear_chunk_index().unwrap();
        assert!(!partial.header.exists());

        partial.cleanup();
        assert!(!partial.exists());
        assert!(!partial.header.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn append_from_path_source() {
        let (root, snapshot) = setup("path");
        let partial = PartialResource::new(&root, &snapshot, "aaaabbbbccccdddd", "txt", "202610");
        partial.create().unwrap();

        let chunk = root.join("chunk.bin");
        std::fs::write(&chunk, b"from-file").unwrap();

        partial.append(ChunkSource::Path(&chunk)).unwrap();
        assert_eq!(std::fs::read(&partial.path).unwrap(), b"from-file");

        // 缺失的分块源 → upload_error（不清理进度）
        let missing = root.join("nope.bin");
        assert!(matches!(
            partial.append(ChunkSource::Path(&missing)),
            Err(Error::UploadError)
        ));
        assert!(partial.exists(), "分块失败不应销毁已拼好的进度");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn size_and_extension_filters() {
        let (root, snapshot) = setup("filters");
        let partial = PartialResource::new(&root, &snapshot, "0123456789abcdef", "jpg", "202610");

        // 0 一律拒；超过分组上限拒；上限内放行
        assert!(matches!(
            partial.filter_by_size(0),
            Err(Error::InvalidResourceSize)
        ));
        assert!(matches!(
            partial.filter_by_size(snapshot.resource_maxsize + 1),
            Err(Error::InvalidResourceSize)
        ));
        partial.filter_by_size(1024).unwrap();

        // 白名单内放行；白名单外拒；黑名单拒；空扩展名拒
        partial.filter_by_extension("jpg").unwrap();
        assert!(matches!(
            partial.filter_by_extension("exe"),
            Err(Error::InvalidResourceType)
        ));
        assert!(matches!(
            partial.filter_by_extension("php"),
            Err(Error::InvalidResourceType)
        ));
        assert!(matches!(
            partial.filter_by_extension(""),
            Err(Error::InvalidResourceType)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mime_check_reads_real_content() {
        let (root, snapshot) = setup("mime");
        let partial = PartialResource::new(&root, &snapshot, "0123456789abcdef", "jpg", "202610");
        partial.create().unwrap();

        // 内容是 PNG，声明的是 jpg —— 白名单里两个都有，按内容反查到 png 即通过
        partial
            .append(ChunkSource::Bytes(b"\x89PNG\r\n\x1a\n...."))
            .unwrap();
        partial
            .check_mime_type(&MagicBytesDetector, &[])
            .expect("png 在白名单内");

        let _ = std::fs::remove_dir_all(&root);

        // 同一个文件在白名单只剩 jpg 的分组里会被拒（内容与声明不符）
        let (root, mut snapshot) = setup("mime2");
        snapshot.resource_extensions = vec!["jpg".to_string()];
        let partial = PartialResource::new(&root, &snapshot, "0123456789abcdef", "jpg", "202610");
        partial.create().unwrap();
        partial
            .append(ChunkSource::Bytes(b"\x89PNG\r\n\x1a\n...."))
            .unwrap();
        assert!(matches!(
            partial.check_mime_type(&MagicBytesDetector, &[]),
            Err(Error::InvalidResourceType)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_group_dir_is_a_subfolder_failure() {
        let (root, snapshot) = setup("nogroupdir");
        // 分组目录没建（模拟忘记跑 groups 命令）
        std::fs::remove_dir_all(root.join("file")).unwrap();

        let partial = PartialResource::new(&root, &snapshot, "0123456789abcdef", "txt", "202610");
        assert!(matches!(partial.create(), Err(Error::CreateSubfolderFail)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hash_and_complete_path() {
        let (root, snapshot) = setup("hash");
        let partial = PartialResource::new(&root, &snapshot, "0123456789abcdef", "txt", "202610");
        partial.create().unwrap();
        partial.append(ChunkSource::Bytes(b"abc")).unwrap();

        // 与 RFC 1321 的 "abc" 向量一致
        assert_eq!(
            partial.calculate_hash().unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert!(
            partial
                .complete_path("x.txt")
                .ends_with("file/202610/x.txt")
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
