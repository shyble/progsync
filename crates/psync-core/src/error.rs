//! Errors: a message plus the chain of causes it came from.
//!
//! `{}` shows the outermost message, `{:#}` the whole chain joined with
//! ": " (e.g. "reading src/a.rs: Permission denied (os error 13)").
//! Any standard error converts with `?`; `.context(..)` and
//! `.with_context(..)` wrap a failure (or a `None`) in a higher-level message.

use std::fmt;

pub struct Error {
    msg: String,
    cause: Option<Box<Error>>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// An error with just a message.
    pub fn msg(msg: impl fmt::Display) -> Error {
        Error { msg: msg.to_string(), cause: None }
    }

    /// Wrap this error in a higher-level message.
    pub fn context(self, msg: impl fmt::Display) -> Error {
        Error { msg: msg.to_string(), cause: Some(Box::new(self)) }
    }

    /// The messages, outermost first.
    pub fn chain(&self) -> impl Iterator<Item = &str> {
        std::iter::successors(Some(self), |e| e.cause.as_deref()).map(|e| e.msg.as_str())
    }
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for Error {
    fn from(e: E) -> Error {
        let mut causes = Vec::new();
        let mut source = e.source();
        while let Some(s) = source {
            causes.push(s.to_string());
            source = s.source();
        }
        let mut cause = None;
        for msg in causes.into_iter().rev() {
            cause = Some(Box::new(Error { msg, cause }));
        }
        Error { msg: e.to_string(), cause }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !f.alternate() {
            return f.write_str(&self.msg);
        }
        for (i, m) in self.chain().enumerate() {
            if i > 0 {
                f.write_str(": ")?;
            }
            f.write_str(m)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)?;
        let mut causes = self.chain().skip(1).peekable();
        if causes.peek().is_some() {
            f.write_str("\n\nCaused by:")?;
            for (i, c) in causes.enumerate() {
                write!(f, "\n    {i}: {c}")?;
            }
        }
        Ok(())
    }
}

/// `.context(..)` / `.with_context(..)` on results and options.
pub trait Context<T> {
    fn context<C: fmt::Display>(self, msg: C) -> Result<T>;
    fn with_context<C: fmt::Display, F: FnOnce() -> C>(self, f: F) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context<C: fmt::Display>(self, msg: C) -> Result<T> {
        self.map_err(|e| e.into().context(msg))
    }

    fn with_context<C: fmt::Display, F: FnOnce() -> C>(self, f: F) -> Result<T> {
        self.map_err(|e| e.into().context(f()))
    }
}

impl<T> Context<T> for Option<T> {
    fn context<C: fmt::Display>(self, msg: C) -> Result<T> {
        self.ok_or_else(|| Error::msg(msg))
    }

    fn with_context<C: fmt::Display, F: FnOnce() -> C>(self, f: F) -> Result<T> {
        self.ok_or_else(|| Error::msg(f()))
    }
}

/// An [`Error`] from a format string: `err!("no task {id}")`.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::msg(format!($($arg)*))
    };
}

/// Return early with an [`Error`]: `bail!("no task {id}")`.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::err!($($arg)*).into())
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_fail() -> Result<()> {
        std::fs::read("/definitely/not/here").context("reading config")?;
        Ok(())
    }

    #[test]
    fn chain_and_formatting() {
        let e = io_fail().unwrap_err().context("loading repository");
        assert_eq!(e.to_string(), "loading repository");
        let full = format!("{e:#}");
        assert!(full.starts_with("loading repository: reading config: "), "{full}");
        assert_eq!(e.chain().count(), 3);
        assert!(format!("{e:?}").contains("Caused by:"));
    }

    #[test]
    fn options_and_macros() {
        let none: Option<u8> = None;
        assert_eq!(format!("{:#}", none.context("no value").unwrap_err()), "no value");
        let f = |n: u8| -> Result<u8> {
            if n == 0 {
                crate::bail!("zero is not allowed (got {n})");
            }
            Ok(n)
        };
        assert_eq!(f(0).unwrap_err().to_string(), "zero is not allowed (got 0)");
        assert_eq!(crate::err!("x{}", 1).to_string(), "x1");
    }
}
