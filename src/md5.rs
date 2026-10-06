// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! MD5（RFC 1321）—— 对应 PHP 内核里的 `md5_file()`。
//!
//! 为什么手写而不是引 `md-5` crate：本 crate 默认 feature 下零第三方依赖（对齐
//! PHP 版「零新增依赖」的取向）。完整性校验只需要一个哈希，且 RFC 1321 自带
//! 全套测试向量 —— 正确性有锚，胆子可以大一点。
//!
//! **这不是密码学安全的哈希**，本库只用它做「上传前后内容一致」的校验与秒传
//! 索引键（与 PHP 版逐字节一致：客户端 `spark-md5`、服务端 `md5_file()`）。

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// floor(abs(sin(i + 1)) × 2³²)，RFC 1321 里以 16 进制写出。
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, //
    0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, //
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, //
    0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, //
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, //
    0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8, //
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, //
    0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, //
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, //
    0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, //
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, //
    0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, //
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, //
    0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1, //
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, //
    0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// 流式 MD5 上下文 —— 大文件按块喂，不需要整个读进内存。
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

impl Md5 {
    pub fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            buffer: [0; 64],
            buffered: 0,
            length: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.length = self.length.wrapping_add(data.len() as u64);

        // 先把上次剩下的半个块拼满
        if self.buffered > 0 {
            let want = 64 - self.buffered;
            let take = want.min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];

            if self.buffered < 64 {
                // 还没攒满一块，等下次
                return;
            }

            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }

        // 整块直接压缩，不经过中间缓冲；`as_chunks` 直接给出 `&[[u8; 64]]`，省一次拷贝
        let (blocks, rest) = data.as_chunks::<64>();
        for block in blocks {
            self.compress(block);
        }

        if !rest.is_empty() {
            self.buffer[..rest.len()].copy_from_slice(rest);
            self.buffered = rest.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        // 填充：0x80 + 0x00… 直到 56 mod 64，再补 8 字节小端位长
        let bit_len = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0x00]);
        }
        // 长度字段直接写进缓冲：此时 buffered == 56，56..64 正是空着的位置
        self.buffer[56..64].copy_from_slice(&bit_len.to_le_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut out = [0u8; 16];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }

        let [mut a, mut b, mut c, mut d] = self.state;

        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };

            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = tmp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

/// 一次性哈希。返回小写 16 进制（与 PHP `md5()` 输出一致）。
pub fn md5_hex(data: &[u8]) -> String {
    let mut ctx = Md5::new();
    ctx.update(data);
    to_hex(&ctx.finalize())
}

/// 流式计算文件哈希。返回小写 16 进制（与 PHP `md5_file()` 输出一致）——
/// 秒传索引键与落盘文件名都来自它，改动会直接让存量资源失联。
pub fn md5_file(path: impl AsRef<Path>) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut ctx = Md5::new();
    let mut buf = [0u8; 64 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }

    Ok(to_hex(&ctx.finalize()))
}

fn to_hex(digest: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 1321 附录 A.5 全套测试向量。
    #[test]
    fn rfc1321_vectors() {
        let cases = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(md5_hex(input.as_bytes()), expected, "input = {input:?}");
        }
    }

    /// 跨块边界的流式输入：分段喂与一次喂必须同值（百万个 'a'）。
    #[test]
    fn streaming_matches_oneshot() {
        let data = vec![b'a'; 1_000_000];
        assert_eq!(md5_hex(&data), "7707d6ae4e027c70eea2a935c2296f21");

        let mut ctx = Md5::new();
        for chunk in data.chunks(7) {
            // 质数步长：故意错开 64 字节块边界
            ctx.update(chunk);
        }
        assert_eq!(to_hex(&ctx.finalize()), "7707d6ae4e027c70eea2a935c2296f21");
    }

    /// 恰好在填充边界上的长度（55/56/64 字节）最容易写错。
    #[test]
    fn padding_boundaries() {
        for n in [55usize, 56, 57, 63, 64, 65, 119, 120, 128] {
            let data = vec![b'x'; n];
            let mut ctx = Md5::new();
            ctx.update(&data);
            let expected = md5_hex(&data);
            assert_eq!(to_hex(&ctx.finalize()), expected, "n = {n}");
        }
    }
}
