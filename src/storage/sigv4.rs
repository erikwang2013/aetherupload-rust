// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! AWS Signature Version 4（S3 规则）—— 对应 PHP 版 `Storage/SignatureV4.php`。
//!
//! 三条 S3 专属规则（照抄 PHP）：路径**逐段** URI 编码、不做点段归一化、不双重编码。
//!
//! 全是纯函数：没有时钟也没有 IO —— `amz_date`（`YYYYMMDDTHHMMSSZ`）由调用方传入，
//! 单测因此能把时刻钉死，直接比对官方测试套件的固定输出。
//!
//! **正确性锚**（全部是「确切输入 → 确切输出」，见本文件 `tests` 模块）：
//! - AWS 官方 sig-v4 测试套件（`aws-sig-v4-test-suite`）的 `get-vanilla`、
//!   `get-header-value-trim`、`post-x-www-form-urlencoded` 三例的 `.creq` / `.sts` /
//!   `.authz` 逐字节内容；
//! - AWS 文档「用查询参数签名」示例的预签名查询串
//!   （`…/test.txt?…&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404`）；
//! - RFC 4231 的 HMAC-SHA256 用例与 FIPS 180-2 的 SHA-256 用例 —— 签名链的基石。

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 算法标识（`Authorization` 头与 `X-Amz-Algorithm` 都用它）。
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// 不签 payload 时的占位值（华为云 OBS 等只接受该形式）。
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// HMAC-SHA256（RFC 2104），返回原始 32 字节。
///
/// 公开是为了让单测能直接锚定 RFC 4231 的公开向量 —— 签名链的其余部分都由它拼出来。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// SHA-256 的小写十六进制。
pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// 流式 SHA-256（读到 `reader` 末尾为止），返回小写十六进制 —— 大文件不整读进内存。
pub fn sha256_hex_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(to_hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

/// 签名密钥：`AWS4` + 四级 HMAC 链（date → region → service → `aws4_request`）。
pub fn signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let key = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let key = hmac_sha256(&key, region.as_bytes());
    let key = hmac_sha256(&key, service.as_bytes());

    hmac_sha256(&key, b"aws4_request")
}

/// 凭据作用域：`<date>/<region>/<service>/aws4_request`。
pub fn credential_scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/aws4_request")
}

/// AWS URI 编码：`A-Za-z0-9-._~` 原样，其余一律 `%XX`（大写十六进制）。
///
/// `encode_slash = false` 用于路径（分隔符保留），`true` 用于查询串的名与值。
pub fn uri_encode(input: &[u8], encode_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut out = String::with_capacity(input.len());
    for &byte in input {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (byte == b'/' && !encode_slash) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }

    out
}

/// 规范查询串：解析 → 名与值各自「先解码再编码一次」→ 按（名, 值）逐字节排序 → `&` 连接。
///
/// 解码再编码一次保证规范语义：无论调用方传进来的是裸值还是已编码值，结果一致。
pub fn canonical_query(query: &str) -> String {
    let pairs = encoded_query_pairs(query);

    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 规范请求 —— 签名与预签名共用的第一层输入。
///
/// `headers` 的名大小写不敏感；值 trim + 连续空白折叠为单空格；参与签名前按小写名排序。
/// 输出含 `host;x-amz-date` 那行前的**空行**（规范的一部分，少一行签名就不对）。
pub fn canonical_request(
    method: &str,
    uri: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let normalized = normalized_headers(headers);

    let mut canonical_headers = String::new();
    for (name, value) in &normalized {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value);
        canonical_headers.push('\n');
    }

    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        uri_encode(uri.as_bytes(), false),
        canonical_query(query),
        canonical_headers,
        signed_headers(headers),
        payload_hash
    )
}

/// 待签串（`StringToSign`）。
pub fn string_to_sign(
    amz_date: &str,
    date: &str,
    region: &str,
    service: &str,
    canonical_request: &str,
) -> String {
    format!(
        "{ALGORITHM}\n{amz_date}\n{}\n{}",
        credential_scope(date, region, service),
        sha256_hex(canonical_request.as_bytes())
    )
}

/// 签名器：把凭据、区域与服务绑在一起（字段全是借用，随用随建）。
///
/// 刻意不派生 `Debug` 的默认实现 —— 手写的那份把 `secret_key` 打成 `***`，
/// 免得哪次调试打印把凭据带进日志。
pub struct Signer<'a> {
    access_key: &'a str,
    secret_key: &'a str,
    region: &'a str,
    service: &'a str,
}

impl<'a> Signer<'a> {
    /// 绑定一套凭据与目标（S3 的 service 恒为 `s3`）。
    pub fn new(
        access_key: &'a str,
        secret_key: &'a str,
        region: &'a str,
        service: &'a str,
    ) -> Self {
        Self {
            access_key,
            secret_key,
            region,
            service,
        }
    }

