# ameleo-display-agent-win

A Windows guest agent for QEMU `-display dbus`, written in Rust. It replaces spice-vdagent-win:
the host and the agent talk over one virtio-serial port, with a peer-to-peer D-Bus session on it
and one versioned D-Bus interface per feature, as defined by
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol).

## License

The agent is free software under the GNU General Public License, version 3 or (at your option)
any later version; see [LICENSE](LICENSE). The D-Bus interfaces it implements,
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol), are under the MIT
or Apache-2.0 license, so that other hosts and agents can implement them.
