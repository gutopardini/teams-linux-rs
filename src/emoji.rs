//! Bounded local Unicode palette. GIFs/stickers require their own service flow.
use crate::i18n::tr;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Category {
    #[default]
    Recent,
    Faces,
    Hands,
    People,
    Animals,
    Food,
    Places,
    Objects,
    Sports,
    Symbols,
}

impl Category {
    pub const ALL: [Self; 10] = [Self::Recent, Self::Faces, Self::Hands, Self::People,
        Self::Animals, Self::Food, Self::Places, Self::Objects, Self::Sports, Self::Symbols];
    pub fn label(self) -> &'static str {
        match self {
            Self::Recent => tr("Recent"), Self::Faces => tr("Smileys and emotion"), Self::Hands => tr("Gestures"),
            Self::People => tr("People"), Self::Animals => tr("Animals and nature"), Self::Food => tr("Food and drink"),
            Self::Places => tr("Travel and places"), Self::Objects => tr("Objects"), Self::Sports => tr("Activities"), Self::Symbols => tr("Symbols"),
        }
    }
}

pub struct Entry { pub value: &'static str, pub name: &'static str, pub category: Category }
impl Entry {
    pub fn label(&self) -> &str {
        crate::reaction_catalog::for_unicode(self.value).map_or(self.value, |entry| entry.label())
    }
}

macro_rules! entries {
    ($($category:ident => [$(($value:literal, $name:literal)),* $(,)?]),* $(,)?) => {
        pub const ENTRIES: &[Entry] = &[$($(Entry { value: $value, name: $name, category: Category::$category },)*)*];
    };
}

entries! {
    Hands => [("👊", "soco cumprimento"), ("🙏", "obrigado oração por favor"), ("🙌", "mãos celebração"),
        ("👏", "palmas parabéns"), ("🫶", "mãos coração amor"), ("👎", "não desaprovar"), ("☝️", "apontar acima"),
        ("🤞", "dedos cruzados sorte"), ("👍", "sim gostei positivo"), ("👋", "olá tchau acenar"), ("👌", "ok perfeito"), ("💪", "força")],
    Faces => [("😆", "rindo sorriso"), ("👀", "olhos olhando"), ("🤯", "cabeça explodindo surpresa"),
        ("🤭", "risada mão boca"), ("😡", "bravo raiva"), ("😀", "feliz sorriso"), ("🤔", "pensando dúvida"),
        ("🥹", "emocionado gratidão"), ("😊", "contente tímido"), ("🤣", "gargalhada rindo"),
        ("😬", "careta nervoso"), ("😰", "preocupado ansiedade"), ("😕", "confuso"), ("💩", "cocô"),
        ("🤩", "estrelas incrível"), ("😸", "gato sorriso"), ("🤐", "boca fechada silêncio"), ("😳", "envergonhado"),
        ("😂", "riso lágrimas"), ("🙃", "cabeça baixo"), ("😎", "óculos sol legal"), ("😍", "apaixonado amor"),
        ("🥳", "festa comemorar"), ("😴", "dormindo sono"), ("😢", "triste lágrima"), ("😭", "chorando"), ("😮", "surpreso")],
    People => [("👩‍💻", "desenvolvedora programadora"), ("👨‍💻", "desenvolvedor programador"),
        ("🧑‍💻", "pessoa computador"), ("👷", "trabalho construção"), ("🧑‍🚀", "astronauta"), ("🧑‍🎓", "estudante formatura")],
    Animals => [("🐶", "cachorro cão"), ("🐱", "gato"), ("🐻", "urso"), ("🐼", "panda"),
        ("🙈", "macaco não vi"), ("🦀", "caranguejo rust"), ("🦋", "borboleta"), ("🌻", "girassol flor"), ("🌱", "planta broto")],
    Food => [("🍕", "pizza"), ("🍔", "hambúrguer"), ("🍎", "maçã"), ("🍌", "banana"),
        ("🍰", "bolo aniversário"), ("☕", "café"), ("🍵", "chá"), ("🍿", "pipoca")],
    Places => [("🚀", "foguete lançamento"), ("✈️", "avião viagem"), ("🚗", "carro"), ("🚲", "bicicleta"),
        ("🏠", "casa"), ("🏢", "escritório prédio"), ("🏖️", "praia férias"), ("🌍", "mundo terra")],
    Objects => [("🎉", "festa celebração"), ("💡", "ideia lâmpada"), ("💻", "computador notebook"),
        ("📱", "celular telefone"), ("📚", "livros estudo"), ("📎", "anexo clipe"), ("🔧", "ferramenta conserto"), ("🕒", "relógio hora")],
    Sports => [("⚽", "futebol bola"), ("🏀", "basquete"), ("🏆", "troféu vitória"), ("🥇", "medalha ouro"), ("🎯", "alvo meta"), ("🎮", "videogame jogo")],
    Symbols => [("❤️", "coração amor"), ("✅", "concluído certo sim"), ("❌", "cancelar não erro"),
        ("⚠️", "atenção aviso"), ("⭐", "estrela favorito"), ("🔥", "fogo"), ("💯", "cem perfeito"), ("🇧🇷", "brasil bandeira"), ("🇺🇸", "estados unidos bandeira")],
}

