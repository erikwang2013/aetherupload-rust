// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 纯函数工具 —— 不依赖运行时、不碰全局状态的那部分。
//!
//! 需要配置与存储的几个动作（删除成品、读取成品、清理秒传记录）不在这里：
//! 它们是 [`crate::runtime::Runtime`] 上的方法，显式传入依赖而不是走全局单例
//! （见 `runtime.rs` 的说明）。

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::SubdirRule;

/// 生成临时资源基础名：8 字节随机数 → 16 个小写十六进制字符（1.x 既有断点文件
/// 沿用的命名形态）。
///
/// 熵源是操作系统 CSPRNG（`/dev/urandom`）；读不到时退化为「时间 ^ pid」种子的
/// splitmix64 —— 名字只在 `_header`/`.part` 内部使用，且始终带内容 md5 兜底，
/// 退化路径只影响不可猜性，不影响正确性。
pub fn generate_temp_name() -> String {
    let mut bytes = [0u8; 8];

    if !fill_random(&mut bytes) {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64).rotate_left(32);

        bytes.copy_from_slice(&splitmix64(seed).to_le_bytes());
    }

    to_hex(&bytes)
}

fn fill_random(buf: &mut [u8]) -> bool {
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buf))
        .is_ok()
}

/// splitmix64 —— 只在拿不到 `/dev/urandom` 时用的退化熵源。
fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 拼接 `basename.ext`；调用方保证扩展名非空（校验发生在拼接之前）。
pub fn get_file_name(base_name: &str, ext: &str) -> String {
    format!("{base_name}.{ext}")
}

/// 路径分量白名单 —— 客户端可控值进入路径之前必须过这一关：
///
/// - `allow_dot = false`：`^[a-zA-Z0-9_\-]+$`
/// - `allow_dot = true` ：`^[a-zA-Z0-9_\-][a-zA-Z0-9_\-\.]*$`（首字符不允许 `.`，
///   因此 `..` 被拒；中间的点合法 —— 文件名 `<md5>.jpg` 靠这条通过）
pub fn is_safe_path_component(value: &str, allow_dot: bool, max_length: Option<usize>) -> bool {
    if value.is_empty() {
        return false;
    }

    if let Some(max) = max_length
        && value.len() > max
    {
        return false;
    }

    let mut chars = value.chars();

    let Some(first) = chars.next() else {
        return false;
    };

    if !(first.is_ascii_alphanumeric() || first == '_' || first == '-') {
        return false;
    }

    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || (allow_dot && c == '.'))
}

/// 分组子目录名 —— 由 `resource_subdir_rule` 决定（`year` / `date` / `const`，
/// 缺省或未知取值按月）。
///
/// 与 1.x 既有部署的一处行为差异：这里取 **UTC**（零依赖下拿不到时区数据库），
/// 没有「服务器本地时区」的概念；影响只在月末月初的时区边界上。
pub fn generate_sub_dir_name(rule: SubdirRule) -> String {
    let (year, month, day) = utc_ymd();

    match rule {
        SubdirRule::Year => format!("{year:04}"),
        SubdirRule::Date => format!("{year:04}{month:02}{day:02}"),
        SubdirRule::Const => "subdir".to_string(),
        // Month 与未知取值都按月（未知取值不报错，落到默认规则）
        SubdirRule::Month => format!("{year:04}{month:02}"),
    }
}

fn utc_ymd() -> (i64, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    civil_from_days(secs.div_euclid(86_400))
}

/// 天序号 → (年, 月, 日)，Howard Hinnant 的 civil_from_days 算法（纯算术，无时区表）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]

    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 展示链接：`route_display + '/' + saved_path`（拼接形态与 1.x 既有部署一致）。
pub fn display_link(route_display: &str, saved_path: &str) -> String {
    format!("{route_display}/{saved_path}")
}

/// 下载链接：`route_download + '/' + saved_path + '/' + new_name`。
///
/// `new_name` 已经过消毒（见 [`sanitize_download_name`]），这里只做拼接。
pub fn download_link(route_download: &str, saved_path: &str, new_name: &str) -> String {
    format!("{route_download}/{saved_path}/{new_name}")
}

/// 下载名消毒：控制字符、DEL、双引号、反斜杠、斜杠一律替换为 `_`，防头注入与
/// `Content-Disposition` 结构破坏。
pub fn sanitize_download_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\u{00}'..='\u{1f}' | '\u{7f}' | '"' | '\\' | '/' => '_',
            other => other,
        })
        .collect()
}

/// 取小写扩展名（`abc.JPG` → `jpg`）。没有扩展名时返回空串。
pub fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 取不含最后一段扩展名的文件名（`a.b.c` → `a.b`）。
pub fn file_stem_of(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_name_is_16_hex_chars_and_varies() {
        let a = generate_temp_name();
        let b = generate_temp_name();

        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次生成不应相同");
    }

    /// 白名单的边界语义（含首字符限制这个最容易写错的边界）。
    #[test]
    fn safe_path_component_boundary_semantics() {
        assert!(is_safe_path_component("file", false, None));
        assert!(is_safe_path_component("202610", false, None));
        assert!(is_safe_path_component("a-b_c", false, None));
        assert!(!is_safe_path_component("", false, None));
        assert!(!is_safe_path_component("a.b", false, None), "不允许点");
        assert!(!is_safe_path_component("a/b", false, None));
        assert!(!is_safe_path_component("中文", false, None));

        // allow_dot：首字符仍不允许点，中间可以
        assert!(is_safe_path_component("abc123.jpg", true, None));
        assert!(
            is_safe_path_component("a..b", true, None),
            "宽松白名单会放过中间的双点"
        );
        assert!(!is_safe_path_component(".hidden", true, None));
        assert!(!is_safe_path_component("..", true, None));
        assert!(!is_safe_path_component("../etc", true, None));

        // 长度上限按字节计（这里的取值全是 ASCII，字节数即字符数）
        assert!(is_safe_path_component("abc", false, Some(3)));
        assert!(!is_safe_path_component("abcd", false, Some(3)));
    }

    #[test]
    fn file_name_joining() {
        assert_eq!(get_file_name("abc", "jpg"), "abc.jpg");
        assert_eq!(extension_of("abc.JPG"), "jpg");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(file_stem_of("abc.jpg"), "abc");
        assert_eq!(file_stem_of("a.b.c"), "a.b");
    }

    #[test]
    fn subdir_rules() {
        // 具体值随时间变化，这里只验证形状
        let month = generate_sub_dir_name(SubdirRule::Month);
        assert_eq!(month.len(), 6);
        assert!(month.chars().all(|c| c.is_ascii_digit()));

        assert_eq!(generate_sub_dir_name(SubdirRule::Year).len(), 4);
        assert_eq!(generate_sub_dir_name(SubdirRule::Date).len(), 8);
        assert_eq!(generate_sub_dir_name(SubdirRule::Const), "subdir");
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 闰日
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn download_name_sanitizing() {
        assert_eq!(sanitize_download_name("a\"b\\c/d"), "a_b_c_d");
        assert_eq!(sanitize_download_name("正常名称.pdf"), "正常名称.pdf");
        assert_eq!(sanitize_download_name("bad\nname"), "bad_name");
    }

    #[test]
    fn links() {
        assert_eq!(
            display_link("/aetherupload/display", "file_202610_x.jpg"),
            "/aetherupload/display/file_202610_x.jpg"
        );
        assert_eq!(
            download_link("/aetherupload/download", "file_202610_x.jpg", "报告.pdf"),
            "/aetherupload/download/file_202610_x.jpg/报告.pdf"
        );
    }
}
