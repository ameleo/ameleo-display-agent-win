# ameleo-display-agent-win

A Windows guest agent for QEMU `-display dbus`, written in Rust. It replaces spice-vdagent-win:
the host and the agent talk over one virtio-serial port, with a peer-to-peer D-Bus session on it
and one versioned D-Bus interface per feature, as defined by
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol).

It provides the absolute mouse, the clipboard (text and images), the file transfer to
Downloads, the client cameras and the display effects. Client drives and printers, and the
clipboard of files, are not part of this agent: a companion service can add them (see
[Companion service](#companion-service)).

It runs on Windows 10 (version 1703 or later) and Windows 11. The client cameras need Windows 11
(build 22000 or later), and Media Foundation, which the N editions of Windows only have with the
Media Feature Pack; without them, the agent runs without cameras.

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
| `feature`, `filexfer` | The features with their own interfaces; files sent from the host and saved in Downloads |
| `camera/` | Client cameras: virtual cameras, and the H.264 decoder in its sandbox |
| `companion/` | The link with a companion service, and the interfaces relayed to it |
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
guest that the LocalService account can read.

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

## Companion service

Another Windows service, the companion, can implement more `fr.ameleo.Agent.*` interfaces in
its own process, such as the client drives and printers of the protocol. The agent keeps the
port and relays them as they are; it has no code for those features. The
[`companion` module](https://github.com/ameleo/ameleo-display-protocol/blob/main/src/companion.rs)
of the protocol (0.1.1 and later) documents the link, under the MIT or Apache-2.0 license, so
that a companion does not need any code of this agent. What is particular to this agent:

* The pipe is created by the session agent, so it comes and goes with it: when the console moves
  to another session, or the session agent restarts, the companion must connect and register
  again.
* The agent waits at most 5 seconds for each `fr.ameleo.Companion1` call, and logs the calls
  that fail; `ReadClipboard`, which a request of the host waits for, gets up to 55 seconds.
* The MIME types of the companion's clipboard formats are announced before the agent's own, so
  that a file copy is offered as a file list first.
* `Companion1.Reset` comes on `Agent1.Reset` and on each new D-Bus session with the host.

## Comparison with spice-vdagent

| spice-vdagent | This agent |
|---|---|
| Absolute mouse, multiple monitors, 5 buttons, wheel | Same, plus the horizontal wheel and high resolution scrolling |
| Mouse on the logon screen and the UAC secure desktop | Same (it runs as SYSTEM in the session and follows the input desktop) |
| Clipboard text, both ways, on demand | Same; the agent converts line endings |
| Clipboard images in PNG and BMP | Same, plus the registered `PNG` format, which keeps transparency |
| Host clipboard lost when the desktop switches | Announced again on the new desktop |
| Maximum clipboard size | `Clipboard1.MaxSize` property |
| Disabling wallpaper, font smoothing and animations, kept across reboots and applied again at logon | Same |
| File transfer to Downloads, `name (n).ext`, disk space check, refused when locked, opens Explorer | Same, with flow control through method replies |
| Reset when the client disconnects | `Agent1.Reset()`, done implicitly on every new session; it releases the buttons left down and the host clipboard, aborts the file transfers, removes the client cameras, and locks the session; stopping the agent and `uninstall` remove the cameras too |
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
