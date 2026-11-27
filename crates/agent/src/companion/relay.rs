// SPDX-License-Identifier: GPL-3.0-only

//! D-Bus interfaces relayed as they are, between the host and the
//! companion service.
//!
//! zbus serves an interface under a name fixed by its type, while the
//! relayed names are only known when the companion registers. Each name
//! gets a slot, `Relay<N>`, whose name is set once for the life of the
//! process; [`MAX_SLOTS`] names at most. The slots forward each method call
//! to the other connection, with its body untouched, and send the answer,
//! or the error, back.
//!
//! `zbus::object_server::Interface` is implemented by hand here, which zbus
//! documents as an unstable API: the workspace pins zbus' minor version, and a
//! new one needs this file checked against the trait.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use zbus::Connection;
use zbus::connection::Builder;
use zbus::fdo;
use zbus::message::{Header, Message};
use zbus::names::{InterfaceName, MemberName};
use zbus::object_server::{DispatchResult, Interface, ObjectServer, SignalEmitter};
use zbus::{BoxDBusError, OwnedVariant, Structure, Variant};

/// Relayed interface names, for the whole process. The slots below are
/// written out for 16.
pub const MAX_SLOTS: usize = proto::companion::MAX_RELAYED_INTERFACES;
const _: () = assert!(MAX_SLOTS == 16);

static NAMES: [OnceLock<&'static str>; MAX_SLOTS] = [const { OnceLock::new() }; MAX_SLOTS];
static TAKEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// The slot of `name`, given one if it has none yet.
pub fn slot(name: &str) -> Option<usize> {
    let mut taken = TAKEN.lock().unwrap();
    if let Some(i) = taken.iter().position(|n| *n == name) {
        return Some(i);
    }
    if taken.len() == MAX_SLOTS {
        return None;
    }
    let i = taken.len();
    let name: &'static str = Box::leak(name.to_owned().into_boxed_str());
    NAMES[i].set(name).ok()?;
    taken.push(name);
    Some(i)
}

/// The name of slot `i`, once given.
pub fn name(i: usize) -> Option<&'static str> {
    NAMES.get(i)?.get().copied()
}

/// Where the calls go: the connection of the moment, if any.
pub type Target = Arc<Mutex<Option<Connection>>>;

/// The slot `N`: relays the calls of its interface to `target`.
pub struct Relay<const N: usize> {
    target: Target,
}

/// Adds the relay of `name` at `path` on `conn`.
pub async fn add(conn: &Connection, path: &str, name: &str, target: Target) -> zbus::Result<()> {
    let Some(i) = slot(name) else {
        return Err(zbus::Error::Failure(format!(
            "more than {MAX_SLOTS} relayed interfaces"
        )));
    };
    let server = conn.object_server();
    macro_rules! at {
        ($($n:literal)*) => {
            match i {
                $($n => {
                    server.at(path, Relay::<$n> { target }).await?;
                })*
                _ => unreachable!(),
            }
        };
    }
    at!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);
    Ok(())
}

/// Adds the relay of `name` at `path` to a connection being built.
pub fn serve<'a>(
    builder: Builder<'a>,
    path: &'a str,
    name: &str,
    target: Target,
) -> zbus::Result<Builder<'a>> {
    let Some(i) = slot(name) else {
        return Err(zbus::Error::Failure(format!(
            "more than {MAX_SLOTS} relayed interfaces"
        )));
    };
    macro_rules! at {
        ($($n:literal)*) => {
            match i {
                $($n => Ok(builder.serve_at(path, Relay::<$n> { target })),)*
                _ => unreachable!(),
            }
        };
    }
    at!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15)
}

