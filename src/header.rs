// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 断点状态文件 —— 对应 PHP 版 `Header.php`。
//!
//! 每个上传中的临时资源对应 `_header/<tempBaseName>` 一个文件，内容只有一个
//! `chunkIndex`（最后一块成功的序号）。它是「断线续传」的唯一状态：刷新页面重来
//! 时，客户端从预处理重新拿 tempBaseName 之外，服务端凭 header 就知道进度。
//!
//! 写入采用「加锁 + 截断 + 覆写」：同一临时名的重试请求可能并发，PHP 版用
//! `flock(LOCK_EX)`，Rust 用 `std::fs::File::lock()`（同为 advisory 锁）。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// `_header` 目录名（与 PHP 一致，直接放在上传根目录下）。
pub const HEADER_DIR: &str = "_header";

#[derive(Debug, Clone)]
pub struct Header {
    /// 临时资源基础名（无扩展名）。
    pub name: String,
    /// 绝对路径：`<root>/_header/<name>`。
    pub real_path: PathBuf,
}

impl Header {
    pub fn new(root: &Path, temp_base_name: &str) -> Self {
        Self {
            name: temp_base_name.to_string(),
            real_path: root.join(HEADER_DIR).join(temp_base_name),
        }
    }

    /// 覆盖写入（内容即 `chunkIndex` 的十进制文本）。
    pub fn write(&self, content: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.real_path)
            .map_err(|_| Error::WriteHeaderFail)?;

        file.lock().map_err(|_| Error::WriteHeaderFail)?;

        let result = (|| -> std::io::Result<()> {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(content.as_bytes())?;
            file.flush()
        })();

        // 无论成败都解锁：锁随文件描述符关闭也会释放，显式 unlock 让错误路径更干净
        let _ = file.unlock();

        result.map_err(|_| Error::WriteHeaderFail)
    }

    pub fn read(&self) -> Result<String> {
        let mut content = String::new();

        File::open(&self.real_path)
            .and_then(|mut file| file.read_to_string(&mut content))
            .map_err(|_| Error::ReadHeaderFail)?;

        Ok(content)
    }

    /// 读取并解析成序号；内容不是数字时按 [`Error::ReadHeaderFail`] 处理。
    pub fn read_index(&self) -> Result<u64> {
        self.read()?
            .trim()
            .parse::<u64>()
            .map_err(|_| Error::ReadHeaderFail)
    }

    pub fn delete(&self) -> Result<()> {
        std::fs::remove_file(&self.real_path).map_err(|_| Error::DeleteHeaderFail)
    }

    pub fn exists(&self) -> bool {
        self.real_path.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aetherupload-header-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(HEADER_DIR)).unwrap();
        dir
    }

    #[test]
    fn write_read_delete_roundtrip() {
        let root = temp_root("roundtrip");
        let header = Header::new(&root, "0123456789abcdef");
        assert!(!header.exists());

        header.write("3").unwrap();
        assert!(header.exists());
        assert_eq!(header.read().unwrap(), "3");
        assert_eq!(header.read_index().unwrap(), 3);

        // 覆写而不是追加：旧内容必须消失
        header.write("17").unwrap();
        assert_eq!(header.read_index().unwrap(), 17);

        header.delete().unwrap();
        assert!(!header.exists());
        assert!(matches!(header.read(), Err(Error::ReadHeaderFail)));
        assert!(matches!(header.delete(), Err(Error::DeleteHeaderFail)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn garbage_content_is_a_read_failure() {
        let root = temp_root("garbage");
        let header = Header::new(&root, "abc");
        header.write("not-a-number").unwrap();
        assert!(matches!(header.read_index(), Err(Error::ReadHeaderFail)));
        let _ = std::fs::remove_dir_all(&root);
    }
}
