use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    pub sha256: String,
}

/// Hash a selected file off the UI thread with bounded memory usage.
pub fn fingerprint_async(
    path: PathBuf,
    max_bytes: u64,
) -> impl std::future::Future<Output = Result<Fingerprint, String>> + Send + 'static {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    let started = std::thread::Builder::new()
        .name("teams-file-fingerprint".into())
        .spawn(move || {
            let _ = sender.send(fingerprint_file(&path, max_bytes));
        });

    async move {
        started.map_err(|_| "Não foi possível preparar o arquivo.".to_owned())?;
        receiver
            .await
            .unwrap_or_else(|_| Err("Não foi possível preparar o arquivo.".to_owned()))
    }
}

pub fn fingerprint_file(path: &Path, max_bytes: u64) -> Result<Fingerprint, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|_| "Não foi possível acessar o arquivo selecionado.".to_owned())?;
    let before = file
        .metadata()
        .map_err(|_| "Não foi possível acessar o arquivo selecionado.".to_owned())?;
    if !before.is_file() {
        return Err("Selecione um arquivo, não uma pasta.".to_owned());
    }
    if before.len() > max_bytes {
        return Err(size_limit_message(max_bytes));
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "Não foi possível ler o arquivo selecionado.".to_owned())?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > max_bytes {
            return Err(size_limit_message(max_bytes));
        }
        hasher.update(&buffer[..read]);
    }

    let after = file
        .metadata()
        .map_err(|_| "Não foi possível acessar o arquivo selecionado.".to_owned())?;
    if size != before.len() || after.len() != before.len() {
        return Err("O arquivo mudou enquanto estava sendo preparado. Anexe-o novamente.".into());
    }

    Ok(Fingerprint {
        size,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn size_limit_message(max_bytes: u64) -> String {
    let mebibytes = max_bytes.saturating_add(1_048_575) / 1_048_576;
    format!("O arquivo ultrapassa o limite local de {mebibytes} MiB.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(bytes: &[u8], suffix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "teams-linux-fingerprint-{}-{}-{suffix}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn hashes_file_in_chunks_and_reports_size() {
        let bytes = vec![b'a'; 700_000];
        let path = temp_file(&bytes, "large");
        let fingerprint = fingerprint_file(&path, 1_000_000).unwrap();

        assert_eq!(fingerprint.size, bytes.len() as u64);
        assert_eq!(fingerprint.sha256, format!("{:x}", Sha256::digest(&bytes)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn content_changes_with_same_size_have_different_fingerprints() {
        let path = temp_file(b"same-size", "same-size");
        let first = fingerprint_file(&path, 100).unwrap();
        std::fs::write(&path, b"different").unwrap();
        let second = fingerprint_file(&path, 100).unwrap();

        assert_eq!(first.size, second.size);
        assert_ne!(first.sha256, second.sha256);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_files_larger_than_the_configured_limit() {
        let path = temp_file(b"12345", "limit");
        let result = fingerprint_file(&path, 4);

        assert!(result.unwrap_err().contains("1 MiB"));
        std::fs::remove_file(path).unwrap();
    }
}
