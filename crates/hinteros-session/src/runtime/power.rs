//! Suspend, reboot and power off, straight to logind : the system's
//! polkit rules let the active local session user do it, and logind keeps its own inhibitor
//! locks. The system service holds a `block` inhibitor while a switch activates, so logind
//! refuses then and we pass its message on as `power_inhibited`.

use hinteros_api::session::{Empty, PowerCapabilities, PowerCapability, Service};
use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::json;
use tracing::info;

use super::Ctx;
use super::dbus::{api_error, proxy};

const SERVICE: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const IFACE: &str = "org.freedesktop.login1.Manager";

fn capability(answer: &str) -> PowerCapability {
    match answer {
        "yes" => PowerCapability::Yes,
        "challenge" => PowerCapability::Challenge,
        "no" => PowerCapability::No,
        _ => PowerCapability::Na,
    }
}

pub async fn capabilities(ctx: &Ctx) -> Result<PowerCapabilities, ErrorObject> {
    let conn = ctx.conn(Service::Logind).await?;
    let p = proxy(&conn, SERVICE, PATH, IFACE)
        .await
        .map_err(|e| api_error(Service::Logind, &e))?;
    let ask = async |method: &str| -> Result<PowerCapability, ErrorObject> {
        let answer: String = p
            .call(method, &())
            .await
            .map_err(|e| api_error(Service::Logind, &e))?;
        Ok(capability(&answer))
    };
    Ok(PowerCapabilities {
        suspend: ask("CanSuspend").await?,
        reboot: ask("CanReboot").await?,
        poweroff: ask("CanPowerOff").await?,
    })
}

/// Call `Suspend`, `Reboot` or `PowerOff` (never interactive: nobody could answer polkit).
async fn act(ctx: &Ctx, method: &str) -> Result<Empty, ErrorObject> {
    let conn = ctx.conn(Service::Logind).await?;
    let p = proxy(&conn, SERVICE, PATH, IFACE)
        .await
        .map_err(|e| api_error(Service::Logind, &e))?;
    info!(method, "asking logind");
    p.call::<_, _, ()>(method, &(false,))
        .await
        .map_err(|e| power_error(&e))?;
    Ok(Empty {})
}

pub async fn suspend(ctx: &Ctx) -> Result<Empty, ErrorObject> {
    act(ctx, "Suspend").await
}

pub async fn reboot(ctx: &Ctx) -> Result<Empty, ErrorObject> {
    act(ctx, "Reboot").await
}

pub async fn poweroff(ctx: &Ctx) -> Result<Empty, ErrorObject> {
    act(ctx, "PowerOff").await
}

/// logind's inhibitor refusal as `power_inhibited`, otherwise the usual mapping.
fn power_error(e: &zbus::Error) -> ErrorObject {
    if let zbus::Error::MethodError(name, desc, _) = e {
        let text = desc.clone().unwrap_or_default();
        if name.contains("BlockedByInhibitor") || text.contains("Operation inhibited") {
            let (who, why) = parse_inhibitor(&text);
            let data = match (who, why) {
                (Some(who), Some(why)) => Some(json!({ "who": who, "why": why })),
                (Some(who), None) => Some(json!({ "who": who })),
                _ => None,
            };
            let message = if text.is_empty() {
                "logind refused: an inhibitor lock blocks it".to_owned()
            } else {
                text
            };
            return match data {
                Some(data) => ErrorObject::with_data(ErrorCode::PowerInhibited, message, data),
                None => ErrorObject::new(ErrorCode::PowerInhibited, message),
            };
        }
    }
    api_error(Service::Logind, e)
}

/// `Operation inhibited by "who" (PID 1, user x), reason is "why".`
fn parse_inhibitor(text: &str) -> (Option<String>, Option<String>) {
    let quoted = |after: &str| {
        let rest = text.split_once(after)?.1;
        let rest = rest
            .strip_prefix('"')
            .or_else(|| rest.strip_prefix(' ').and_then(|r| r.strip_prefix('"')))?;
        Some(rest.split('"').next()?.to_owned())
    };
    (quoted("inhibited by "), quoted("reason is "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibitor_message() {
        let (who, why) = parse_inhibitor(
            r#"Operation inhibited by "hinteros-system" (PID 812 "hinteros-system", user root), reason is "switching to a new system"."#,
        );
        assert_eq!(who.as_deref(), Some("hinteros-system"));
        assert_eq!(why.as_deref(), Some("switching to a new system"));
        assert_eq!(parse_inhibitor("nothing useful"), (None, None));
    }

    #[test]
    fn capabilities_map() {
        assert_eq!(capability("yes"), PowerCapability::Yes);
        assert_eq!(capability("challenge"), PowerCapability::Challenge);
        assert_eq!(capability("na"), PowerCapability::Na);
        assert_eq!(capability("weird"), PowerCapability::Na);
    }
}
