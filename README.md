# ameleo-display-agent-win

A Windows guest agent for QEMU `-display dbus`, written in Rust. It replaces spice-vdagent-win:
the host and the agent talk over one virtio-serial port, with a peer-to-peer D-Bus session on it
and one versioned D-Bus interface per feature, as defined by
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol).

## QEMU setup

```
-device virtio-serial-pci,id=virtserial1
-chardev dbus,id=agent0,name=fr.ameleo.agent.0
-device virtserialport,name=fr.ameleo.agent.0,chardev=agent0,bus=virtserial1.0,nr=1
```

The agent does not use the `qemu-vdagent` chardev or the `com.redhat.spice.0` port.

## Repository layout

| Crate | Role |
|---|---|
| `crates/agent` | `ameleo-display-agent.exe`: the Windows service and the session agent |

The modules of `crates/agent/src`, by feature:

| Module | Role |
|---|---|
| `main`, `service`, `session`, `ifaces` | Entry points, the Windows service, the D-Bus session and its `Agent.*` interfaces |
| `channel/` | The virtio-serial port and the D-Bus sessions on it |
| `desktop/` | The desktop thread: the input desktop and the session state |
| `user`, `win` | User session and Win32 helpers |

## Building

On Windows, with stable Rust:

```
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release -p ameleo-display-agent --target-dir target\static
```

With the static C runtime, nothing else needs to be installed in the guest. Copy the files to a
directory of the guest that the LocalService account can read.

## Installing in the guest

From an elevated prompt (in PowerShell, type `sc.exe`, because `sc` is an alias of
`Set-Content`):

```
ameleo-display-agent.exe install
sc.exe start ameleo-display-agent
```

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
| `AMELEO_DISPLAY_AGENT_LOG` | `info` | The `tracing` filter, such as `debug` |

## How the agent implements the protocol

[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) documents the D-Bus
interfaces for hosts. This section describes how this agent implements them in Windows.

The agent follows the D-Bus framing on the stream and polls `HostConnected` on the port, because
vioserial does not complete a pending read when the host goes away.

## Comparison with spice-vdagent

| spice-vdagent | This agent |
|---|---|
| Reset when the client disconnects | `Agent1.Reset()`, done implicitly on every new session; it also locks the session |
| The service restarts the agent after a crash or a console change | Same |
| Capability bitmask | Replaced by D-Bus interfaces and introspection |

## Not supported yet

* An installer (MSI) and code signing.

## License

The agent is free software under the GNU General Public License, version 3 or (at your option)
any later version; see [LICENSE](LICENSE). The D-Bus interfaces it implements,
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol), are under the MIT
or Apache-2.0 license, so that other hosts and agents can implement them.
