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

    /// 取得**排他锁并保持到守卫释放** —— 给「读断点 → 校验 → 追加 → 回写断点」
    /// 这类读-改-写序列用。
    ///
    /// 只对单次 read/write 加锁不够：两个并发请求会**双双**读到同一个 `last`、
    /// 双双通过「序号 == last + 1」检查，然后各追加一次 —— 分块文件多出一块，
    /// 末块大小校验失败 → 错误分支 cleanup() 把整份上传删掉（实测 400 轮里 74.5%）。
    /// 同一临时名的分块写入本就该串行，这里用一把持有整段的锁把它串起来。
    pub fn lock_exclusive(&self) -> Result<HeaderGuard<'_>> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.real_path)
            .map_err(|_| Error::WriteHeaderFail)?;

        file.lock().map_err(|_| Error::WriteHeaderFail)?;

        Ok(HeaderGuard {
            file,
            marker: std::marker::PhantomData,
        })
    }

    pub fn read(&self) -> Result<String> {
        let mut file = File::open(&self.real_path).map_err(|_| Error::ReadHeaderFail)?;

        // **共享锁**：与 write() 的排他锁互斥。少了它会撞进 write 的
        // 「set_len(0) → write_all」窗口，读到空文件 → ReadHeaderFail →
        // save_chunk 的错误分支把整份上传的 `.part` 与断点一起清掉（数据销毁级的并发竞态）。
        file.lock_shared().map_err(|_| Error::ReadHeaderFail)?;

        let mut content = String::new();
        let read = file.read_to_string(&mut content);

        let _ = file.unlock();

        read.map_err(|_| Error::ReadHeaderFail)?;

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

/// [`Header::lock_exclusive`] 的 RAII 守卫：构造即持锁，Drop 即释放。
///
/// 用它的那一段必须把「读断点 → 校验 → 追加 → 回写断点」整段包住 —— 这才是
/// 同一临时名的分块写入的原子单位（对应 PHP 版里没有、但并发下必需的那把锁）。
pub struct HeaderGuard<'a> {
    file: File,
    // 只用于 Debug 展示是哪个断点被锁住；锁随 file 释放
    #[allow(dead_code)]
    marker: std::marker::PhantomData<&'a Header>,
}

impl HeaderGuard<'_> {
    /// 在锁内读断点。
    pub fn read_index(&self) -> Result<u64> {
        let mut content = String::new();
        let mut file = &self.file;

        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_string(&mut content))
            .map_err(|_| Error::ReadHeaderFail)?;

        content
            .trim()
            .parse::<u64>()
            .map_err(|_| Error::ReadHeaderFail)
    }

    /// 在锁内回写断点。
    pub fn write_index(&self, index: u64) -> Result<()> {
        let mut file = &self.file;

        let result = (|| -> std::io::Result<()> {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(index.to_string().as_bytes())?;
            file.flush()
        })();

        result.map_err(|_| Error::WriteHeaderFail)
    }
}

impl Drop for HeaderGuard<'_> {
    fn drop(&mut self) {
        let _ = self.file.unlock();
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

    /// 读写交错：`write` 的排他锁与 `read` 的共享锁必须互斥，
    /// 否则读者会撞进「truncate 之后、write 之前」的空窗口（单测里表现为偶发 ReadHeaderFail）。
    #[test]
    fn concurrent_reads_never_observe_the_truncate_window() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = temp_root("rw-race");
        let header = Arc::new(Header::new(&root, "race"));
        header.write("1").unwrap();

        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let header = Arc::clone(&header);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut n = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    header.write(&n.to_string()).unwrap();
                }
            })
        };

        // 2000 次读取：每一次读到的都必须是「能解析成数字」的内容 —— 空读即竞态
        for _ in 0..2000 {
            let value = header.read_index().expect("读写必须互斥：读到空内容即竞态");
            assert!(value >= 1, "断点值必须来自某次完整写入");
        }

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

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
