//! 错误类型。所有对外可见的失败都收敛到这里，避免把 `rusqlite` 的类型泄进 HTTP 层。

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 数据库不可用。读失败必须能跟「没有数据」区分开——
    /// 「这个人我不认识」和「记忆库坏了」是两件完全不同的事。
    #[error("存储不可用：{0}")]
    Storage(String),

    /// 请求体不合法。
    #[error("请求不合法：{0}")]
    BadRequest(String),

    /// 没带 token 或 token 不对。
    #[error("未授权")]
    Unauthorized,

    /// 目标不存在。
    #[error("找不到：{0}")]
    NotFound(String),

    /// 回滚不能执行（比如目标时间晚于现在、或快照不存在）。
    #[error("回滚失败：{0}")]
    Rollback(String),

    /// 配置有问题。启动期就该暴露，不能等到跑起来才炸。
    #[error("配置错误：{0}")]
    Config(String),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        // 唯一键冲突要能被上层识别成「重复写入」而不是「存储坏了」，
        // 否则幂等重放会变成日志里的一片 ERROR。
        match e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Error::BadRequest(format!("唯一约束冲突：{e}"))
            }
            other => Error::Storage(other.to_string()),
        }
    }
}

impl Error {
    /// HTTP 状态码映射。集中在一处，避免每个 handler 各写一套。
    pub fn status(&self) -> u16 {
        match self {
            Error::BadRequest(_) => 400,
            Error::Unauthorized => 401,
            Error::NotFound(_) => 404,
            Error::Rollback(_) => 409,
            Error::Storage(_) | Error::Io(_) => 503,
            Error::Config(_) => 500,
        }
    }

    /// 是否值得让调用方重试。存储类错误重试有意义，参数错误重试永远是错的。
    pub fn retryable(&self) -> bool {
        matches!(self, Error::Storage(_) | Error::Io(_))
    }
}

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Error", 3)?;
        st.serialize_field("error", &self.to_string())?;
        st.serialize_field("retryable", &self.retryable())?;
        st.serialize_field("kind", &kind(self))?;
        st.end()
    }
}

fn kind(e: &Error) -> &'static str {
    match e {
        Error::Storage(_) => "storage",
        Error::BadRequest(_) => "bad_request",
        Error::Unauthorized => "unauthorized",
        Error::NotFound(_) => "not_found",
        Error::Rollback(_) => "rollback",
        Error::Config(_) => "config",
        Error::Io(_) => "io",
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", kind_of(self))
    }
}

/// 仅用于测试断言，避免测试里对字符串做脆弱的匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Storage,
    BadRequest,
    Unauthorized,
    NotFound,
    Rollback,
    Config,
    Io,
}

impl ErrorKind {
    /// 从 `kind` 字符串反查。只用于把 axum 的提取器错误归一成我们的格式。
    pub fn parse(s: &str) -> ErrorKind {
        match s {
            "bad_request" => ErrorKind::BadRequest,
            "unauthorized" => ErrorKind::Unauthorized,
            "not_found" => ErrorKind::NotFound,
            "rollback" => ErrorKind::Rollback,
            "config" => ErrorKind::Config,
            "io" => ErrorKind::Io,
            _ => ErrorKind::Storage,
        }
    }

    /// 这类错误是否值得让调用方重试。
    pub fn retryable(&self) -> bool {
        matches!(self, ErrorKind::Storage | ErrorKind::Io)
    }
}

fn kind_of(k: &ErrorKind) -> &'static str {
    match k {
        ErrorKind::Storage => "storage",
        ErrorKind::BadRequest => "bad_request",
        ErrorKind::Unauthorized => "unauthorized",
        ErrorKind::NotFound => "not_found",
        ErrorKind::Rollback => "rollback",
        ErrorKind::Config => "config",
        ErrorKind::Io => "io",
    }
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Storage(_) => ErrorKind::Storage,
            Error::BadRequest(_) => ErrorKind::BadRequest,
            Error::Unauthorized => ErrorKind::Unauthorized,
            Error::NotFound(_) => ErrorKind::NotFound,
            Error::Rollback(_) => ErrorKind::Rollback,
            Error::Config(_) => ErrorKind::Config,
            Error::Io(_) => ErrorKind::Io,
        }
    }
}
