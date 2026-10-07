// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 极简 RESP 客户端 —— 只为 CLI 的运维命令服务。
//!
//! 库本身**不依赖任何 Redis 客户端**（`InstantStore` 是 trait，客户端由宿主注入）；
//! 这里给的是一个零依赖的参考实现，也顺带演示了「一个 `InstantStore` 实现要写多厚」：
//! 五个动作 + 前缀遍历，就这样。
//!
//! 支持得刚好够用：`AUTH`（可选）、`SELECT`（可选）、`GET` / `SETEX` / `DEL` / `KEYS` /
//! `HGET` / `HDEL`。单连接、同步收发（连接包在 `Mutex` 里以满足 `Send + Sync`），
//! 不做连接池与重连 —— 运维命令跑一次就退出。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

use aetherupload::instant::{InstantStore, KEY_PREFIX, LEGACY_HASH_KEY};
use aetherupload::{Error, Result};

pub struct RespClient {
    stream: Mutex<BufReader<TcpStream>>,
}

impl RespClient {
    /// 解析 `redis://[:password@]host:port[/db]` 并建立连接。
    pub fn connect(url: &str) -> Result<Self> {
        let rest = url.strip_prefix("redis://").ok_or_else(|| {
            Error::Backend(format!(
                "不认识的 Redis 地址：{url}（应形如 redis://127.0.0.1:6379/0）"
            ))
        })?;

        let (auth, rest) = match rest.split_once('@') {
            Some((credentials, rest)) => {
                let password = credentials
                    .split_once(':')
                    .map(|(_, pwd)| pwd)
                    .unwrap_or(credentials);
                (Some(password.to_string()), rest)
            }
            None => (None, rest),
        };

        let (host_port, db) = match rest.split_once('/') {
            Some((host_port, db)) => (host_port, db.parse::<u32>().ok()),
            None => (rest, None),
        };

        let host_port = if host_port.contains(':') {
            host_port.to_string()
        } else {
            format!("{host_port}:6379")
        };

        let stream = TcpStream::connect(&host_port)
            .map_err(|err| Error::Backend(format!("连接 Redis {host_port} 失败：{err}")))?;

        let client = Self {
            stream: Mutex::new(BufReader::new(stream)),
        };

        if let Some(password) = auth {
            client.command(&["AUTH", &password])?;
        }

        if let Some(db) = db {
            client.command(&["SELECT", &db.to_string()])?;
        }

        Ok(client)
    }

    fn command(&self, args: &[&str]) -> Result<Reply> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| Error::Backend("Redis 连接已被污染".to_string()))?;

        let mut request = format!("*{}\r\n", args.len());
        for arg in args {
            request.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
        }

        stream
            .get_mut()
            .write_all(request.as_bytes())
            .and_then(|()| stream.get_mut().flush())
            .map_err(|err| Error::Backend(format!("写 Redis 失败：{err}")))?;

        read_reply(&mut stream)
    }
}

fn read_reply(stream: &mut BufReader<TcpStream>) -> Result<Reply> {
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .map_err(|err| Error::Backend(format!("读 Redis 失败：{err}")))?;

    let trimmed = line.trim_end_matches(['\r', '\n']);
    let Some((kind, body)) = trimmed.split_at_checked(1) else {
        return Err(Error::Backend("Redis 返回了空响应".to_string()));
    };

    match kind {
        "+" => Ok(Reply::Status),
        "-" => Err(Error::Backend(format!("Redis 报错：{body}"))),
        ":" => {
            // 整数回复本客户端用不到（SETEX / DEL 的结果都忽略），但仍要确认它确实是数字，
            // 免得协议错位时静默把脏数据当成功
            body.parse::<i64>()
                .map_err(|_| Error::Backend(format!("无法解析的 RESP 整数：{body}")))?;
            Ok(Reply::Int)
        }
        "$" => {
            let len: i64 = body.parse().unwrap_or(-1);
            if len < 0 {
                return Ok(Reply::Nil);
            }

            let mut buf = vec![0u8; len as usize + 2];
            stream
                .read_exact(&mut buf)
                .map_err(|err| Error::Backend(format!("读 Redis 失败：{err}")))?;
            buf.truncate(len as usize);

            Ok(Reply::Bulk(String::from_utf8_lossy(&buf).into_owned()))
        }
        "*" => {
            let count: i64 = body.parse().unwrap_or(-1);
            let mut items = Vec::new();
            for _ in 0..count.max(0) {
                items.push(read_reply(stream)?);
            }
            Ok(Reply::Array(items))
        }
        other => Err(Error::Backend(format!("无法解析的 RESP 类型：{other}"))),
    }
}

#[derive(Debug)]
enum Reply {
    Status,
    Int,
    Bulk(String),
    Nil,
    Array(Vec<Reply>),
}

impl Reply {
    fn into_bulk(self) -> Option<String> {
        match self {
            Self::Bulk(value) => Some(value),
            _ => None,
        }
    }
}

impl InstantStore for RespClient {
    fn get(&self, key: &str) -> Result<Option<String>> {
        self.command(&["GET", key])
            .map(Reply::into_bulk)
            .map(|value| value.filter(|value| !value.is_empty()))
    }

    fn set_ex(&self, key: &str, value: &str, ttl_seconds: u64) -> Result<()> {
        self.command(&["SETEX", key, &ttl_seconds.to_string(), value])
            .map(|_| ())
    }

    fn del(&self, key: &str) -> Result<()> {
        self.command(&["DEL", key]).map(|_| ())
    }

    /// 本实现真的会去读旧版单 hash（`HGET aetherupload_resource`），声明能力位为真，
    /// 从 PHP 版迁移站点的存量数据继续可命中。
    fn supports_legacy_fallback(&self) -> bool {
        true
    }

    fn legacy_get(&self, field: &str) -> Result<Option<String>> {
        self.command(&["HGET", LEGACY_HASH_KEY, field])
            .map(Reply::into_bulk)
    }

    fn legacy_del(&self, field: &str) -> Result<()> {
        self.command(&["HDEL", LEGACY_HASH_KEY, field]).map(|_| ())
    }

    /// `KEYS prefix*` + 逐个 `DEL` —— 与 PHP 版 `deleteAll()` 同一取向：
    /// 只在每日重建命令里调用，记录数在百万级以内可接受。
    fn delete_prefix(&self, _prefix: &str) -> Result<()> {
        let keys = match self.command(&["KEYS", &format!("{KEY_PREFIX}*")])? {
            Reply::Array(items) => items
                .into_iter()
                .filter_map(Reply::into_bulk)
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };

        for key in keys {
            self.command(&["DEL", &key])?;
        }

        // 旧版单 hash 一并清掉
        self.command(&["DEL", LEGACY_HASH_KEY]).map(|_| ())
    }
}
