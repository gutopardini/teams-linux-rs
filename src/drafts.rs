//! Per-account durable chat drafts. Message text and staged images remain local.
use crate::{images, message_body::ComposerFormatting};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const STORE_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_DRAFTS: usize = 1024;
const MAX_STORED_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_STORED_IMAGE_TOTAL: usize = 128 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct DraftSnapshot {
    pub text: HashMap<String, String>,
    pub formatting: HashMap<String, ComposerFormatting>,
    pub images: HashMap<String, Vec<images::StagedImage>>,
}

#[derive(Clone, Default)]
pub struct LoadedDrafts {
    pub text: HashMap<String, String>,
    pub formatting: HashMap<String, ComposerFormatting>,
    pub images: HashMap<String, Vec<images::StagedImage>>,
    pub skipped_images: usize,
}

#[derive(Clone)]
pub struct DraftStore {
    directory: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    drafts: Vec<DraftRecord>,
}

#[derive(Serialize, Deserialize)]
struct DraftRecord {
    key: String,
    text: String,
    #[serde(default)]
    formatting: ComposerFormatting,
    #[serde(default)]
    images: Vec<StoredImage>,
}

#[derive(Serialize, Deserialize)]
struct StoredImage {
    key: String,
    sha256: String,
    #[serde(default)]
    remote: Option<images::InlineImage>,
    #[serde(default)]
    uploaded: bool,
}

