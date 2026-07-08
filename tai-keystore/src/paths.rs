use crate::error::KeystoreError;
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf, KeystoreError> {
    let config_dir = dirs::config_dir().ok_or(KeystoreError::ConfigDirNotFound)?;
    Ok(config_dir.join("tai-daemon"))
}

pub fn private_key_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("identity.pk"))
}

pub fn private_key_enc_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("identity.pk.enc"))
}

pub fn public_key_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("public.pk"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_returns_tai_daemon_subdir() {
        let path = config_dir().unwrap();
        assert!(path.ends_with("tai-daemon"));
    }

    #[test]
    fn private_key_path_ends_with_identity_pk() {
        let path = private_key_path().unwrap();
        assert!(path.ends_with("identity.pk"));
        assert!(path.to_string_lossy().contains("tai-daemon"));
    }

    #[test]
    fn private_key_enc_path_ends_with_identity_pk_enc() {
        let path = private_key_enc_path().unwrap();
        assert!(path.ends_with("identity.pk.enc"));
    }

    #[test]
    fn public_key_path_ends_with_public_pk() {
        let path = public_key_path().unwrap();
        assert!(path.ends_with("public.pk"));
    }

    #[test]
    fn all_paths_share_same_config_dir() {
        let pk = private_key_path().unwrap();
        let enc = private_key_enc_path().unwrap();
        let pubk = public_key_path().unwrap();
        let cfg = config_dir().unwrap();

        assert!(pk.starts_with(&cfg));
        assert!(enc.starts_with(&cfg));
        assert!(pubk.starts_with(&cfg));
    }
}
