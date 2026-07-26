// SPDX-License-Identifier: MIT OR Apache-2.0
//! Error type shared by the whole workspace. The three variants map directly
//! onto the CLI exit codes promised by the design (§3): 1 usage/validation,
//! 2 device access, 3 verify mismatch.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Bad manifest, bad arguments, failed validation.
    #[error("{0}")]
    Validation(String),

    /// Device access problems: open, ioctl, raw I/O, permissions.
    #[error("{0}")]
    Device(String),

    /// A verify pass found on-card bytes that differ from the source image.
    #[error("{0}")]
    VerifyMismatch(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Validation(_) => 1,
            Error::Device(_) => 2,
            Error::VerifyMismatch(_) => 3,
        }
    }

    pub fn validation(msg: impl Into<String>) -> Self {
        Error::Validation(msg.into())
    }

    pub fn device(msg: impl Into<String>) -> Self {
        Error::Device(msg.into())
    }

    pub fn verify_mismatch(msg: impl Into<String>) -> Self {
        Error::VerifyMismatch(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Wrap an I/O error with context, classifying it as a device error.
pub fn dev_err(ctx: impl std::fmt::Display, e: std::io::Error) -> Error {
    Error::Device(format!("{ctx}: {e}"))
}

/// Wrap an I/O error with context, classifying it as a validation error
/// (e.g. an unreadable image file named by the user).
pub fn val_err(ctx: impl std::fmt::Display, e: std::io::Error) -> Error {
    Error::Validation(format!("{ctx}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_design() {
        assert_eq!(Error::Validation("x".into()).exit_code(), 1);
        assert_eq!(Error::Device("x".into()).exit_code(), 2);
        assert_eq!(Error::VerifyMismatch("x".into()).exit_code(), 3);
    }

    #[test]
    fn wrappers_classify_and_keep_context() {
        let io = || std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let d = dev_err("opening /dev/x", io());
        assert!(matches!(d, Error::Device(_)));
        assert!(d.to_string().contains("opening /dev/x"));
        let v = val_err("image foo.img", io());
        assert!(matches!(v, Error::Validation(_)));
        assert!(v.to_string().contains("gone"));

        assert!(matches!(Error::validation("v"), Error::Validation(_)));
        assert!(matches!(Error::device("d"), Error::Device(_)));
        assert!(matches!(
            Error::verify_mismatch("m"),
            Error::VerifyMismatch(_)
        ));
    }
}