pub fn supported(value: &str) -> bool { ENTRIES.iter().any(|entry| entry.value == value) }

fn normalize(value: &str) -> String {
    value.to_lowercase().chars().map(|c| match c {
        'á' | 'à' | 'ã' | 'â' => 'a', 'é' | 'ê' => 'e', 'í' => 'i',
        'ó' | 'ô' | 'õ' => 'o', 'ú' => 'u', 'ç' => 'c', _ => c,
    }).collect()
}

pub fn visible(category: Category, query: &str, recent: &[String]) -> Vec<&'static Entry> {
    let query = normalize(query);
    if !query.trim().is_empty() {
        return ENTRIES.iter().filter(|entry| {
            let name = normalize(entry.name);
            query.split_whitespace().all(|word| name.contains(word)) || entry.value == query
                || crate::reaction_catalog::for_unicode(entry.value)
                    .is_some_and(|entry| crate::reaction_catalog::matches_query(entry, &query))
        }).collect();
    }
    if category == Category::Recent && !recent.is_empty() {
        return recent.iter().filter_map(|value| ENTRIES.iter().find(|entry| entry.value == value)).collect();
    }
    ENTRIES.iter().filter(|entry| category == Category::Recent || entry.category == category).collect()
}

pub fn remember(recent: &mut Vec<String>, value: &str) {
    if !supported(value) { return; }
    recent.retain(|old| old != value && supported(old));
    recent.insert(0, value.into());
    recent.truncate(36);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn searches_cross_categories_and_respect_accents_without_accepting_arbitrary_text() {
        assert!(visible(Category::Faces, "programadora", &[]).iter().any(|entry| entry.value == "👩‍💻"));
        assert!(visible(Category::Food, "cafe", &[]).iter().any(|entry| entry.value == "☕"));
        assert!(visible(Category::Hands, "🇧🇷", &[]).iter().any(|entry| entry.value == "🇧🇷"));
        assert!(visible(Category::Faces, "nenhum resultado", &[]).is_empty());
        assert!(!supported("<script>"));
        let values: std::collections::HashSet<_> = ENTRIES.iter().map(|entry| entry.value).collect();
        assert_eq!(values.len(), ENTRIES.len());
    }
    #[test]
    fn recents_are_real_bounded_unique_and_preserve_whole_emoji_sequences() {
        let mut recent = Vec::new();
        for entry in ENTRIES { remember(&mut recent, entry.value); }
        assert_eq!(recent.len(), 36);
        remember(&mut recent, "👩‍💻");
        remember(&mut recent, "👩‍💻");
        assert_eq!(recent[0], "👩‍💻");
        assert_eq!(recent.iter().filter(|value| *value == "👩‍💻").count(), 1);
        let before = recent.clone();
        remember(&mut recent, "arbitrary text");
        assert_eq!(recent, before);
        assert_eq!(visible(Category::Recent, "", &recent)[0].value, "👩‍💻");
    }
}