impl DraftStore {
    pub fn for_account(account_id: &str) -> Option<Self> {
        if account_id.trim().is_empty() {
            return None;
        }
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/state"))
            })?;
        let account_hash = format!("{:x}", Sha256::digest(account_id.as_bytes()));
        Some(Self::at(
            base.join("teams-linux-rs/drafts").join(account_hash),
        ))
    }

    fn at(directory: PathBuf) -> Self {
        Self {
            directory,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn load(&self) -> Result<LoadedDrafts, String> {
        let manifest_path = self.directory.join("drafts.json");
        let bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedDrafts::default());
            }
            Err(_) => return Err("Não foi possível ler os rascunhos locais.".into()),
        };
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err("O arquivo de rascunhos locais ultrapassa o limite permitido.".into());
        }
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| "O arquivo de rascunhos locais não pôde ser interpretado.".to_owned())?;
        if manifest.version != STORE_VERSION || manifest.drafts.len() > MAX_DRAFTS {
            return Err("A versão dos rascunhos locais não é compatível.".into());
        }

        let mut loaded = LoadedDrafts::default();
        let mut total_image_bytes = 0usize;
        for mut record in manifest.drafts {
            if record.key.is_empty() || record.key.len() > 8192 {
                continue;
            }
            normalize_formatting(&mut record.formatting, &record.text);
            loaded.text.insert(record.key.clone(), record.text);
            if !record.formatting.is_empty() {
                loaded
                    .formatting
                    .insert(record.key.clone(), record.formatting);
            }

            let mut restored_images = Vec::new();
            if record.images.len() > images::MAX_IMAGES {
                loaded.skipped_images += record.images.len() - images::MAX_IMAGES;
            }
            for stored in record.images.into_iter().take(images::MAX_IMAGES) {
                if stored.key.is_empty() || stored.key.len() > 256 || !is_sha256(&stored.sha256) {
                    loaded.skipped_images += 1;
                    continue;
                }
                let path = self
                    .directory
                    .join("images")
                    .join(format!("{}.png", stored.sha256));
                let Ok(bytes) = fs::read(path) else {
                    loaded.skipped_images += 1;
                    continue;
                };
                if bytes.len() > MAX_STORED_IMAGE_BYTES
                    || total_image_bytes.saturating_add(bytes.len()) > MAX_STORED_IMAGE_TOTAL
                    || sha256_hex(&bytes) != stored.sha256
                {
                    loaded.skipped_images += 1;
                    continue;
                }
                let Ok(mut image) = images::stage_png(bytes) else {
                    loaded.skipped_images += 1;
                    continue;
                };
                total_image_bytes = total_image_bytes.saturating_add(image.png.len());
                image.key = stored.key;
                image.remote = stored
                    .remote
                    .filter(|remote| valid_remote(remote, image.width, image.height));
                image.uploaded = stored.uploaded && image.remote.is_some();
                restored_images.push(image);
            }
            if !restored_images.is_empty() {
                loaded.images.insert(record.key, restored_images);
            }
        }
        Ok(loaded)
    }

    pub fn save(&self, snapshot: &DraftSnapshot) -> Result<(), String> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| "O armazenamento de rascunhos está indisponível.".to_owned())?;
        ensure_private_dir(&self.directory)?;
        let images_dir = self.directory.join("images");
        ensure_private_dir(&images_dir)?;

        let mut keys = BTreeSet::new();
        keys.extend(snapshot.text.keys().cloned());
        keys.extend(snapshot.formatting.keys().cloned());
        keys.extend(snapshot.images.keys().cloned());
        if keys.len() > MAX_DRAFTS {
            return Err("Há rascunhos demais para salvar localmente.".into());
        }

        let mut records = Vec::with_capacity(keys.len());
        let mut active_blobs = BTreeSet::new();
        let mut total_image_bytes = 0usize;
        for key in keys {
            let text = snapshot.text.get(&key).cloned().unwrap_or_default();
            let formatting = snapshot.formatting.get(&key).cloned().unwrap_or_default();
            let pictures = snapshot
                .images
                .get(&key)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if text.is_empty() && formatting.is_empty() && pictures.is_empty() {
                continue;
            }
            if key.len() > 8192 || pictures.len() > images::MAX_IMAGES {
                return Err("Um rascunho excede os limites de armazenamento.".into());
            }

            let mut stored_images = Vec::with_capacity(pictures.len());
            for picture in pictures {
                if picture.png.len() > MAX_STORED_IMAGE_BYTES
                    || total_image_bytes.saturating_add(picture.png.len()) > MAX_STORED_IMAGE_TOTAL
                {
                    return Err(
                        "As imagens dos rascunhos excedem o limite de armazenamento.".into(),
                    );
                }
                total_image_bytes = total_image_bytes.saturating_add(picture.png.len());
                let digest = sha256_hex(&picture.png);
                let path = images_dir.join(format!("{digest}.png"));
                let existing_size = fs::metadata(&path).ok().map(|metadata| metadata.len());
                if existing_size != Some(picture.png.len() as u64) {
                    write_atomic(&path, &picture.png)?;
                }
                active_blobs.insert(format!("{digest}.png"));
                stored_images.push(StoredImage {
                    key: picture.key.clone(),
                    sha256: digest,
                    remote: picture.remote.clone(),
                    uploaded: picture.uploaded,
                });
            }
            records.push(DraftRecord {
                key,
                text,
                formatting,
                images: stored_images,
            });
        }

        let manifest = Manifest {
            version: STORE_VERSION,
            drafts: records,
        };
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|_| "Não foi possível preparar os rascunhos locais.".to_owned())?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err("O texto dos rascunhos excede o limite de armazenamento.".into());
        }
        write_atomic(&self.directory.join("drafts.json"), &bytes)?;
        remove_unreferenced_blobs(&images_dir, &active_blobs);
        Ok(())
    }
}

