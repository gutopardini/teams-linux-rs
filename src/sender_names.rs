//! Display-name fallback by exact canonical identity, using metadata already
//! available in this account. Missing wire names must not poison known names.
use std::collections::HashMap;

pub const UNKNOWN: &str = "Remetente desconhecido";

pub fn known(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty()
        && !matches!(name, "?" | UNKNOWN | "Participante sem nome")
        && crate::avatars::person_id(name).is_none())
    .then_some(name)
}

pub fn remember(names: &mut HashMap<String, String>, id: &str, name: &str) {
    let id = crate::reactions::identity(id);
    if !id.is_empty() {
        if let Some(name) = known(name) {
            names.insert(id, name.to_owned());
        }
    }
}

pub fn resolve(name: &str, id: Option<&str>, names: &HashMap<String, String>) -> String {
    known(name)
        .or_else(|| {
            id.and_then(|id| names.get(&crate::reactions::identity(id)))
                .and_then(|name| known(name))
        })
        .unwrap_or(UNKNOWN)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    const PERSON: &str = "aaaaaaaa-1111-2222-3333-444444444444";
    #[test]
    fn missing_names_do_not_overwrite_known_identity_and_match_mri_case_insensitively() {
        let mut names = HashMap::new();
        remember(&mut names, PERSON, " Ana Paula ");
        for missing in ["?", "", "   ", UNKNOWN, PERSON] {
            remember(&mut names, PERSON, missing);
        }
        assert_eq!(
            resolve(
                "?",
                Some(&format!("8:ORGID:{}", PERSON.to_uppercase())),
                &names
            ),
            "Ana Paula"
        );
        assert_eq!(
            resolve("Nome atualizado", Some(PERSON), &names),
            "Nome atualizado"
        );
        assert_eq!(
            resolve("?", Some("bbbbbbbb-1111-2222-3333-444444444444"), &names),
            UNKNOWN
        );
        assert_eq!(resolve("?", None, &names), UNKNOWN);
    }
}
