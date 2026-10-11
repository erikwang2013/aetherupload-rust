// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 两个入口控制器 —— 内核的「用例层」：其余模块都是它们与适配器共用的零件。

pub mod resource;
pub mod upload;

pub use resource::{ACCEL_PREFIX, ResourceController, ResourceResponse, attachment_disposition};
pub use upload::{
    ChunkBody, PreprocessOk, PreprocessRequest, SaveChunkOk, SaveChunkRequest, UploadController,
    save_chunk_ok_to_json, upload_ok_to_json,
};

/// 上传流程的端到端单测：从 `preprocess` 到末块落盘，外加 §4 表每一个 code 的触发路径。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::md5::md5_hex;
    use crate::runtime::Runtime;
    use std::path::Path;
    use std::sync::Arc;

    /// 建一个可上传的根：分组目录与断点目录都要先存在（真实部署里由 `aetherupload groups` 建）。
    fn controller_with(tag: &str, config: Config) -> (std::path::PathBuf, UploadController) {
        let root = std::env::temp_dir().join(format!("aetherupload-flow-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("storage/app/aetherupload/file")).unwrap();
        std::fs::create_dir_all(root.join("storage/app/aetherupload/_header")).unwrap();

        let runtime = Arc::new(Runtime::new(config, &root).unwrap());

        (root, UploadController::new(runtime))
    }

    fn controller(tag: &str) -> (std::path::PathBuf, UploadController) {
        controller_with(tag, Config::default())
    }

    fn pre_request(name: &str, size: &str) -> PreprocessRequest {
        PreprocessRequest {
            resource_name: Some(name.into()),
            resource_size: Some(size.into()),
            group: Some("file".into()),
            resource_hash: None,
            locale: Some("en".into()),
        }
    }

    fn chunk_request(pre: &PreprocessOk, index: u64, total: u64, bytes: &[u8], hash: Option<&str>) -> SaveChunkRequest {
        SaveChunkRequest {
            chunk_total: Some(total.to_string()),
            chunk_index: Some(index.to_string()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some(pre.resource_ext.clone()),
            group_subdir: Some(pre.group_sub_dir.clone()),
            group: Some("file".into()),
            resource_hash: hash.map(str::to_string),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(bytes.to_vec())),
        }
    }

    fn part_path(root: &Path, pre: &PreprocessOk) -> std::path::PathBuf {
        root.join("storage/app/aetherupload/file")
            .join(&pre.group_sub_dir)
            .join(format!("{}.{}.part", pre.resource_temp_base_name, pre.resource_ext))
    }

    #[test]
    fn preprocess_returns_the_upload_result() {
        let (root, controller) = controller("pre-basic");

        let ok = controller.preprocess(&pre_request("a.txt", "5")).unwrap();
        assert_eq!(ok.chunk_size, 1_000_000);
        assert_eq!(ok.resource_ext, "txt");
        assert_eq!(ok.group_sub_dir.len(), 6);
        assert_eq!(ok.resource_temp_base_name.len(), 16);
        assert_eq!(ok.saved_path, "");

        // 成功体的字段名在 json.rs 单测里逐字段断言
        assert!(upload_ok_to_json(&ok).contains(r#""chunk_size":1000000"#));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preprocess_rejects_bad_parameters() {
        let (root, controller) = controller("pre-params");

        let problem = controller.preprocess(&PreprocessRequest::default()).unwrap_err();
        assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"));

        // 声明大小：非数字 / 0 / 超分组上限（都归 400）
        for size in ["5.5", "1e3", "0", "-3", "104857601"] {
            let problem = controller.preprocess(&pre_request("a.txt", size)).unwrap_err();
            assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"), "size={size}");
        }

        // 安全检查只看派生扩展名：正常中文/空格文件名继续通过
        assert!(controller.preprocess(&pre_request("我的 报告.txt", "5")).is_ok());

        // 派生扩展名不安全 → 400；不在白名单 → 415
        let problem = controller.preprocess(&pre_request("a.t x", "5")).unwrap_err();
        assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"));
        let problem = controller.preprocess(&pre_request("a.exe", "5")).unwrap_err();
        assert_eq!((problem.status(), problem.code), (415, "unsupported_media_type"));

        // 分组：未配置 / 含下划线 → 400 invalid_group
        for group in ["nope", "my_file"] {
            let mut request = pre_request("a.txt", "5");
            request.group = Some(group.into());
            let problem = controller.preprocess(&request).unwrap_err();
            assert_eq!((problem.status(), problem.code), (400, "invalid_group"), "group={group}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_chunk_completes_and_reports_saved_path() {
        let (root, controller) = controller("flow");

        let ok = controller.preprocess(&pre_request("hello.txt", "5")).unwrap();
        let hash = md5_hex(b"hello");

        // 中间块：saved_path 为空
        let mid = controller.save_chunk(&chunk_request(&ok, 1, 2, b"hel", Some(&hash))).unwrap();
        assert_eq!(save_chunk_ok_to_json(&mid), r#"{"saved_path":""}"#);

        // 末块：落盘，saved_path = group_subdir_<md5>.ext
        let done = controller.save_chunk(&chunk_request(&ok, 2, 2, b"lo", Some(&hash))).unwrap();
        assert_eq!(done.saved_path, format!("file_{}_{hash}.txt", ok.group_sub_dir));

        // 成品内容完整，临时文件与断点都清了
        let landed = root
            .join("storage/app/aetherupload/file")
            .join(&ok.group_sub_dir)
            .join(format!("{hash}.txt"));
        assert_eq!(std::fs::read(&landed).unwrap(), b"hello");
        assert!(!part_path(&root, &ok).exists());

        // 清理之后再重发末块 → 409 未预处理
        let again = controller.save_chunk(&chunk_request(&ok, 2, 2, b"lo", Some(&hash))).unwrap_err();
        assert_eq!((again.status(), again.code), (409, "upload_not_prepared"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_of_order_is_409_with_expected_index() {
        let (root, controller) = controller("order");
        let ok = controller.preprocess(&pre_request("a.txt", "9")).unwrap();

        // 跳号：last = 0，先发 2 → 期望 1
        let problem = controller.save_chunk(&chunk_request(&ok, 2, 3, b"x", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (409, "chunk_out_of_order"));
        assert_eq!(problem.extra, vec![("expected_index", "1".to_string())]);
        assert!(problem.to_json().contains(r#""expected_index":1"#), "{}", problem.to_json());

        // 服务端尚未收到的那一块（1 == last + 1）被幂等接受
        assert!(controller.save_chunk(&chunk_request(&ok, 1, 3, b"a", None)).is_ok());

        // 重发已落块（1 <= last）也是 409 + expected_index
        let problem = controller.save_chunk(&chunk_request(&ok, 1, 3, b"a", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (409, "chunk_out_of_order"));
        assert_eq!(problem.extra, vec![("expected_index", "2".to_string())]);

        // 末块提前到达（last + 1 < total）同样 409
        let problem = controller.save_chunk(&chunk_request(&ok, 3, 3, b"c", None)).unwrap_err();
        assert_eq!(problem.extra, vec![("expected_index", "2".to_string())]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn not_prepared_never_cleans_up_resumable_state() {
        let (root, controller) = controller("prepared");

        // 从未预处理（没有 .part 也没有断点）
        let orphan = SaveChunkRequest {
            chunk_total: Some("3".into()),
            chunk_index: Some("1".into()),
            resource_temp_basename: Some("0011223344556677".into()),
            resource_ext: Some("txt".into()),
            group_subdir: Some("202610".into()),
            group: Some("file".into()),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(b"x".to_vec())),
            ..Default::default()
        };
        let problem = controller.save_chunk(&orphan).unwrap_err();
        assert_eq!((problem.status(), problem.code), (409, "upload_not_prepared"));

        // `.part` 在、断点被删 → 同样 409，且已落盘的分块**不许**被清理
        let ok = controller.preprocess(&pre_request("a.txt", "9")).unwrap();
        assert!(controller.save_chunk(&chunk_request(&ok, 1, 3, b"a", None)).is_ok());

        let header = root.join("storage/app/aetherupload/_header").join(&ok.resource_temp_base_name);
        std::fs::remove_file(&header).unwrap();

        let problem = controller.save_chunk(&chunk_request(&ok, 2, 3, b"b", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (409, "upload_not_prepared"));
        assert!(part_path(&root, &ok).exists(), "可恢复状态不许销毁数据");
        assert_eq!(std::fs::read(part_path(&root, &ok)).unwrap(), b"a");

        // 断点被写坏（不可解析）同样是可恢复的 409，同样不清理
        std::fs::write(&header, "not-a-number").unwrap();
        let problem = controller.save_chunk(&chunk_request(&ok, 2, 3, b"b", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (409, "upload_not_prepared"));
        assert!(part_path(&root, &ok).exists(), "断点坏掉也不许清理数据");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn size_limits_are_413() {
        // 单块 > chunk_size
        let (root, controller) = controller_with("big-chunk", {
            let mut config = Config::default();
            config.chunk_size = 4;
            config
        });
        let ok = controller.preprocess(&pre_request("a.txt", "8")).unwrap();
        assert_eq!(ok.chunk_size, 4);

        let problem = controller.save_chunk(&chunk_request(&ok, 1, 2, b"12345", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (413, "payload_too_large"));

        // 累计写入超分组上限
        let (root2, controller2) = controller_with("max-size", {
            let mut config = Config::default();
            config.groups.get_mut("file").unwrap().resource_maxsize = 6;
            config
        });
        let ok = controller2.preprocess(&pre_request("a.txt", "6")).unwrap();
        assert!(controller2.save_chunk(&chunk_request(&ok, 1, 2, b"1234", None)).is_ok());

        let problem = controller2.save_chunk(&chunk_request(&ok, 2, 2, b"567", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (413, "payload_too_large"));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn final_chunk_validates_type_mime_and_md5() {
        // 客户端在末块改扩展名 → 415（局部绑定改名 ctl，避免遮蔽同名构造函数）
        let (root, ctl) = controller("final-type");
        let ok = ctl.preprocess(&pre_request("a.txt", "3")).unwrap();
        let mut request = chunk_request(&ok, 1, 1, b"abc", None);
        request.resource_ext = Some("exe".into());
        let problem = ctl.save_chunk(&request).unwrap_err();
        assert_eq!((problem.status(), problem.code), (415, "unsupported_media_type"));

        // 真实内容与声明扩展名不符 → 415
        let (root2, controller2) = controller_with("final-mime", {
            let mut config = Config::default();
            config.groups.get_mut("file").unwrap().resource_extensions = vec!["jpg".into()];
            config
        });
        let ok = controller2.preprocess(&pre_request("a.jpg", "4")).unwrap();
        let problem = controller2.save_chunk(&chunk_request(&ok, 1, 1, b"not-a-jpeg", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (415, "unsupported_media_type"));

        // 整份 md5 与声明不符（非 lax）→ 422
        let (root3, controller3) = controller("final-md5");
        let ok = controller3.preprocess(&pre_request("a.txt", "3")).unwrap();
        let wrong = "0".repeat(32);
        let problem = controller3.save_chunk(&chunk_request(&ok, 1, 1, b"abc", Some(&wrong))).unwrap_err();
        assert_eq!((problem.status(), problem.code), (422, "hash_mismatch"));
        assert!(problem.to_json().contains(r#""status":422"#));

        // hash 格式非法（非 32 位 hex）→ 400
        let (root4, controller4) = controller("hash-format");
        let ok = controller4.preprocess(&pre_request("a.txt", "3")).unwrap();
        let problem = controller4.save_chunk(&chunk_request(&ok, 1, 1, b"abc", Some("zzz"))).unwrap_err();
        assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"));

        for dir in [root, root2, root3, root4] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn save_chunk_rejects_bad_parameters() {
        let (root, controller) = controller("save-params");
        let ok = controller.preprocess(&pre_request("a.txt", "9")).unwrap();

        // 序号 > 总数
        let problem = controller.save_chunk(&chunk_request(&ok, 3, 2, b"x", None)).unwrap_err();
        assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"));

        let mut request = chunk_request(&ok, 1, 3, b"x", None);

        // 序号非数字 / 总数超上限
        request.chunk_index = Some("1.5".into());
        assert_eq!(controller.save_chunk(&request).unwrap_err().code, "invalid_parameter");
        request.chunk_index = Some("1".into());
        request.chunk_total = Some("10001".into());
        assert_eq!(controller.save_chunk(&request).unwrap_err().code, "invalid_parameter");

        // group_subdir 含下划线会破坏 savedPath 三段式解码
        request.chunk_total = Some("3".into());
        request.group_subdir = Some("20_2610".into());
        assert_eq!(controller.save_chunk(&request).unwrap_err().code, "invalid_parameter");

        // 未知分组 → invalid_group（与 preprocess 同一判定）
        request.group_subdir = Some(ok.group_sub_dir.clone());
        request.group = Some("nope".into());
        assert_eq!(controller.save_chunk(&request).unwrap_err().code, "invalid_group");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_chunk_body_keeps_progress() {
        let (root, controller) = controller("no-chunk");
        let ok = controller.preprocess(&pre_request("a.txt", "3")).unwrap();

        let mut request = chunk_request(&ok, 1, 1, b"abc", None);
        request.chunk = None;

        let problem = controller.save_chunk(&request).unwrap_err();
        assert_eq!((problem.status(), problem.code), (400, "invalid_parameter"));
        assert!(part_path(&root, &ok).exists(), "分块缺失是可恢复的：进度必须保留");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 秒传：`preprocess` 命中即带出 `saved_path`，一个分块都不用传。
    /// 要两件套：分组开 `instant_completion` + 运行时注入可用 `InstantStore`
    ///（默认 `NullInstantStore` 视同「索引不可用」→ 按未命中继续）。
    #[test]
    fn instant_completion_returns_saved_path_without_chunks() {
        let root = std::env::temp_dir().join(format!("aetherupload-flow-instant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("storage/app/aetherupload/file")).unwrap();
        std::fs::create_dir_all(root.join("storage/app/aetherupload/_header")).unwrap();

        let mut config = Config::default();
        config.instant_completion = true;
        let runtime = Arc::new(
            Runtime::new(config, &root)
                .unwrap()
                .with_instant(Arc::new(crate::instant::MemoryInstantStore::new())),
        );
        let controller = UploadController::new(runtime);

        let ok = controller.preprocess(&pre_request("a.txt", "3")).unwrap();
        let hash = md5_hex(b"abc");
        let done = controller.save_chunk(&chunk_request(&ok, 1, 1, b"abc", Some(&hash))).unwrap();

        // 同内容再传：preprocess 直接返回既有路径
        let mut again = pre_request("b.txt", "3");
        again.resource_hash = Some(hash.clone());
        let ok2 = controller.preprocess(&again).unwrap();
        assert_eq!(ok2.saved_path, done.saved_path);

        let _ = std::fs::remove_dir_all(&root);
    }
}
