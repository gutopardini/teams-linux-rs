//! Per-account local organization. No server mutation or cross-account IDs.
use crate::i18n::tr;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    QuickViews,
    Favorites,
    Chats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Organization {
    pub favorites: Vec<String>,
    pub quick_collapsed: bool,
    pub favorites_collapsed: bool,
    pub chats_collapsed: bool,
}
impl Organization {
    pub fn collapsed(&self, section: Section) -> bool {
        match section {
            Section::QuickViews => self.quick_collapsed,
            Section::Favorites => self.favorites_collapsed,
            Section::Chats => self.chats_collapsed,
        }
    }
    pub fn toggle(&mut self, section: Section) {
        let value = match section {
            Section::QuickViews => &mut self.quick_collapsed,
            Section::Favorites => &mut self.favorites_collapsed,
            Section::Chats => &mut self.chats_collapsed,
        };
        *value = !*value;
    }
    pub fn is_favorite(&self, id: &str) -> bool {
        self.favorites.iter().any(|item| item == id)
    }
    pub fn favorite(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        if !valid_id(id) {
            return Err(tr("This conversation cannot be added to favorites.").into());
        }
        if enabled && !self.is_favorite(id) {
            if self.favorites.len() >= 256 {
                return Err(tr("The favorites limit has been reached.").into());
            }
            self.favorites.push(id.to_owned());
        } else if !enabled {
            self.favorites.retain(|item| item != id);
        }
        Ok(())
    }
    fn valid(&self) -> bool {
        self.favorites.len() <= 256
            && self
                .favorites
                .iter()
                .enumerate()
                .all(|(index, id)| valid_id(id) && !self.favorites[..index].contains(id))
    }
}
fn valid_id(id: &str) -> bool {
    !id.trim().is_empty()
        && id.len() <= 8192
        && !id.chars().any(char::is_control)
        && !crate::graph::is_private_feed_thread(id)
}
#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    revision: Arc<Mutex<u64>>,
}
#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    organization: Organization,
}
impl Store {
    pub fn for_account(id: &str) -> Option<Self> {
        if id.trim().is_empty() {
            return None;
        }
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        Some(Self {
            path: base
                .join("teams-linux-rs/chat-sidebar")
                .join(format!("{:x}", Sha256::digest(id.as_bytes())))
                .join("organization.json"),
            revision: Arc::new(Mutex::new(0)),
        })
    }
    pub fn load(&self) -> Result<Organization, String> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Organization::default());
            }
            Err(_) => return Err(tr("Could not read local favorites.").into()),
        };
        if file
            .metadata()
            .map_err(|_| tr("Could not read local favorites."))?
            .len()
            > 3 * 1024 * 1024
        {
            return Err(tr("The local favorites file exceeds the limit.").into());
        }
        let file: File = serde_json::from_reader(file)
            .map_err(|_| tr("The local favorites file format is invalid."))?;
        if file.version != 1 || !file.organization.valid() {
            return Err(tr("The local favorites version is incompatible.").into());
        }
        Ok(file.organization)
    }
    pub fn save(&self, revision: u64, organization: &Organization) -> Result<(), String> {
        let mut current = self
            .revision
            .lock()
            .map_err(|_| tr("Could not save local favorites."))?;
        if revision <= *current {
            return Ok(());
        }
        if !organization.valid() {
            return Err(tr("Local favorites exceed the allowed limits.").into());
        }
        let parent = self.path.parent().ok_or(tr("The favorites folder is invalid."))?;
        fs::create_dir_all(parent).map_err(|_| tr("Could not create the favorites folder."))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| tr("Could not protect the favorites folder."))?;
        let bytes = serde_json::to_vec(&File {
            version: 1,
            organization: organization.clone(),
        })
        .map_err(|_| tr("Could not prepare favorites."))?;
        let temp = self.path.with_extension("json.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| tr("Could not save local favorites."))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| tr("Could not protect local favorites."))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .and_then(|_| fs::rename(temp, &self.path))
            .map_err(|_| tr("Could not save local favorites."))?;
        *current = revision;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn organization_is_private_atomic_and_old_writes_cannot_win() {
        let root = std::env::temp_dir().join(format!(
            "teams-sidebar-test-{}",
            crate::call_context::native_id()
        ));
        let store = Store {
            path: root.join("organization.json"),
            revision: Arc::new(Mutex::new(0)),
        };
        let mut organization = Organization::default();
        organization.favorite("19:one@thread.v2", true).unwrap();
        organization.toggle(Section::Favorites);
        store.save(2, &organization).unwrap();
        store.save(1, &Organization::default()).unwrap();
        assert_eq!(store.load().unwrap(), organization);
        assert_eq!(
            fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(organization.favorite("48:notifications", true).is_err());
        assert!(organization.favorite("bad\nvalue", true).is_err());
        organization.favorite("19:one@thread.v2", false).unwrap();
        store.save(3, &organization).unwrap();
        assert!(store.load().unwrap().favorites.is_empty());
        fs::write(&store.path, b"invalid").unwrap();
        assert!(store.load().is_err());
        fs::remove_dir_all(root).unwrap();
    }

}
