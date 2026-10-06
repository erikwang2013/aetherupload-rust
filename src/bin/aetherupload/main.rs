// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 运维 CLI —— 对应 PHP 版 `php webman aetherupload:groups | :build | :clean`。
//!
//! ```text
//! aetherupload groups [--base-path DIR] [--root-dir DIR] [--group NAME]...
//! aetherupload build  --redis URL [--base-path DIR] [--root-dir DIR] [--group NAME]...
//! aetherupload clean  <days> [--base-path DIR] [--root-dir DIR] [--group NAME]...
//! aetherupload pet
//! ```
//!
//! 覆盖默认布局的日常运维。**自定义配置的宿主请直接调用库 API**
//! （`aetherupload::console::{list_groups, build_redis_hashes, clean_up_directory}`），
//! 把自家的 `Config` 传进去即可，本 CLI 只是默认布局的便捷壳。

mod resp;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aetherupload::config::{Config, GroupConfig};
use aetherupload::console;
use aetherupload::instant::NullInstantStore;
use aetherupload::{Runtime, pet};

const USAGE: &str = "\
用法:
  aetherupload groups [--base-path DIR] [--root-dir DIR] [--group NAME]...
  aetherupload build  --redis redis://[密码@]主机:端口/库 [--base-path DIR] [--root-dir DIR] [--group NAME]...
  aetherupload clean  <days> [--base-path DIR] [--root-dir DIR] [--group NAME]...
  aetherupload pet

说明:
  groups  建上传根目录、_header 与各分组目录（不建就一定上传失败）
  build   按磁盘现状重建秒传索引（需要 --redis）
  clean   按 mtime 回收 N 天前的临时文件（_header 与 *.part）
  pet     打印项目宠物

选项:
  --base-path DIR  宿主项目根（默认：当前目录）
  --root-dir DIR   上传根目录（默认：storage/app/aetherupload）
  --group NAME     追加一个分组（目录同名，上限与扩展名沿用默认 file 分组）
  --redis URL      秒传索引的 Redis 地址，仅 build 需要
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match run(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("Error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some(command) = args.first() else {
        println!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    };

    let mut base_path = PathBuf::from(".");
    let mut root_dir: Option<String> = None;
    let mut extra_groups: Vec<String> = Vec::new();
    let mut redis_url: Option<String> = None;
    let mut days: Option<i64> = None;

    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();

        let value = |index: usize| -> Result<&String, String> {
            args.get(index + 1).ok_or_else(|| format!("{arg} 缺少取值"))
        };

        match arg {
            "--base-path" => {
                base_path = PathBuf::from(value(index)?);
                index += 2;
            }
            "--root-dir" => {
                root_dir = Some(value(index)?.clone());
                index += 2;
            }
            "--group" => {
                extra_groups.push(value(index)?.clone());
                index += 2;
            }
            "--redis" => {
                redis_url = Some(value(index)?.clone());
                index += 2;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            other => match other.parse::<i64>() {
                Ok(number) if days.is_none() => {
                    days = Some(number);
                    index += 1;
                }
                _ => return Err(format!("无法识别的参数：{other}\n\n{USAGE}")),
            },
        }
    }

    if command == "pet" {
        println!("{}\n", pet::ASCII);
        println!("{} —— {}", pet::NAME, pet::TAGLINE);
        return Ok(ExitCode::SUCCESS);
    }

    let runtime = Arc::new(build_runtime(
        &base_path,
        root_dir,
        &extra_groups,
        redis_url.as_deref(),
    )?);

    let mut write = |line: &str| println!("{line}");

    let code = match command.as_str() {
        "groups" => console::list_groups(&runtime, &mut write),
        "build" => {
            if redis_url.is_none() {
                return Err("build 需要 --redis（不接 Redis 时秒传索引无处可写）".to_string());
            }
            console::build_redis_hashes(&runtime, &mut write)
        }
        "clean" => {
            let Some(days) = days else {
                return Err("clean 需要一个天数参数，例如 `aetherupload clean 2`".to_string());
            };
            console::clean_up_directory(&runtime, &mut write, days)
        }
        other => return Err(format!("无法识别的命令：{other}\n\n{USAGE}")),
    };

    Ok(if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn build_runtime(
    base_path: &std::path::Path,
    root_dir: Option<String>,
    extra_groups: &[String],
    redis_url: Option<&str>,
) -> Result<Runtime, String> {
    let mut config = Config::default();

    if let Some(root_dir) = root_dir {
        config.root_dir = root_dir;
    }

    // 追加分组：目录同名，上限与扩展名沿用默认 file 分组
    let template: GroupConfig = config
        .groups
        .get("file")
        .cloned()
        .ok_or_else(|| "默认配置缺少 file 分组".to_string())?;

    for name in extra_groups {
        config.groups.insert(
            name.clone(),
            GroupConfig {
                group_dir: name.clone(),
                ..template.clone()
            },
        );
    }

    let mut runtime = Runtime::new(config, base_path).map_err(|err| err.to_string())?;

    match redis_url {
        Some(url) => {
            runtime = runtime.with_instant(Arc::new(
                resp::RespClient::connect(url).map_err(|err| err.to_string())?,
            ));
        }
        None => {
            // 不接 Redis 时把默认的 Null 换成同样「一调用就报错」但信息更清楚的提示
            runtime = runtime.with_instant(Arc::new(NullInstantStore));
        }
    }

    Ok(runtime)
}
