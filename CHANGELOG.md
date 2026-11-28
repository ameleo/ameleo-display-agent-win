# Changelog

This file lists the changes in each release. The builds are available on the
[releases page](https://github.com/ameleo/ameleo-display-agent-win/releases).

## 0.4.0 — 2026-11-28

Client drives and printers are part of the agent, which no longer relays interfaces to a
companion service. It implements
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) 0.4.0, on zbus 6.
The release archive holds the agent and the camera source; the Print Support App of the client
printers (`ameleo-display-agent-print.msix`) is distributed apart.

### Added

- **Client drives** (`Drive1`, `Host.Drive1`): the client's redirected drives appear as guest
  drive letters, from `Z:` downwards, each a [Dokany](https://github.com/dokan-dev/dokany) 2 file
  system. Every operation goes to the client; only threads of the agent's session may open the
  files. Dokany is installed apart; without it, `AddDrive` fails and the agent runs as usual.
- **Client printers** (`Printer1`, `Host.Printer1`): each client printer is a virtual printer of
  a Print Support App, a separate MSIX package on the inbox Microsoft Virtual Print Class Driver,
  so the printers work under Windows protected print. The agent installs the package with
  `install` when it is next to the executable, gives the app the client drivers' capabilities,
  opens the client driver's preferences on the client, and sends the jobs to the host as OpenXPS
  with the client's PrintTicket. Its pipe accepts only the app's package installed under
  `WindowsApps`, not one registered from a folder in developer mode with the same family name.

### Changed

- zbus 6, and [ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol)
  0.4.0. The D-Bus messages do not change: a host on zbus 5 still speaks to the agent.

### Removed

- The companion service link: the agent no longer serves the
  `\\.\pipe\ameleo-display-agent-companion` pipe and no longer relays interfaces to another
  service. Version 0.4.0 of the protocol drops its `companion` module.

### Security

- `Agent1.Reset` locks the session each time a user is logged on, instead of trusting the
  agent's cached lock state: a stale state could leave the session open behind the client that
  went away.

### Fixed

- A late `Agent1.Reset` could act on the next client session. `Agent1` is now handled one call
  at a time: a reset is taken before any call that comes after it.

## 0.3.0 — 2026-11-27

The agent asks the signed-in user before someone views or controls the session. It implements
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) 0.1.2.

### Added

- **Consent** (`Consent1`, `Host.Consent1`): the host asks whether someone may see the session
  or control it, and the user answers in a Windows task dialog. The request defaults to no:
  Enter, Esc and closing refuse, Accept stays disabled for a second, and it is refused after 30
  seconds, or at once while the session is locked or the logon screen shows. While a consent
  lasts, a panel in the bottom right corner, which the user may reduce to a tab, shows who sees
  or controls the session, with Take control back and Stop, and a thin band of its color runs
  around every screen. Locking the session, the logon screen, a new D-Bus session and
  `Agent1.Reset` stop every consent.
- The executable embeds an application manifest: Common Controls 6 and per-monitor DPI
  awareness.

### Fixed

- `SessionLocked` stayed false after `LockWorkStation`: the lock notification went to the window
  of the desktop being replaced. The lock state now follows Windows itself each time the input
  desktop switches, and a sign-out counts as locked.

## 0.2.0 — 2026-11-15

The clipboard now carries files and folders both ways, and the camera decoder runs in a sandbox
instead of the SYSTEM session agent. Like 0.1.0, it implements
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) 0.1.1.

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