    /// `Authorization` 头的值（用于 PUT / HEAD / DELETE / GET 等真实请求）。
    ///
    /// `payload_hash` 必须等于**实际发送字节**的 SHA-256，或 [`UNSIGNED_PAYLOAD`]。
    pub fn sign_request(
        &self,
        method: &str,
        uri: &str,
        query: &str,
        headers: &[(String, String)],
        payload_hash: &str,
        amz_date: &str,
    ) -> String {
        let date = date_of(amz_date);
        let canonical = canonical_request(method, uri, query, headers, payload_hash);
        let signature = self.signature(&canonical, amz_date, date);

        format!(
            "{ALGORITHM} Credential={}/{}, SignedHeaders={}, Signature={signature}",
            self.access_key,
            credential_scope(date, self.region, self.service),
            signed_headers(headers)
        )
    }

    /// 预签名查询串（含 `X-Amz-Signature`），供 URL 拼接 —— 对应 PHP `presignQuery()`。
    ///
    /// 预签名的 payload 位恒为 [`UNSIGNED_PAYLOAD`]（下载方不会重算内容哈希）。
    /// `query` 里已有的参数与 `X-Amz-*` 合并后统一编码一次、排序一次。
    pub fn presign(
        &self,
        method: &str,
        uri: &str,
        query: &str,
        headers: &[(String, String)],
        amz_date: &str,
        expires: u64,
    ) -> String {
        let date = date_of(amz_date);
        let scope = credential_scope(date, self.region, self.service);

        let mut pairs = encoded_query_pairs(query);
        for (name, value) in [
            ("X-Amz-Algorithm", ALGORITHM.to_string()),
            ("X-Amz-Credential", format!("{}/{scope}", self.access_key)),
            ("X-Amz-Date", amz_date.to_string()),
            ("X-Amz-Expires", expires.to_string()),
            ("X-Amz-SignedHeaders", signed_headers(headers)),
        ] {
            pairs.push((
                uri_encode(name.as_bytes(), true),
                uri_encode(value.as_bytes(), true),
            ));
        }
        pairs.sort();

        let canonical_query = pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");

        let canonical = canonical_request(method, uri, &canonical_query, headers, UNSIGNED_PAYLOAD);
        let signature = self.signature(&canonical, amz_date, date);

        format!("{canonical_query}&X-Amz-Signature={signature}")
    }

    fn signature(&self, canonical_request: &str, amz_date: &str, date: &str) -> String {
        let to_sign = string_to_sign(amz_date, date, self.region, self.service, canonical_request);
        let key = signing_key(self.secret_key, date, self.region, self.service);

        to_hex(&hmac_sha256(&key, to_sign.as_bytes()))
    }
}

impl fmt::Debug for Signer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("access_key", &self.access_key)
            .field("secret_key", &"***")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish()
    }
}

fn date_of(amz_date: &str) -> &str {
    amz_date.get(..8).unwrap_or(amz_date)
}

/// 小写名 => 归一值，按名排序。
fn normalized_headers(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            // PHP 是 `preg_replace('/\s+/', ' ', trim($value))`：折叠空白是规范的一部分
            (
                name.to_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect()
}

fn signed_headers(headers: &[(String, String)]) -> String {
    normalized_headers(headers)
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(";")
}

/// 已编码的 [名, 值] 对（先解码再编码一次，保证规范语义）。
fn encoded_query_pairs(query: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        pairs.push((
            uri_encode(&percent_decode(name), true),
            uri_encode(&percent_decode(value), true),
        ));
    }

    pairs.sort();
    pairs
}