/// Removes the relay of `name` at `path` from `conn`.
pub async fn remove(conn: &Connection, path: &str, name: &str) -> zbus::Result<()> {
    let Some(i) = TAKEN.lock().unwrap().iter().position(|n| *n == name) else {
        return Ok(());
    };
    let server = conn.object_server();
    macro_rules! rm {
        ($($n:literal)*) => {
            match i {
                $($n => server.remove::<Relay<$n>, _>(path).await?,)*
                _ => unreachable!(),
            }
        };
    }
    rm!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);
    Ok(())
}

/// Forwards the method call `msg` to `target` and answers it.
async fn forward(target: Option<Connection>, conn: &Connection, msg: &Message) -> fdo::Result<()> {
    let hdr = msg.header();
    let res = match target {
        Some(target) => call(&target, &hdr, msg).await,
        None => Err(zbus::Error::Failure(
            "the other side is not connected".into(),
        )),
    };
    let sent = match res {
        Ok(reply) => {
            let body = reply.body();
            if body.signature().to_string().is_empty() {
                conn.reply(&hdr, &()).await
            } else {
                let value: Structure<'_> = body.deserialize()?;
                conn.reply(&hdr, &value).await
            }
        }
        Err(zbus::Error::MethodError(name, text, _)) => {
            conn.reply_error(&hdr, name, &text.unwrap_or_default())
                .await
        }
        Err(e) => {
            conn.reply_dbus_error(&hdr, fdo::Error::Failed(e.to_string()))
                .await
        }
    };
    sent.map(|_| ())
        .map_err(|e| fdo::Error::Failed(e.to_string()))
}

/// The same call, on `target`.
async fn call(target: &Connection, hdr: &Header<'_>, msg: &Message) -> zbus::Result<Message> {
    let path = hdr.path().ok_or(zbus::Error::MissingField)?;
    let member = hdr.member().ok_or(zbus::Error::MissingField)?;
    let body = msg.body();
    if body.signature().to_string().is_empty() {
        target
            .call_method(None::<&str>, path, hdr.interface(), member, &())
            .await
    } else {
        let value: Structure<'_> = body.deserialize()?;
        target
            .call_method(None::<&str>, path, hdr.interface(), member, &value)
            .await
    }
}

