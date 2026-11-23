# Experimental calls and meetings

The desktop launcher enables the libwebrtc path when the application is built
with `libwebrtc-av`. The current build uses a pinned Linux x86-64 artifact
verified by `scripts/prepare-libwebrtc.sh`. PipeWire and the desktop portal
provide screen capture on Wayland.

Synthetic tests use local audio and video fixtures. In an earlier live
meeting, a remote participant confirmed outgoing voice, camera, and screen
sharing. Incoming audio continuity and remote camera display still need more
live validation. Direct calls and meetings remain experimental.

Use the [build and test commands](../README.md#build-and-install) to build that
path locally. See [the notices](../NOTICE) and
[third-party licenses](../third-party-licenses/) for media dependencies.