/// `%XX` 解码；非法序列原样保留（与 PHP `rawurldecode` 同取向）。
/// 注意：`+` **不**解码成空格 —— 那是 `urldecode`/表单语义。
fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }

    out
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS 官方测试套件的固定材料（`aws-sig-v4-test-suite` 各用例共用的 credentials/date）。
    const ACCESS: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const AMZ_DATE: &str = "20150830T123600Z";
    const DATE: &str = "20150830";
    const REGION: &str = "us-east-1";
    const SERVICE: &str = "service";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn signer() -> Signer<'static> {
        Signer::new(ACCESS, SECRET, REGION, SERVICE)
    }

    /// 签名链的基石：SHA-256 与 HMAC 先钉死在公开向量上，上层用例才有意义。
    #[test]
    fn hash_primitives_match_published_vectors() {
        // FIPS 180-2
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // RFC 4231 用例 1/2/3
        assert_eq!(
            to_hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            to_hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );

        // 流式与一次性必须同值（大文件走流式）
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            sha256_hex_reader(&mut payload.as_slice()).unwrap(),
            sha256_hex(&payload)
        );
    }

    /// 官方 `get-vanilla`：`.creq` / `.sts` / `.authz` 逐字节比对。
    #[test]
    fn get_vanilla_matches_the_official_vector() {
        let headers = [
            ("Host".to_string(), "example.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), AMZ_DATE.to_string()),
        ];

        let canonical = canonical_request("GET", "/", "", &headers, EMPTY_SHA256);
        assert_eq!(
            canonical,
            format!(
                "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:{AMZ_DATE}\n\nhost;x-amz-date\n{EMPTY_SHA256}"
            )
        );

        let to_sign = string_to_sign(AMZ_DATE, DATE, REGION, SERVICE, &canonical);
        assert_eq!(
            to_sign,
            format!(
                "AWS4-HMAC-SHA256\n{AMZ_DATE}\n20150830/us-east-1/service/aws4_request\n\
                 bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
            )
        );

        assert_eq!(
            signer().sign_request("GET", "/", "", &headers, EMPTY_SHA256, AMZ_DATE),
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// 官方 `get-header-value-trim`：头值 trim + 连续空白折叠为单空格。
    #[test]
    fn get_header_value_trim_matches_the_official_vector() {
        let headers = [
            ("Host".to_string(), "example.amazonaws.com".to_string()),
            ("My-Header1".to_string(), "value1".to_string()),
            ("My-Header2".to_string(), "\"a   b   c\"".to_string()),
            ("X-Amz-Date".to_string(), AMZ_DATE.to_string()),
        ];

        assert_eq!(
            canonical_request("GET", "/", "", &headers, EMPTY_SHA256),
            format!(
                "GET\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nmy-header2:\"a b c\"\n\
                 x-amz-date:{AMZ_DATE}\n\nhost;my-header1;my-header2;x-amz-date\n{EMPTY_SHA256}"
            )
        );

        assert_eq!(
            signer().sign_request("GET", "/", "", &headers, EMPTY_SHA256, AMZ_DATE),
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;my-header1;my-header2;x-amz-date, \
             Signature=acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736"
        );
    }

    /// 官方 `post-x-www-form-urlencoded`：payload 哈希与签名都取自 `.creq` / `.authz`。
    #[test]
    fn post_x_www_form_urlencoded_matches_the_official_vector() {
        let payload_hash = sha256_hex(b"Param1=value1");
        assert_eq!(
            payload_hash,
            "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e"
        );

        let headers = [
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Host".to_string(), "example.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), AMZ_DATE.to_string()),
            ("Content-Length".to_string(), "13".to_string()),
        ];

        assert_eq!(
            signer().sign_request("POST", "/", "", &headers, &payload_hash, AMZ_DATE),
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=content-length;content-type;host;x-amz-date, \
             Signature=fec50118d90ecf934441dd37fb9a49bd7f5adb6450802ca3a0977623bbb7c27f"
        );
    }

    /// AWS 文档「用查询参数签名」示例：预签名查询串整体逐字节比对（含参数排序与单次编码）。
    #[test]
    fn presign_matches_the_documented_example() {
        let signer = Signer::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "s3",
        );
        let headers = [(
            "host".to_string(),
            "examplebucket.s3.amazonaws.com".to_string(),
        )];

        assert_eq!(
            signer.presign("GET", "/test.txt", "", &headers, "20130524T000000Z", 86_400),
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&\
             X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&\
             X-Amz-Date=20130524T000000Z&\
             X-Amz-Expires=86400&\
             X-Amz-SignedHeaders=host&\
             X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn canonical_query_sorts_by_name_then_value_and_encodes_once() {
        // 乱序 + 已编码值 + 裸值混合：结果必须与「已编码、排序后」的一致
        assert_eq!(
            canonical_query("uploadId=u-1&uploads="),
            "uploadId=u-1&uploads="
        );
        assert_eq!(canonical_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(
            canonical_query("response-content-disposition=attachment%3B%20a%20b.gif"),
            "response-content-disposition=attachment%3B%20a%20b.gif"
        );
        // 同名按值排序；裸值编一次后与已编码值等价
        assert_eq!(canonical_query("k=b&k=a"), "k=a&k=b");
        assert_eq!(canonical_query("k=a b"), "k=a%20b");
        // 无 '=' 的段按空值处理；空段忽略
        assert_eq!(canonical_query("&&flag"), "flag=");
    }

    #[test]
    fn canonical_uri_encodes_each_segment_without_normalizing() {
        assert_eq!(
            uri_encode(b"/bucket/up/gd/sub/a b.gif", false),
            "/bucket/up/gd/sub/a%20b.gif"
        );
        assert_eq!(uri_encode(b"a/b", true), "a%2Fb");
        // S3 规则：不做点段归一化
        assert_eq!(uri_encode(b"/a/../b", false), "/a/../b");
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let rendered = format!("{:?}", signer());
        assert!(rendered.contains("***"));
        assert!(!rendered.contains(SECRET));
    }
}