#[async_trait::async_trait]
impl<const N: usize> Interface for Relay<N> {
    fn name() -> InterfaceName<'static> {
        let name = NAMES[N].get().copied().unwrap_or("fr.ameleo.Unused");
        InterfaceName::from_static_str_unchecked(name)
    }

    async fn get(
        &self,
        _: &str,
        _: &ObjectServer,
        _: &Connection,
        _: Option<&Header<'_>>,
        _: &SignalEmitter<'_>,
    ) -> Option<Result<OwnedVariant, BoxDBusError>> {
        None
    }

    async fn get_all(
        &self,
        _: &ObjectServer,
        _: &Connection,
        _: Option<&Header<'_>>,
        _: &SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedVariant>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _: &str,
        _: &Variant<'_>,
        _: &ObjectServer,
        _: &Connection,
        _: Option<&Header<'_>>,
        _: &SignalEmitter<'_>,
    ) -> Option<Result<(), BoxDBusError>> {
        None
    }

    fn call<'call>(
        &'call self,
        _: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call Message,
        _: MemberName<'call>,
    ) -> DispatchResult<'call> {
        let target = self.target.lock().unwrap().clone();
        DispatchResult::Async(Box::pin(async move {
            forward(target, connection, msg)
                .await
                .map_err(|e| Box::new(e) as BoxDBusError)
        }))
    }

    fn call_mut<'call>(
        &'call mut self,
        _: &'call ObjectServer,
        _: &'call Connection,
        _: &'call Message,
        _: MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn std::fmt::Write, level: usize) {
        let _ = writeln!(
            writer,
            "{:indent$}<interface name=\"{}\"/>",
            "",
            Self::name(),
            indent = level
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    use zbus::connection::AuthMechanism;

    struct Comp;
    #[zbus::interface(name = "fr.ameleo.Agent.RelayTest1")]
    impl Comp {
        fn echo(&self, n: u32, s: String) -> (String, u32) {
            (s, n + 1)
        }
        fn nothing(&self) {}
        fn single(&self) -> u32 {
            7
        }
        fn fail(&self) -> proto::Result<()> {
            Err(proto::Error::NotFound("no such thing".into()))
        }
    }

    struct HostSide;
    #[zbus::interface(name = "fr.ameleo.Host.RelayTest1")]
    impl HostSide {
        fn double(&self, n: u32) -> u32 {
            n * 2
        }
    }

    /// A server and a client connection over an in-memory stream.
    async fn pair(
        server: impl FnOnce(Builder<'static>) -> Builder<'static>,
        client: impl FnOnce(Builder<'static>) -> Builder<'static>,
    ) -> (Connection, Connection) {
        let (a, b) = tokio::io::duplex(1 << 16);
        let s = server(
            Builder::socket(super::super::link::split(a, oneshot::channel().0))
                .server(zbus::Guid::generate())
                .p2p()
                .auth_mechanism(AuthMechanism::Anonymous),
        );
        let c = client(
            Builder::socket(super::super::link::split(b, oneshot::channel().0))
                .p2p()
                .auth_mechanism(AuthMechanism::Anonymous),
        );
        let (s, c) = tokio::join!(s.build(), c.build());
        (s.unwrap(), c.unwrap())
    }

    #[tokio::test]
    async fn calls_cross_the_relay_both_ways() {
        // host (client) <-> agent (server), agent (server) <-> companion (client)
        let (agent_h, host) = pair(|b| b, |b| b.serve_at(proto::HOST_PATH, HostSide)).await;
        let (agent_c, comp) = pair(|b| b, |b| b.serve_at(proto::AGENT_PATH, Comp)).await;
        let to_comp: Target = Arc::new(Mutex::new(Some(agent_c.clone())));
        let to_host: Target = Arc::new(Mutex::new(Some(agent_h.clone())));
        add(
            &agent_h,
            proto::AGENT_PATH,
            "fr.ameleo.Agent.RelayTest1",
            to_comp.clone(),
        )
        .await
        .unwrap();
        add(
            &agent_c,
            proto::HOST_PATH,
            "fr.ameleo.Host.RelayTest1",
            to_host,
        )
        .await
        .unwrap();

        let iface = Some("fr.ameleo.Agent.RelayTest1");
        let r = host
            .call_method(
                None::<&str>,
                proto::AGENT_PATH,
                iface,
                "Echo",
                &(41u32, "hi"),
            )
            .await
            .unwrap();
        let (s, n): (String, u32) = r.body().deserialize().unwrap();
        assert_eq!((s.as_str(), n), ("hi", 42));

        host.call_method(None::<&str>, proto::AGENT_PATH, iface, "Nothing", &())
            .await
            .unwrap();
        let r = host
            .call_method(None::<&str>, proto::AGENT_PATH, iface, "Single", &())
            .await
            .unwrap();
        assert_eq!(r.body().deserialize::<u32>().unwrap(), 7);

        let err = host
            .call_method(None::<&str>, proto::AGENT_PATH, iface, "Fail", &())
            .await
            .unwrap_err();
        let zbus::Error::MethodError(name, text, _) = err else {
            panic!("{err}")
        };
        assert_eq!(name.as_str(), "fr.ameleo.Agent.Error.NotFound");
        assert_eq!(text.as_deref(), Some("no such thing"));

        let r = comp
            .call_method(
                None::<&str>,
                proto::HOST_PATH,
                Some("fr.ameleo.Host.RelayTest1"),
                "Double",
                &(21u32,),
            )
            .await
            .unwrap();
        assert_eq!(r.body().deserialize::<u32>().unwrap(), 42);

        // The companion gone: a clear error, not a hang.
        *to_comp.lock().unwrap() = None;
        let err = host
            .call_method(None::<&str>, proto::AGENT_PATH, iface, "Nothing", &())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }
}
