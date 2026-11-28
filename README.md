# ameleo-display-agent-win

A Windows guest agent for QEMU `-display dbus`, written in Rust. It replaces spice-vdagent-win:
the host and the agent talk over one virtio-serial port, with a peer-to-peer D-Bus session on it
and one versioned D-Bus interface per feature, as defined by
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol).

It provides the absolute mouse, the clipboard (text, images and files), the file transfer to
Downloads, the client cameras, drives and printers, the display effects, and the signed-in user's
consent before someone views or controls the session.

It runs on Windows 10 (version 1703 or later) and Windows 11. The client cameras need Windows 11
(build 22000 or later), and Media Foundation, which the N editions of Windows only have with the
Media Feature Pack; without them, the agent runs without cameras. The client drives need
[Dokany](https://github.com/dokan-dev/dokany) 2, installed apart, and the client printers need the
Print Support App package (`ameleo-display-agent-print.msix`), which needs Windows 11 too; without
them, the agent runs without drives or printers.

## QEMU setup

```
-device virtio-serial-pci,id=virtserial1
-chardev dbus,id=agent0,name=fr.ameleo.agent.0
-device virtserialport,name=fr.ameleo.agent.0,chardev=agent0,bus=virtserial1.0,nr=1
```

The agent does not use the `qemu-vdagent` chardev or the `com.redhat.spice.0` port.

In the guest, the agent needs the virtio-serial driver (vioserial) of
[virtio-win](https://github.com/virtio-win/virtio-win-pkg-scripts): without it, Windows shows no
port and the agent waits for one, retrying every 5 seconds.

## Repository layout

| Crate | Role |
|---|---|
| `crates/agent` | `ameleo-display-agent.exe`: the Windows service and the session agent |
| `crates/vcam-shm` | The shared memory between the agent and the virtual camera source |
| `crates/vcam-source` | `vcam_source.dll`: the virtual camera media source, loaded by Frame Server |

The modules of `crates/agent/src`, by feature:

| Module | Role |
|---|---|
| `main`, `service`, `session`, `ifaces` | Entry points, the Windows service, the D-Bus session and its `Agent.*` interfaces |
| `channel/` | The virtio-serial port and the D-Bus sessions on it |
| `desktop/` | The desktop thread: the input desktop, the session state, the mouse, the clipboard ownership and the display effects |
| `clipboard/` | The clipboard formats and their conversions |
| `file_clipboard/` | The clipboard of files: file lists, the virtual file data object and its helper process |
| `feature`, `filexfer` | The features with their own interfaces; files sent from the host and saved in Downloads |
| `camera/` | Client cameras: virtual cameras, and the H.264 decoder in its sandbox |
| `drive/` | Client drives, through Dokany |
| `printer/` | Client printers: the link with the Print Support App, jobs, PrintTickets and XPS |
| `consent` | The user's consent: the request dialog, the corner panel and the band around the screens |
| `user`, `win` | User session and Win32 helpers |

## Building

On Windows, with stable Rust:

```
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release -p ameleo-display-agent -p vcam-source --target-dir target\static
```

With the static C runtime, nothing else needs to be installed in the guest. Each release on the
[releases page](https://github.com/ameleo/ameleo-display-agent-win/releases) provides these
files, already built, in a zip: the agent and `vcam_source.dll`. Copy them to a directory of the
guest that the LocalService account can read. The Print Support App package is not in the zip: it
is distributed apart.

## Installing in the guest

From an elevated prompt (in PowerShell, type `sc.exe`, because `sc` is an alias of
`Set-Content`):

```
ameleo-display-agent.exe install
sc.exe start ameleo-display-agent
```

When `vcam_source.dll`, the virtual camera media source, is next to the executable, `install`
also registers it; `regsvr32 vcam_source.dll` does the same by hand. The Frame Server service
loads this DLL, so keep both files in a directory that LocalService can read.

When `ameleo-display-agent-print.msix`, the Print Support App of the client printers, is next to
the executable, `install` also installs it. It trusts `ameleo-display-agent-print.cer`, which
must also be next to the executable, in `LocalMachine\TrustedPeople`, provisions the package for
all users (each user gets it at their next logon), and adds it for the user running `install`.
Running `install` again with a newer package updates it. `uninstall` removes the package for all
users.

If spice-vdagent (the `spice-agent` service) is installed, stop and disable it: both agents
would try to own the clipboard.

The service runs as LocalSystem and starts `ameleo-display-agent.exe session` as SYSTEM in the
active console session. It restarts the session agent when it crashes and when the console moves
to another session.

### Debugging

`ameleo-display-agent.exe console` runs the session agent in the foreground and writes its logs
to stderr. Stop the service first, because only one process can open the port. The console mode
must run elevated, because the vioserial port is not open to regular users (the error is
`Access denied (os error 5)`). Without SYSTEM rights, it cannot follow the logon screen and the
UAC secure desktop.

```powershell
$env:AMELEO_DISPLAY_AGENT_LOG = "debug"
ameleo-display-agent.exe console 2> agent.log
```

The service writes its logs to `C:\ProgramData\ameleo-display-agent\ameleo-display-agent-*.log`.
Logging never blocks the agent: if the writer falls behind, lines are dropped.

| Variable | Default | Effect |
|---|---|---|
| `AMELEO_DISPLAY_AGENT_LOG` | `info` | The `tracing` filter, such as `debug` or `debug,ameleo_display_agent::ifaces=trace` (to see pointer events) |
| `AMELEO_DISPLAY_AGENT_CAMERA_BUFFER` | `3` | The number of frames the virtual camera buffers (1 to 4). It absorbs about that many frame periods of jitter, and adds as much latency |

## How the agent implements the protocol

[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) documents the D-Bus
interfaces for hosts. This section describes how this agent implements them in Windows.

The agent follows the D-Bus framing on the stream and polls `HostConnected` on the port, because
vioserial does not complete a pending read when the host goes away.

### Mouse

The agent matches `head` against the `QueryDisplayConfig` target id, since the display driver
uses the virtio-gpu scanout index as the `VidPnTargetId`; the pointer follows any layout the user
picks. It handles the calls of `Mouse1` one by one in arrival order, so a release never
overtakes its press, and it drops the moves that the next move replaces.

### Clipboard

The agent offers the host's data with delayed rendering: nothing crosses the port until an app
pastes. A paste waits at most 5 seconds for the host, so the pasting app never looks hung.
`Clipboard1.Request` fails with `SessionLocked` while the session is locked: a locked user's
clipboard is not given out.

Files are exchanged the MS-RDPECLIP way (`FileClipboard1`, `Host.FileClipboard1`), as the
`FileGroupDescriptorW` type of `Clipboard1`:

* A copy of files in the guest (`CF_HDROP`, from Explorer) is announced as a file list. The
  agent builds the list, folders included, as the logged on user, when the host asks for it,
  and serves the files' contents by ranges.
* A file list from the host becomes a data object of virtual files (`FileGroupDescriptorW` and
  `FileContents`), which Explorer pastes from. Explorer gives up on the contents of a data
  object owned by another account, so the object lives in
  `ameleo-display-agent.exe clipboard-helper`, which the session agent starts as the logged on
  user and talks to over two pipes. It reads ahead, so a large paste streams.

### Cameras

Each client camera becomes a Windows 11 virtual camera (build 22000 or later), which every app
can use. The camera profile that Windows apps use limits the rate to 30 fps.

Frame Server loads `vcam_source.dll` (as LocalService), which serves NV12 frames in the announced
formats. The agent decodes the H.264 stream with the Media Foundation decoder and passes the
pictures to the source through shared memory (`Global\ameleo-display-agent-vcam-<slot>`, a ring
of 6 frames). The source plays them at the camera rate, a few frames behind
(`AMELEO_DISPLAY_AGENT_CAMERA_BUFFER`), so that late frames and bursts, caused by network jitter
or a slow key frame decode, are not visible.

Camera controls appear as the KS VideoProcAmp and CameraControl property sets of the virtual
camera (the manual and auto flags are swapped between the two sets). Users can change them from
the Windows camera settings, the Camera app, or any app that uses `IAMVideoProcAmp` or
`IAMCameraControl`. Iris, saturation, sharpness, gamma and gain have no MS-RDPECAM equivalent.

The stream comes from the client unchanged, so the decoder does not run in the session agent
(SYSTEM): each camera has its own `ameleo-display-agent.exe camera-decoder` process, in an
AppContainer without capabilities (low integrity, no privileges, no network), in a job (no
child process, 1 GB of memory, no access to the clipboard or to other windows) and with
Microsoft-signed DLLs only. The agent checks every picture's size and copies it into the shared
memory itself; it kills a decoder that crashes, hangs or breaks the protocol, and starts another
one at most once per second.

To update `vcam_source.dll` while Frame Server has it loaded, rename the old file (Windows allows
this for a loaded DLL), copy the new one, then restart Frame Server
(`Restart-Service FrameServer -Force`) so that it loads the new file.

`ameleo-display-agent.exe camera-test [--h264]`, run elevated, shows a test pattern camera
without a host. With `--h264`, the pattern also goes through the encoder and the decoder.

### Printers

Each client printer (`Printer1`, `Host.Printer1`) is a **virtual printer** of a Print Support
App, an MSIX package based on the inbox **Microsoft Virtual Print Class Driver**. No third-party
driver and no IPP are involved: the printers work with Windows protected print and show its
badge in Settings. The app is a separate program, not part of this repository; without it, the
agent has no client printers. The agent and the app talk over the named pipe
`\\.\pipe\ameleo-display-agent-print`, with the `print_link` module of the protocol.

* **Installation.** Windows only installs virtual printers for the calling user, and it refuses
  requests from the agent (SYSTEM), even when the agent impersonates the user. The app therefore
  starts at logon, connects to the agent, and installs, removes and sets the default printers of
  its user on the agent's request. Printers for a user who has not logged on yet are installed
  when the app connects. Each time the app connects, the agent brings the user's virtual
  printers in line with its own list.
* **Printer identity.** The URI of a virtual printer is `ameleo-display-agent:<slot>`. The app's
  tasks pass it to the agent with each request.
* **Options and preferences.** The agent gives the app the client driver's PrintCapabilities and
  default PrintTicket, from which the app builds the printer's capabilities. The printing
  preferences open the client driver's own dialog on the client, through the host
  (`Host.Printer1.DocumentProperties`); the agent waits up to 30 minutes for the user to close it.
* **Jobs.** The app passes each job to the agent as OpenXPS with its PrintTicket. The agent puts
  the ticket for the client driver into the document (`Metadata/Job_PT.xml`) and sends the job to
  the host in chunks, up to 1 GB.
* **Security.** The pipe only accepts processes of the app's package
  (`Ameleo.DisplayAgent.Printers_8ndmhckkynd76`) when that package is installed under
  `%ProgramFiles%\WindowsApps`, so from a package signed by a trusted certificate: a family name
  alone proves nothing, since a package registered from a folder in developer mode can claim any.
  Apart from the capabilities requests, the process must also run in the agent's session.
* **Lifecycle.** The printers are named `<client name> (redirected)`. When a client session ends
  (`Agent1.Reset`, a new D-Bus session), they are kept for 60 seconds, then removed; a printer
  with the same name in the next session reuses the existing one. `RemovePrinter` and stopping
  the agent remove them at once. Removing the app's package removes them all.

### Drives

The client's redirected drives (`Drive1`, `Host.Drive1`) appear as guest drive letters, assigned
from `Z:` downwards.

* **Dokany.** Each drive is a [Dokany](https://github.com/dokan-dev/dokany) 2.x file system
  (user mode file systems, LGPL). Dokany is installed separately, with its own MSI
  (`Dokan_x64.msi` from its releases). The agent loads `dokan2.dll` from System32 at run time;
  without Dokany, the agent works as usual, but `AddDrive` fails.
* **The client's rules.** Each open goes to the client with the app's own NT parameters, so the
  client's file system decides on sharing, existing files and directories, as on a Windows RDP
  server. Nothing of the client's files is kept in the guest, apart from a few seconds of
  listings and file information, which Explorer asks for many times.
* **One user.** Drive letters are global, because the agent runs as SYSTEM, so the agent checks
  every open: only threads of the agent's session get access; other sessions and services get
  access denied.
* **End.** `RemoveDrive`, `Agent1.Reset`, a new D-Bus session and stopping the agent unmount the
  drives. A reset does not wait for the operations running: the drives go on a thread, and a
  drive that the next client session adds is not one of them.

### Consent

Before someone views or controls the session, the host asks the signed-in user
(`Consent1.Ask`):

* The request is a Windows task dialog: who asks, and whether to see the screen or to control
  it. Refuse is the default (Enter, Esc and closing refuse), Accept stays disabled for a
  second, and the request is refused after 30 seconds, as at once while the session is locked,
  the logon screen shows, or another request is shown.
* While a consent lasts, a panel in the bottom right corner says who sees or controls the
  session, with Take control back and Stop (`Host.Consent1.Stopped`); the user may reduce it to
  a tab, not hide it. A thin band runs around every screen, orange under control and blue when
  the screen is only seen. The remote viewer sees it too.
* The session's lock, the logon screen, a new D-Bus session and `Agent1.Reset` stop every
  consent.

The windows belong to the session agent (SYSTEM), so that User Interface Privilege Isolation
keeps the user's programs from clicking them. `cargo test -p ameleo-display-agent
consent::tests::preview -- --ignored` shows them on the desktop and saves pictures of them in
the temp directory.

## Comparison with spice-vdagent

| spice-vdagent | This agent |
|---|---|
| Absolute mouse, multiple monitors, 5 buttons, wheel | Same, plus the horizontal wheel and high resolution scrolling |
| Mouse on the logon screen and the UAC secure desktop | Same (it runs as SYSTEM in the session and follows the input desktop) |
| Clipboard text, both ways, on demand | Same; the agent converts line endings |
| Clipboard images in PNG and BMP | Same, plus the registered `PNG` format, which keeps transparency |
| No file copy and paste through the clipboard | Files and folders, both ways, the MS-RDPECLIP way (`FileClipboard1`) |
| No client drives | Client drives as drive letters, through Dokany (`Drive1`) |
| No client printers | Client printers as virtual printers of the Print Support App, under Windows protected print (`Printer1`) |
| Host clipboard lost when the desktop switches | Announced again on the new desktop |
| Maximum clipboard size | `Clipboard1.MaxSize` property |
| Disabling wallpaper, font smoothing and animations, kept across reboots and applied again at logon | Same |
| File transfer to Downloads, `name (n).ext`, disk space check, refused when locked, opens Explorer | Same, with flow control through method replies |
| Reset when the client disconnects | `Agent1.Reset()`, done implicitly on every new session; it releases the buttons left down and the host clipboard, aborts the file transfers, removes the client cameras, drives and printers, and locks the session; stopping the agent removes them too, and `uninstall` the cameras |
| The service restarts the agent after a crash or a console change | Same |
| Monitor configuration (`SetDisplayConfig`) | **Left out on purpose**: the layout belongs to the display driver |
| Color depth | Left out (meaningless on a virtual GPU) |
| Capability bitmask | Replaced by D-Bus interfaces and introspection |

## Not supported yet

* An installer (MSI) and code signing.

## License

The agent is free software under the GNU General Public License, version 3 only; see
[LICENSE](LICENSE). The D-Bus interfaces it implements,
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol), are under the MIT
or Apache-2.0 license, so that other hosts and agents can implement them.
