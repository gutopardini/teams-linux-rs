# Inter 4.1

Unmodified Regular, Medium, SemiBold, Italic and SemiBoldItalic TTF files from the official release:
https://github.com/rsms/inter/releases/tag/v4.1

Source archive: https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip

Copyright (c) 2016 The Inter Project Authors. SIL Open Font License 1.1;
see LICENSE.txt. Fonts are embedded in the binary and need no system install.

# Selawik 1.01 — default UI family

Regular, Semibold and Bold from Microsoft's official release, unchanged:
https://github.com/microsoft/Selawik/releases/tag/1.01

Archive SHA-256: `3f62c51e05e3b5a1e6241cf92a371f0be2ea1183aa87b30718bbd40832a8d423`.
SIL Open Font License 1.1; see `Selawik-LICENSE.txt` from upstream commit
`89362e84731d1f5777fa078fd4d5ebbd339e4378`.

- Regular: `e9d98518d8ac2817782a9a382430463a2e0793ea68350b695bb727d9a830ee1c`.
- Semibold: `0a9e9d0549a10f24bef9b3a29e06fe6e0b5c21e7a784c503b048a307841a7783`.
- Bold: `f0db5e174a90e0956ad7d2844bdca1d5e6da92ec65b2c04e57ba9b180668c904`.

Selawik is Microsoft's open replacement for Segoe UI, not an exact copy. Upstream
documents kerning/hinting differences. Segoe UI is absent on the tested machine
and the pinned Teams MSIX contains no font files. No Segoe UI binaries were added.
Selawik has no released italic face; the existing Inter italic faces preserve
real italic formatting in the message renderer and editor.

# Color emoji

`TeamsLinuxEmoji.ttf` derives from the CBDT/CBLC Noto Color Emoji font v2.047:
https://github.com/googlefonts/noto-emoji/blob/v2.047/fonts/NotoColorEmoji.ttf

SIL Open Font License 1.1; see `NotoEmoji-LICENSE.txt`. FontTools changed name
records 1/4/16 to `Teams Linux Emoji` and record 6 to `TeamsLinuxEmoji`, avoiding
selection of the system's COLRv1 font under the same family name. Glyphs and
copyright records are unchanged. Source SHA-256:
`39ee3c587e10e89669b9ff32703261d10d5f9c4dd5ad147b6b5a1c5200591817`.
Bundled derivative SHA-256:
`d4a5436e6c58926daefedd7fe402e831d200e760206fe8474defeb0d1935ab75`.

Message spans select this family at whole-grapheme boundaries, preserving ZWJ,
skin-tone modifiers, flags and keycaps without replacing their Unicode text.
