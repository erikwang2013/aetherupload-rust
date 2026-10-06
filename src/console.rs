// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 运维命令 —— 对应 PHP 版 `src/Console/*Runner.php`（宿主再包一层命令壳）。
//!
//! 三个动作原样搬过来，包括它们的「先说清在做什么、再如实报数」的输出习惯：
//!
//! - [`list_groups`]：建上传根目录与各分组目录（**不建就一定上传失败** —— 内核
//!   建子目录用的是非递归 `mkdir`，缺父目录时报的是笼统的上传错误）；
//! - [`build_redis_hashes`]：按磁盘现状重建秒传索引（每日跑，消除脏数据）；
//! - [`clean_up_directory`]：按 mtime 回收 `_header/` 与 `*.part` 的过期临时文件。
//!
//! Rust 版把 PHP 的 `callable $write` 换成 `&mut dyn FnMut(&str)`，返回值仍是退出码
//! （0 成功 / 1 失败），方便宿主直接接进自家的 CLI 或 cron 任务。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::instant::InstantIndex;
use crate::runtime::Runtime;
use crate::saved_path::SavedPath;
use crate::util::{extension_of, file_stem_of, is_safe_path_component};

/// `build` 每攒够这么多条就写一批（与 PHP 的 1000 一致）。
const BUILD_BATCH: usize = 1000;

fn say(write: &mut dyn FnMut(&str), message: impl AsRef<str>) {
    write(message.as_ref());
}

/// 列出分组并创建对应目录（`aetherupload groups`）。
pub fn list_groups(runtime: &Runtime, write: &mut dyn FnMut(&str)) -> i32 {
    match run_list_groups(runtime, write) {
        Ok(()) => 0,
        Err(err) => {
            say(write, format!("Error: {err}"));
            1
        }
    }
}

fn run_list_groups(runtime: &Runtime, write: &mut dyn FnMut(&str)) -> Result<()> {
    let root_dir = runtime.upload_root();

    if !root_dir.is_dir() {
        // 只有这一步是递归建目录：根目录与 _header 一起造出来
        std::fs::create_dir_all(root_dir.join(crate::header::HEADER_DIR))
            .map_err(|_| Error::CreateSubfolderFail)?;
        say(
            write,
            format!(
                "Root directory \"{}\" has been created.",
                root_dir.display()
            ),
        );
    }

    let existing: Vec<String> = std::fs::read_dir(root_dir)
        .map_err(Error::Io)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();

    // 分组目录逐个建（非递归，与 PHP 相同）；重复的 group_dir 只建一次
    let mut created: Vec<String> = Vec::new();

    for group in runtime.config().groups.values() {
        if group.group_dir.is_empty() || created.contains(&group.group_dir) {
            continue;
        }

        if existing.contains(&group.group_dir) {
            continue;
        }

        let path = root_dir.join(&group.group_dir);

        match std::fs::create_dir(&path) {
            Ok(()) => {
                created.push(group.group_dir.clone());
                say(
                    write,
                    format!("Directory \"{}\" has been created.", path.display()),
                );
            }
            Err(_) if path.is_dir() => {
                created.push(group.group_dir.clone());
            }
            Err(_) => {
                return Err(Error::Backend(format!(
                    "Fail to create directory \"{}\".",
                    path.display()
                )));
            }
        }
    }

    say(write, "Group-Directory List:");

    for (group_name, group) in &runtime.config().groups {
        // 含下划线的分组名会让 savedPath 解码错位 —— 建目录阶段就点出来
        if group_name.contains('_') {
            say(
                write,
                format!(
                    "Invalid group name \"{group_name}\": underscore is not allowed, rename the group."
                ),
            );
            continue;
        }

        let path = root_dir.join(&group.group_dir);
        if path.is_dir() {
            say(write, format!("{group_name} - {}", path.display()));
        }
    }

    Ok(())
}

/// 按磁盘现状重建秒传索引（`aetherupload build`）。
pub fn build_redis_hashes(runtime: &Runtime, write: &mut dyn FnMut(&str)) -> i32 {
    match run_build_redis_hashes(runtime, write) {
        Ok(()) => 0,
        Err(err) => {
            say(write, format!("Error: {err}"));
            1
        }
    }
}

