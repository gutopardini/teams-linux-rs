//! Minimal per-account index used to restore chats omitted from the server's
//! recent directory after the app restarts. Message previews are never stored.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{atomic::{AtomicU64, Ordering}, Mutex},
};
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_HIDDEN_CHATS: usize = 512;
const MAX_ID_BYTES: usize = 4096;
const MAX_TITLE_BYTES: usize = 2048;

static STORE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct HiddenChat {
    pub id: String,
    pub title: String,
    pub is_group: bool,
}

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct Manifest {
    version: u32,
    chats: Vec<HiddenChat>,
}

impl Store {
    pub fn for_account(account_id: &str) -> Option<Self> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/state"))
            })?;
        Self::at(&base, account_id)
    }

    fn at(base: &Path, account_id: &str) -> Option<Self> {
        if account_id.trim().is_empty() {
            return None;
        }
        let account_hash = format!("{:x}", Sha256::digest(account_id.as_bytes()));
        Some(Self {
            directory: base
                .join("teams-linux-rs/hidden-chats")
                .join(account_hash),
        })
    }

    pub fn load(&self) -> Result<Vec<HiddenChat>, String> {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Não foi possível abrir o índice de conversas ocultas.".to_owned())?;
        self.read_unlocked()
    }

    pub fn set_hidden(
        &self,
        chat_id: &str,
        title: &str,
        is_group: bool,
        hidden: bool,
    ) -> Result<(), String> {
        if !valid_text(chat_id, MAX_ID_BYTES) {
            return Err("O identificador da conversa oculta é inválido.".into());
        }
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Não foi possível atualizar o índice de conversas ocultas.".to_owned())?;
        let mut chats = self.read_unlocked()?;
        chats.retain(|chat| chat.id != chat_id);
        if hidden {
            let title = if valid_text(title.trim(), MAX_TITLE_BYTES) {
                title.trim()
            } else {
                "Conversa oculta"
            };
            if chats.len() >= MAX_HIDDEN_CHATS {
                return Err("O limite local de conversas ocultas foi atingido.".into());
            }
            chats.push(HiddenChat {
                id: chat_id.to_owned(),
                title: title.to_owned(),
                is_group,
            });
        }
        self.write_unlocked(&chats)
    }

    fn read_unlocked(&self) -> Result<Vec<HiddenChat>, String> {
        let path = self.directory.join("hidden.json");
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err("Não foi possível ler as conversas ocultas locais.".into()),
        };
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err("O índice local de conversas ocultas excede o limite permitido.".into());
        }
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| "O índice local de conversas ocultas está inválido.".to_owned())?;
        if manifest.version != STORE_VERSION || manifest.chats.len() > MAX_HIDDEN_CHATS {
            return Err("A versão do índice de conversas ocultas não é compatível.".into());
        }
        let mut chats = Vec::with_capacity(manifest.chats.len());
        for chat in manifest.chats {
            if !valid_text(&chat.id, MAX_ID_BYTES)
                || !valid_text(&chat.title, MAX_TITLE_BYTES)
                || chats.iter().any(|stored: &HiddenChat| stored.id == chat.id)
            {
                continue;
            }
            chats.push(chat);
        }
        Ok(chats)
    }

    fn write_unlocked(&self, chats: &[HiddenChat]) -> Result<(), String> {
        if chats.len() > MAX_HIDDEN_CHATS {
            return Err("O limite local de conversas ocultas foi atingido.".into());
        }
        let manifest = Manifest {
            version: STORE_VERSION,
            chats: chats.to_vec(),
        };
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|_| "Não foi possível preparar o índice de conversas ocultas.".to_owned())?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err("O índice local de conversas ocultas excede o limite permitido.".into());
        }
        fs::create_dir_all(&self.directory)
            .map_err(|_| "Não foi possível criar o índice local de conversas ocultas.".to_owned())?;
        #[cfg(unix)]
        fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| {
                "Não foi possível proteger o índice local de conversas ocultas.".to_owned()
            })?;

        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = self.directory.join(format!(
            ".hidden-{}-{sequence}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temp)
            .map_err(|_| {
                "Não foi possível gravar o índice local de conversas ocultas.".to_owned()
            })?;
        if file.write_all(&bytes).is_err() || file.sync_all().is_err() {
            let _ = fs::remove_file(&temp);
            return Err("Não foi possível gravar o índice local de conversas ocultas.".into());
        }
        drop(file);
        if fs::rename(&temp, self.directory.join("hidden.json")).is_err() {
            let _ = fs::remove_file(&temp);
            return Err("Não foi possível salvar o índice local de conversas ocultas.".into());
        }
        Ok(())
    }
}

fn valid_text(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_store(label: &str) -> (Store, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "teams-linux-rs-hidden-{label}-{}-{nonce}",
            std::process::id()
        ));
        (Store::at(&base, "account-id").unwrap(), base)
    }

    #[test]
    fn hidden_chat_roundtrip_stores_only_directory_metadata_and_can_remove() {
        let (store, root) = temporary_store("roundtrip");
        store
            .set_hidden("19:group@thread.v2", "Projeto", true, true)
            .unwrap();

        assert_eq!(
            store.load().unwrap(),
            vec![HiddenChat {
                id: "19:group@thread.v2".into(),
                title: "Projeto".into(),
                is_group: true,
            }]
        );
        let raw = fs::read_to_string(store.directory.join("hidden.json")).unwrap();
        assert!(!raw.contains("preview"));
        assert!(!raw.contains("message body"));

        store
            .set_hidden("19:group@thread.v2", "Projeto", true, false)
            .unwrap();
        assert!(store.load().unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_storage_path_uses_a_hash_and_empty_accounts_are_rejected() {
        let base = std::env::temp_dir();
        let first = Store::at(&base, "private-account-one").unwrap();
        let second = Store::at(&base, "private-account-two").unwrap();
        assert_ne!(first.directory, second.directory);
        assert!(!first
            .directory
            .to_string_lossy()
            .contains("private-account-one"));
        assert!(Store::at(&base, " ").is_none());
    }

    #[test]
    fn invalid_ids_are_rejected_and_duplicate_hidden_entries_are_replaced() {
        let (store, root) = temporary_store("validation");
        assert!(store.set_hidden("\ninvalid", "Título", false, true).is_err());
        store
            .set_hidden("19:direct@thread.v2", "Pessoa", false, true)
            .unwrap();
        store
            .set_hidden("19:direct@thread.v2", "Pessoa renomeada", false, true)
            .unwrap();
        let chats = store.load().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].title, "Pessoa renomeada");
        let _ = fs::remove_dir_all(root);
    }
}
