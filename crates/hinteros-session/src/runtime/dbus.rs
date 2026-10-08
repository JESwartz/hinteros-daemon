//! D-Bus plumbing shared by the runtime settings: connecting, uncached proxies, property
//! maps, change streams with debouncing, and turning D-Bus errors into API errors.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use hinteros_api::session::Service;
use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::json;
use zbus::fdo::PropertiesProxy;
use zbus::message::Type as MessageType;
use zbus::names::InterfaceName;
use zbus::proxy::{Builder, CacheProperties};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, DBusError, MatchRule, MessageStream, Proxy};

pub type PropMap = HashMap<String, OwnedValue>;

/// Every D-Bus call that may wait on a service gets this long (pairing and connecting have
/// their own, longer limits).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(25);

pub async fn connect(bus: Option<&str>) -> zbus::Result<Connection> {
    match bus {
        None => Connection::system().await,
        Some(address) => zbus::connection::Builder::address(address)?.build().await,
    }
}

/// A proxy that reads properties from the service every time (no property cache, so no
/// background tasks per proxy).
pub async fn proxy(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
) -> zbus::Result<Proxy<'static>> {
    Builder::new(conn)
        .destination(dest.to_owned())?
        .path(path.to_owned())?
        .interface(iface.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// All properties of one interface of one object.
pub async fn props(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
) -> zbus::Result<PropMap> {
    let p = PropertiesProxy::builder(conn)
        .destination(dest.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    Ok(p.get_all(InterfaceName::try_from(iface.to_owned())?)
        .await?)
}

/// A property converted to `T`; `None` if it is missing or of another type.
pub fn get<T>(map: &PropMap, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    let v = map.get(key)?.try_clone().ok()?;
    T::try_from(v).ok()
}

/// `a{sa{sv}}` as D-Bus wants it (NetworkManager connection settings).
pub type Settings<'a> = HashMap<&'a str, HashMap<&'a str, Value<'a>>>;

/// Where a D-Bus failure went wrong, for the API.
pub fn api_error(service: Service, e: &zbus::Error) -> ErrorObject {
    let (name, text) = match e {
        zbus::Error::MethodError(name, desc, _) => {
            (name.to_string(), desc.clone().unwrap_or_default())
        }
        zbus::Error::FDO(fdo) => (
            fdo.name().to_string(),
            fdo.description().unwrap_or_default().to_owned(),
        ),
        other => (String::new(), other.to_string()),
    };
    let gone = [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
        "org.freedesktop.DBus.Error.NoServer",
        "org.freedesktop.DBus.Error.Disconnected",
    ];
    if gone.contains(&name.as_str())
        || matches!(
            e,
            zbus::Error::Connection(..) | zbus::Error::Address(_) | zbus::Error::InputOutput(_)
        )
    {
        let reason = if text.is_empty() {
            format!("cannot reach it on D-Bus ({e})")
        } else {
            text
        };
        return unavailable(service, reason);
    }
    let denied = name == "org.freedesktop.DBus.Error.AccessDenied"
        || name.ends_with(".PermissionDenied")
        || name.ends_with(".NotAuthorized")
        || name == "org.freedesktop.PolicyKit1.Error.NotAuthorized";
    if denied {
        let what = if text.is_empty() {
            "access denied"
        } else {
            &text
        };
        return ErrorObject::new(
            ErrorCode::NotAuthorized,
            format!("{} refused: {what}", service_name(service)),
        );
    }
    let text = if text.is_empty() { e.to_string() } else { text };
    failed(
        "service_error",
        format!("{}: {text}", service_name(service)),
    )
}

pub fn service_name(service: Service) -> &'static str {
    match service {
        Service::Networkmanager => "NetworkManager",
        Service::Bluez => "BlueZ",
        Service::Audio => "PipeWire",
        Service::Display => "gamescope",
        Service::Logind => "logind",
        Service::Catalog => "the app catalog",
        Service::Polkit => "polkit",
        Service::Profiles => "profiles",
    }
}

pub fn unavailable(service: Service, reason: impl Into<String>) -> ErrorObject {
    let reason = reason.into();
    ErrorObject::with_data(
        ErrorCode::Unavailable,
        format!("{} is not available: {reason}", service_name(service)),
        json!({ "service": service.name(), "reason": reason }),
    )
}

/// `device_failed`; `reason` is a short snake_case code.
pub fn failed(reason: &str, message: impl Into<String>) -> ErrorObject {
    ErrorObject::with_data(
        ErrorCode::DeviceFailed,
        message,
        json!({ "reason": reason }),
    )
}

pub fn not_found(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(ErrorCode::NotFound, message)
}

/// A stream of "something changed" for a service: every signal it sends, and its name gaining
/// or losing an owner. Callers recompute their state on each tick (debounced with
/// [`settle`]).
pub async fn changes(
    conn: &Connection,
    service: &'static str,
) -> zbus::Result<impl futures_util::Stream<Item = ()> + use<>> {
    let signals = MessageStream::for_match_rule(
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(service)?
            .build(),
        conn,
        Some(256),
    )
    .await?;
    let owner = MessageStream::for_match_rule(
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, service)?
            .build(),
        conn,
        Some(16),
    )
    .await?;
    Ok(futures_util::stream::select(signals, owner).map(|_| ()))
}

/// After a tick, wait until `quiet` passes without another one.
pub async fn settle<S>(stream: &mut S, quiet: Duration)
where
    S: futures_util::Stream<Item = ()> + Unpin,
{
    loop {
        match tokio::time::timeout(quiet, stream.next()).await {
            Ok(Some(())) => {}
            _ => return,
        }
    }
}

/// The unique name that owns `name` now (it changes when the service restarts, even if the
/// restart is quicker than a debounce).
pub async fn owner(conn: &Connection, name: &'static str) -> Option<String> {
    let bus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
    let name = zbus::names::BusName::try_from(name).ok()?;
    bus.get_name_owner(name).await.ok().map(|o| o.to_string())
}
