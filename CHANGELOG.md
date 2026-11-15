# Changelog

This file lists the changes in each release. The builds are available on the
[releases page](https://github.com/ameleo/ameleo-display-agent-win/releases).

## Unreleased

### Added

- **Clipboard of files:** files and folders copied in Explorer go to the client, and files
  copied on the client can be pasted in Explorer, the MS-RDPECLIP way (`FileClipboard1` and
  `Host.FileClipboard1` of the protocol). The guest's file list is built as the logged on
  user; the host's files are offered by `ameleo-display-agent.exe clipboard-helper`, a process
  that the session agent starts as the logged on user, because Explorer gives up on the
  contents of a data object owned by another account.

### Security

- **Camera decoding is sandboxed.** The H.264 stream of a client camera comes from the client
  unchanged, and the agent decoded it in its session process, which runs as SYSTEM: a crafted
  stream that exploited a flaw in the decoder ran its code as SYSTEM. Each camera's decoder now
  runs in its own process, `ameleo-display-agent camera-decoder`:
  - in an AppContainer with no capabilities: low integrity, no privileges, no network, and
    access only to what Windows opens to all app packages;
  - in a job object that allows no child process, limits its memory, blocks access to the
    clipboard and to other processes' windows, and kills it when the agent drops it;
  - with process mitigations that load only Microsoft-signed DLLs.

  The agent checks the size of every picture the decoder returns and copies it into the camera
  itself. A decoder that crashes, hangs for 2 seconds or breaks the protocol is killed and
  replaced, at most once per second. The AppContainer profile is created under the agent's
  account and deleted when the agent stops.

### Fixed

- The session agent panicked when the service stopped, so the service had to kill it after a
  delay; the same panic hit pasting a clipboard format handled by a companion service. Both
  made a Tokio timer outside the runtime.

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