fn run_build_redis_hashes(runtime: &Runtime, write: &mut dyn FnMut(&str)) -> Result<()> {
    say(write, "Start rebuilding the correlations...");

    let index = runtime.instant_index();

    // 先清空（旧版单 hash 与新前缀一起），避免留下已删除资源的死链
    index.delete_all()?;

    let mut batch: Vec<(String, String)> = Vec::new();
    let mut total = 0usize;

    for (group_name, group) in &runtime.config().groups {
        if group_name.contains('_') {
            say(
                write,
                format!("Invalid group name \"{group_name}\": underscore is not allowed, skipped."),
            );
            continue;
        }

        let group_path = runtime.upload_root().join(&group.group_dir);

        for sub_dir in read_dir_entries(&group_path, EntryKind::Dirs) {
            for file in read_dir_entries(&sub_dir, EntryKind::Files) {
                // 分块文件不是成品
                if extension_of(&file.to_string_lossy()) == "part" {
                    continue;
                }

                let Some(file_name) = file.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some(sub_dir_name) = sub_dir.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };

                let hash = file_stem_of(file_name);
                if !is_safe_path_component(&hash, false, Some(64)) {
                    continue;
                }

                let Ok(key) = InstantIndex::key(group_name, &hash) else {
                    continue;
                };

                batch.push((key, SavedPath::encode(group_name, sub_dir_name, file_name)));
                total += 1;

                if batch.len() >= BUILD_BATCH {
                    index.set_multi(&batch)?;
                    batch.clear();
                }
            }
        }
    }

    if !batch.is_empty() {
        index.set_multi(&batch)?;
    }

    say(write, format!("{total} items have been set in Redis."));
    say(write, "Done.");

    Ok(())
}

/// 按 mtime 清理过期临时文件（`aetherupload clean <days>`）。
pub fn clean_up_directory(runtime: &Runtime, write: &mut dyn FnMut(&str), days: i64) -> i32 {
    match run_clean_up(runtime, write, days) {
        Ok(()) => 0,
        Err(err) => {
            say(write, format!("Error: {err}"));
            1
        }
    }
}

fn run_clean_up(runtime: &Runtime, write: &mut dyn FnMut(&str), days: i64) -> Result<()> {
    if days <= 0 {
        return Err(Error::Backend(
            "invalid param 'days', should be greater than 0 .".to_string(),
        ));
    }

    say(
        write,
        format!("Start deleting partial files created {days} days ago..."),
    );

    let due = std::time::SystemTime::now() - std::time::Duration::from_secs(days as u64 * 86_400);

    // 断点文件：`_header` 下**没有扩展名**的文件（与 PHP 的 pathinfo 判定一致）
    let header_dir = runtime.upload_root().join(crate::header::HEADER_DIR);

    let invalid_headers: Vec<PathBuf> = read_dir_entries(&header_dir, EntryKind::Files)
        .into_iter()
        .filter(|path| extension_of(&path.to_string_lossy()).is_empty())
        .filter(|path| is_older_than(path, due))
        .collect();

    delete_files(&invalid_headers)?;
    say(
        write,
        format!(
            "{} invalid headers have been deleted.",
            invalid_headers.len()
        ),
    );

    // 分块文件：各分组的子目录里的 `*.part`
    let mut invalid_parts: Vec<PathBuf> = Vec::new();

    for group in runtime.config().groups.values() {
        let group_path = runtime.upload_root().join(&group.group_dir);

        for sub_dir in read_dir_entries(&group_path, EntryKind::Dirs) {
            for file in read_dir_entries(&sub_dir, EntryKind::Files) {
                if extension_of(&file.to_string_lossy()) == "part" && is_older_than(&file, due) {
                    invalid_parts.push(file);
                }
            }
        }
    }

    delete_files(&invalid_parts)?;
    say(
        write,
        format!("{} invalid files have been deleted.", invalid_parts.len()),
    );
    say(write, "Done.");

    Ok(())
}

enum EntryKind {
    Dirs,
    Files,
}

/// 列目录（缺失目录按空处理 —— PHP 的 `opendir` 失败时同样直接返回）。
fn read_dir_entries(path: &Path, kind: EntryKind) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(path) else {
        return Vec::new();
    };

    dir.filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| match kind {
            EntryKind::Dirs => path.is_dir(),
            EntryKind::Files => path.is_file(),
        })
        .collect()
}

fn is_older_than(path: &Path, due: std::time::SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified < due)
        .unwrap_or(false)
}

