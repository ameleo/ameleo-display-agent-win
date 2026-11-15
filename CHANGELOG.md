# Changelog

This file lists the changes in each release. The builds are available on the
[releases page](https://github.com/ameleo/ameleo-display-agent-win/releases).

## 0.1.0 — 2026-11-15

The first release: a Windows guest agent for QEMU `-display dbus`, written in Rust, which
replaces spice-vdagent-win and implements
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) 0.1.1. It talks to
the host over one virtio-serial port, with a peer-to-peer D-Bus session and one versioned D-Bus
interface per feature. It runs as a service (LocalSystem) and follows the console session,
including the logon screen and the UAC secure desktop. It runs on Windows 10 (version 1703 or
later) and Windows 11; the client cameras need Windows 11 (build 22000 or later) and Media
Foundation, and the agent runs without them otherwise.

- **Mouse:** absolute positioning on several monitors, five buttons, both wheels and high
  resolution scrolling.
- **Clipboard:** text and images (PNG with transparency, and BMP), in both directions, on
  demand.
- **File transfer:** files sent from the client are saved in Downloads.
- **Client cameras:** shown as virtual cameras (a Frame Server media source); the agent decodes
  the H.264 stream in the guest.
- **Display effects:** wallpaper, font smoothing and animations can be turned off on request, and
  are restored afterwards.
- **Companion service:** another Windows service can add `fr.ameleo.Agent.*` interfaces and
  clipboard formats in its own process; the agent relays them over a local named pipe.

The release archive contains the agent (`ameleo-display-agent.exe`) and the camera source
(`vcam_source.dll`). There is no installer (MSI) yet, and no code signing by a public authority.
