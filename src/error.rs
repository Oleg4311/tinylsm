//! Ошибки хранилища.

use std::fmt;
use std::io;

/// Ошибка работы с базой.
#[derive(Debug)]
pub enum Error {
    /// Ошибка ввода-вывода (диск, права, нехватка места).
    Io(io::Error),
    /// Данные на диске повреждены: не сошлась контрольная сумма, неверный формат, обрыв в середине файла.
    Corruption(String),
    /// Неверное обращение: пустой ключ, слишком большое значение и т. п.
    InvalidArgument(String),
    /// База уже открыта другим процессом или другим экземпляром.
    Locked(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "ошибка ввода-вывода: {e}"),
            Error::Corruption(m) => write!(f, "повреждение данных: {m}"),
            Error::InvalidArgument(m) => write!(f, "неверный аргумент: {m}"),
            Error::Locked(m) => write!(f, "база занята: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub(crate) fn corruption<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Corruption(msg.into()))
}
