use choreo_proto::ProtoError;
use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error(
        "no unlock key available for {0}: add `unlock_key` (base64) to known_servers.toml, or run /unlock <base64 unlock-key>"
    )]
    NoUnlockKey(String),
    #[error("failed to read private key: {0}")]
    PrivateKeyRead(String),
    #[error("invalid private key file: expected 32 bytes")]
    PrivateKeyInvalid,
    #[error("failed to read public key: {0}")]
    PublicKeyRead(String),
    #[error("invalid public key file")]
    PublicKeyInvalid,
    #[error("{0}")]
    CredentialParse(String),
    #[error("postcard serialization failed: {0}")]
    Postcard(String),
    #[error("encryption failed: {0}")]
    Encryption(String),
}

/// Convert an mpsc send error (or any displayable error) into a
/// `ClientError::Io(BrokenPipe)`.  This is the standard pattern when
/// the daemon connection drops and we need to propagate the error
/// through `client_tx.send(...).map_err(broken_pipe)?`.
pub fn broken_pipe(err: impl std::fmt::Display) -> ClientError {
    ClientError::Io(io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
}

impl From<ClientError> for io::Error {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Proto(proto) => io::Error::from(proto),
            ClientError::Io(io) => io,
            ClientError::Utf8(e) => io::Error::new(io::ErrorKind::InvalidData, e),
            ClientError::NoUnlockKey(_)
            | ClientError::PrivateKeyRead(_)
            | ClientError::PrivateKeyInvalid
            | ClientError::PublicKeyRead(_)
            | ClientError::PublicKeyInvalid
            | ClientError::CredentialParse(_)
            | ClientError::Postcard(_)
            | ClientError::Encryption(_) => io::Error::new(io::ErrorKind::InvalidData, error),
        }
    }
}
