# Outgoing links and file cards

Plain outgoing messages use RichText/Html. Previously their URLs were escaped as
text without an anchor, so another Teams client did not offer them as links.
The plain serializer now emits a safe HTTP(S) anchor for URL tokens, escaping
both destination and label. Explicit newlines and spaces remain preserved and
URLs inside fenced code remain plain code. A local HTTP fixture verifies the
actual message body and idempotency field with a synthetic sharing URL.

File attachment headers are now single clickable controls, with a fixed file
icon, a clipped and abbreviated filename that retains its extension, and a
separate open indicator inside the control. The full filename is available on
hover. Existing image thumbnails continue to open in the image viewer. Local
widget tests exercise the left, center, and right of the file card with a
synthetic document. Service behavior and visual parity remain unverified on a
real conversation; previously sent messages are not modified.
