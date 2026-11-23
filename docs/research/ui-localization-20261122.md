# UI localization

English is the default for new and existing preferences without a language.
The app saves the language selected in **Settings → Appearance → App language**
and applies it while the settings window is open. The demo can start with
`--language=en` or `--language=pt-BR`.

## Add a language

Add a locale entry to `src/i18n.rs` and embed its JSON catalog under `locales/`.
Use the English source message as the key. Missing entries fall back to that
message. Use named placeholders for interpolated messages so translators can
reorder values. Give the language a label in its own language and include it
in the settings picker. Wire identifiers belong outside the catalogs.

The existing expanded reaction catalog keeps service IDs and artwork metadata.
The English descriptions and search terms are keyed by the same IDs in
`locales/reactions-en.json`. The catalog covers all 1,619 bundled reactions
and supports searching in either language.
[English reaction metadata](https://statics.teams.cdn.office.net/evergreen-assets/personal-expressions/v1/metadata/a098bcb732fd7dd80ce11c12ad15767f/en-us.json)

The default, fallback and legacy preference decoding are covered by local
tests. The migrated areas include the shell, settings, chat, composer,
calendar, call controls and sign-in. Some integration diagnostics and
format strings still need catalog entries.
