use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct PartFile {
    path: PathBuf,
}

impl PartFile {
    pub fn create_new(path: &Path, expected_size: Option<u64>) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        let file = options.open(path).map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => {
                Error::User(format!("destino parcial já existe: {}", path.display()))
            }
            _ => error.into(),
        })?;
        if let Some(size) = expected_size {
            file.set_len(size)?;
        }
        file.sync_all()?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(Error::User("arquivo parcial não encontrado".into()));
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn reset(path: &Path) -> Result<Self> {
        if path.exists() {
            fs::remove_file(path)?;
        }
        Self::create_new(path, None)
    }

    pub fn write_at(&self, mut offset: u64, mut bytes: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        let file = OpenOptions::new().write(true).open(&self.path)?;
        while !bytes.is_empty() {
            let written = file.write_at(bytes, offset)?;
            if written == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "não foi possível gravar arquivo parcial",
                )));
            }
            offset += written as u64;
            bytes = &bytes[written..];
        }
        Ok(())
    }

    pub fn append(&self, bytes: &[u8]) -> Result<()> {
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(bytes)?;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        OpenOptions::new()
            .read(true)
            .open(&self.path)?
            .sync_data()?;
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(fs::metadata(&self.path)?.len())
    }

    pub fn finalize(
        &self,
        destination: &Path,
        expected_size: Option<u64>,
        expected_sha256: Option<&str>,
    ) -> Result<()> {
        self.sync()?;
        if let Some(expected_size) = expected_size {
            let actual = self.len()?;
            if actual != expected_size {
                return Err(Error::User(format!(
                    "tamanho final inválido: esperado {expected_size}, obtido {actual}"
                )));
            }
        }
        if let Some(expected) = expected_sha256 {
            let actual = sha256_file(&self.path)?;
            if actual != expected {
                return Err(Error::User(
                    "SHA-256 não confere; o arquivo parcial foi preservado".into(),
                ));
            }
        }
        if destination.exists() {
            return Err(Error::User(format!(
                "destino já existe: {}",
                destination.display()
            )));
        }
        fs::rename(&self.path, destination)?;
        sync_directory(destination.parent().unwrap_or_else(|| Path::new(".")))?;
        Ok(())
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