fn normalize_formatting(formatting: &mut ComposerFormatting, text: &str) {
    let graphemes = unicode_segmentation::UnicodeSegmentation::graphemes(text, true).count();
    let parts:Vec<_>=unicode_segmentation::UnicodeSegmentation::graphemes(text,true).collect();
    formatting.mentions.retain(|r|r.start<r.end && r.end<=parts.len() && parts[r.start..r.end].concat()==r.entry.name
        && (!r.entry.everyone && crate::avatars::person_id(&r.entry.id).is_some() || r.entry.everyone && r.entry.id.starts_with("19:")));
    formatting.mentions.truncate(100);
    formatting
        .marks
        .retain(|range| range.start < range.end && range.end <= graphemes);
    formatting.sizes.retain(|range| {
        range.start < range.end
            && range.end <= graphemes
            && (75..=200).contains(&range.percent)
            && range.percent != 100
    });
    formatting.colors.retain(|range| {
        range.start < range.end && range.end <= graphemes && range.color <= 0x00ff_ffff
    });
    formatting.highlights.retain(|range| {
        range.start < range.end && range.end <= graphemes && range.color <= 0x00ff_ffff
    });
    formatting
        .links
        .retain(|range| range.start < range.end && range.end <= graphemes);
    let lines = text.lines().count().max(1);
    formatting
        .lists
        .retain(|range| range.start < range.end && range.end <= lines);
    formatting
        .code_blocks
        .retain(|range| range.start < range.end && range.end <= lines);
    formatting
        .quote_blocks
        .retain(|range| range.start < range.end && range.end <= lines);
    formatting.tables.retain(|range| {
        range.start < range.end
            && range.end <= lines
            && (1..=12).contains(&range.columns)
    });
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_remote(image: &images::InlineImage, width: u32, height: u32) -> bool {
    if image.id.is_empty()
        || image.id.len() > 200
        || !image
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        || image.width != width
        || image.height != height
    {
        return false;
    }
    let Ok(url) = url::Url::parse(&image.url) else {
        return false;
    };
    images::valid_media_url(&image.url)
        && url.path() == format!("/v1/objects/{}/views/imgo", image.id)
        && url.query().is_none()
        && url.fragment().is_none()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|_| "Não foi possível criar o armazenamento de rascunhos.".to_owned())?;
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|_| "Não foi possível proteger o armazenamento de rascunhos.".to_owned())?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "O caminho do armazenamento de rascunhos é inválido.".to_owned())?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "O caminho do armazenamento de rascunhos é inválido.".to_owned())?;
    let temp = parent.join(format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = (|| {
        let mut file = options
            .open(&temp)
            .map_err(|_| "Não foi possível criar o arquivo local de rascunhos.".to_owned())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| "Não foi possível gravar os rascunhos locais.".to_owned())?;
        drop(file);
        fs::rename(&temp, path)
            .map_err(|_| "Não foi possível confirmar o salvamento dos rascunhos.".to_owned())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn remove_unreferenced_blobs(images_dir: &Path, active: &BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(images_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.ends_with(".png") && !active.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_body::{
        ColorRange, FormatRange, InlineMark, LinkRange, ListKind, ListRange, SizeRange,
    };

    fn temp_store(name: &str) -> DraftStore {
        let path = std::env::temp_dir().join(format!(
            "teams-linux-drafts-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        DraftStore::at(path)
    }

    #[test]
    fn account_store_path_uses_a_digest_instead_of_the_account_identifier() {
        let store = DraftStore::for_account("private-account-id").unwrap();
        assert!(
            !store
                .directory
                .to_string_lossy()
                .contains("private-account-id")
        );
        assert_eq!(
            store.directory.file_name().unwrap().to_string_lossy().len(),
            64
        );
        assert!(DraftStore::for_account("  ").is_none());
    }

    #[test]
    fn text_and_rich_formatting_roundtrip_and_empty_draft_removal_cleans_manifest() {
        let store = temp_store("roundtrip");
        let snapshot = DraftSnapshot {
            text: HashMap::from([("chat:one".into(), "Olá 👋".into())]),
            formatting: HashMap::from([(
                "chat:one".into(),
                ComposerFormatting {
                    marks: vec![FormatRange {
                        start: 0,
                        end: 4,
                        mark: InlineMark::Bold,
                    }],
                    sizes: vec![SizeRange {
                        start: 4,
                        end: 5,
                        percent: 150,
                    }],
                    colors: vec![ColorRange {
                        start: 4,
                        end: 5,
                        color: 0xc50f1f,
                    }],
                    links: vec![LinkRange {
                        start: 1,
                        end: 4,
                        url: "https://example.com".into(),
                    }],
                    lists: vec![ListRange {
                        start: 0,
                        end: 1,
                        kind: ListKind::Unordered,
                    }],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        store.save(&snapshot).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.text, snapshot.text);
        assert_eq!(loaded.formatting, snapshot.formatting);
        assert!(loaded.images.is_empty());
        assert_eq!(loaded.skipped_images, 0);

        store.save(&DraftSnapshot::default()).unwrap();
        assert!(store.load().unwrap().text.is_empty());
        let _ = fs::remove_dir_all(&store.directory);
    }

    #[test]
    fn malformed_format_ranges_are_discarded_without_losing_the_draft_text() {
        let store = temp_store("format-bounds");
        let mut formatting = ComposerFormatting::default();
        formatting.marks.push(FormatRange {
            start: 0,
            end: 99,
            mark: InlineMark::Italic,
        });
        formatting.lists.push(ListRange {
            start: 4,
            end: 8,
            kind: ListKind::Ordered,
        });
        let snapshot = DraftSnapshot {
            text: HashMap::from([("chat:one".into(), "oi".into())]),
            formatting: HashMap::from([("chat:one".into(), formatting)]),
            ..Default::default()
        };
        store.save(&snapshot).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.text["chat:one"], "oi");
        assert!(!loaded.formatting.contains_key("chat:one"));
        let _ = fs::remove_dir_all(&store.directory);
    }

    #[test]
    fn staged_image_and_upload_reservation_roundtrip_by_content_hash() {
        let store = temp_store("image-roundtrip");
        let mut image = images::demo_image();
        image.remote = Some(images::InlineImage {
            id: "object-1".into(),
            url: "https://api.asm.skype.com/v1/objects/object-1/views/imgo".into(),
            width: image.width,
            height: image.height,
        });
        image.uploaded = true;
        let image_key = image.key.clone();
        let snapshot = DraftSnapshot {
            text: HashMap::from([("chat:one".into(), "legenda".into())]),
            images: HashMap::from([("chat:one".into(), vec![image.clone()])]),
            ..Default::default()
        };
        store.save(&snapshot).unwrap();
        let loaded = store.load().unwrap();
        let restored = &loaded.images["chat:one"][0];
        assert_eq!(restored.key, image_key);
        assert_eq!(restored.png.as_slice(), image.png.as_slice());
        assert_eq!(restored.remote, image.remote);
        assert!(restored.uploaded);
        assert_eq!(
            (restored.width, restored.height),
            (image.width, image.height)
        );
        assert_eq!(loaded.skipped_images, 0);

        store.save(&DraftSnapshot::default()).unwrap();
        assert_eq!(
            fs::read_dir(store.directory.join("images"))
                .unwrap()
                .count(),
            0
        );
        let _ = fs::remove_dir_all(&store.directory);
    }

    #[test]
    fn invalid_cached_media_reservation_is_reset_but_the_local_png_survives() {
        let store = temp_store("invalid-reservation");
        let mut image = images::demo_image();
        image.remote = Some(images::InlineImage {
            id: "../../unexpected".into(),
            url: "https://example.invalid/v1/objects/unexpected/views/imgo".into(),
            width: image.width,
            height: image.height,
        });
        image.uploaded = true;
        let snapshot = DraftSnapshot {
            images: HashMap::from([("chat:one".into(), vec![image.clone()])]),
            ..Default::default()
        };
        store.save(&snapshot).unwrap();
        let loaded = store.load().unwrap();
        let restored = &loaded.images["chat:one"][0];
        assert_eq!(restored.png.as_slice(), image.png.as_slice());
        assert!(restored.remote.is_none());
        assert!(!restored.uploaded);
        let _ = fs::remove_dir_all(&store.directory);
    }
}