fn delete_files(files: &[PathBuf]) -> Result<()> {
    for file in files {
        if file.exists() && std::fs::remove_file(file).is_err() {
            return Err(Error::Backend(format!("fail to delete {}", file.display())));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::instant::MemoryInstantStore;
    use std::sync::Arc;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("aetherupload-console-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn runtime_with_instant(root: &Path) -> Runtime {
        Runtime::new(Config::default(), root)
            .unwrap()
            .with_instant(Arc::new(MemoryInstantStore::new()))
    }

    #[test]
    fn groups_creates_root_header_and_group_dirs() {
        let root = temp_root("groups");
        let runtime = runtime_with_instant(&root);
        let mut output = Vec::new();

        let code = list_groups(&runtime, &mut |line| output.push(line.to_string()));

        assert_eq!(code, 0, "输出: {output:?}");
        assert!(runtime.upload_root().join("_header").is_dir());
        assert!(runtime.upload_root().join("file").is_dir());
        assert!(
            output
                .iter()
                .any(|line| line.contains("Group-Directory List:"))
        );
        assert!(output.iter().any(|line| line.starts_with("file - ")));

        // 幂等：再跑一次不报错
        let mut second = Vec::new();
        assert_eq!(
            list_groups(&runtime, &mut |line| second.push(line.to_string())),
            0
        );
        assert!(!second.iter().any(|line| line.contains("has been created")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn groups_flags_underscore_names() {
        let root = temp_root("underscore");
        let mut config = Config::default();
        config.groups.insert(
            "my_file".to_string(),
            config.groups.get("file").unwrap().clone(),
        );
        let runtime = Runtime::new(config, &root).unwrap();
        let mut output = Vec::new();

        assert_eq!(
            list_groups(&runtime, &mut |line| output.push(line.to_string())),
            0
        );
        assert!(
            output
                .iter()
                .any(|line| line.contains("Invalid group name")),
            "下划线分组名必须被点出来：{output:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_indexes_finished_files_only() {
        let root = temp_root("build");
        let runtime = runtime_with_instant(&root);
        runtime.config().resolve_group("file").unwrap();

        // 造两个成品 + 一个分块残留
        let dir = runtime.upload_root().join("file/202610");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("d41d8cd98f00b204e9800998ecf8427e.jpg"), b"a").unwrap();
        std::fs::write(dir.join("0cc175b9c0f1b6a831c399e269772661.pdf"), b"b").unwrap();
        std::fs::write(dir.join("deadbeef.part"), b"c").unwrap();

        let mut output = Vec::new();
        assert_eq!(
            build_redis_hashes(&runtime, &mut |line| output.push(line.to_string())),
            0
        );
        assert!(
            output
                .iter()
                .any(|line| line.contains("2 items have been set"))
        );

        // 索引命中：按 hash 查得到 savedPath
        let index = runtime.instant_index();
        assert_eq!(
            index
                .get("file_d41d8cd98f00b204e9800998ecf8427e")
                .unwrap()
                .as_deref(),
            Some("file_202610_d41d8cd98f00b204e9800998ecf8427e.jpg")
        );
        // 分块文件不进索引
        assert_eq!(index.get("file_deadbeef").unwrap(), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_removes_only_old_temporary_files() {
        let root = temp_root("clean");
        let runtime = runtime_with_instant(&root);

        let header_dir = runtime.upload_root().join("_header");
        let part_dir = runtime.upload_root().join("file/202610");
        std::fs::create_dir_all(&header_dir).unwrap();
        std::fs::create_dir_all(&part_dir).unwrap();

        let old_header = header_dir.join("0123456789abcdef");
        let old_part = part_dir.join("0123456789abcdef.jpg.part");
        let finished = part_dir.join("d41d8cd98f00b204e9800998ecf8427e.jpg");
        for path in [&old_header, &old_part, &finished] {
            std::fs::write(path, b"x").unwrap();
        }

        let mut output = Vec::new();
        // 0 天：全部文件都「超过 0 天」但参数不合法 → 报错
        assert_eq!(
            clean_up_directory(&runtime, &mut |line| output.push(line.to_string()), 0),
            1
        );
        assert!(output[0].contains("invalid param 'days'"));

        // 合法参数下（文件刚建，未过期）什么都不删
        let mut output = Vec::new();
        assert_eq!(
            clean_up_directory(&runtime, &mut |line| output.push(line.to_string()), 2),
            0
        );
        assert!(old_header.exists() && old_part.exists() && finished.exists());

        // 把 mtime 拨回 3 天前 → 断点与分块被删，成品留下
        let three_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        set_mtime(&old_header, three_days_ago);
        set_mtime(&old_part, three_days_ago);

        let mut output = Vec::new();
        assert_eq!(
            clean_up_directory(&runtime, &mut |line| output.push(line.to_string()), 2),
            0
        );
        assert!(!old_header.exists(), "过期断点应被删除");
        assert!(!old_part.exists(), "过期分块应被删除");
        assert!(finished.exists(), "成品不能被清理");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用 `touch` 造旧文件（避免引第三方 crate 去改 mtime）。
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let stamp = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("@{secs}"))
            .arg(path)
            .status();

        assert!(stamp.map(|s| s.success()).unwrap_or(false), "touch 失败");
    }
}
