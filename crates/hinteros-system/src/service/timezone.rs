//! Time zone: `time_zone.get`, `time_zone.list` and
//! `time_zone.set`, which changes `state.json` and `/etc/localtime` at once, without a job.
//!
//! The service has no write access to `/etc`: `time_zone.set` leaves the zone in
//! `<run-dir>/time-zone.request`, starts `hinteros-time-zone-apply.service` (a oneshot running
//! `hinteros-system time-zone-apply`, see [`crate::timezone::apply_main`]: it writes
//! `state.json` and asks systemd-timedated for `/etc/localtime`) and waits for its systemd job
//! to end.
//!
//! The admin PIN guards `time_zone.set` like every box-wide setting, except in first-boot mode
//! (no admin profile yet, the rule of `profiles.setup_admin`): the first-boot wizard sets the
//! zone before there is an admin.

use hinteros_api::system::{
    self, EventName, TimeZoneInfo, TimeZoneListResult, TimeZoneSetParams, is_zone_name,
};
use std::time::Duration;

use futures_util::StreamExt;
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::Value;
use tracing::{info, warn};
use zbus::zvariant::OwnedObjectPath;

use super::{Service, internal};
use crate::job;
use crate::peer::Peer;
use crate::timezone::{self, APPLY_UNIT, ZoneError};

/// How long `time_zone.set` waits for the helper unit.
const APPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Formats only (before the rate limit and polkit).
pub fn validate(p: &TimeZoneSetParams) -> Result<(), ErrorObject> {
    if !is_zone_name(&p.zone) {
        return Err(ErrorObject::invalid_field(
            "zone",
            "bad_format",
            "this is not a time zone name",
        ));
    }
    super::check_admin_pin(p.admin_pin.as_deref())
}

fn zone_error(e: ZoneError) -> ErrorObject {
    match e {
        ZoneError::BadFormat(_) => {
            ErrorObject::invalid_field("zone", "bad_format", "this is not a time zone name")
        }
        ZoneError::Unknown(_) => ErrorObject::invalid_field("zone", "unknown", "unknown time zone"),
        ZoneError::Io(..) => internal(e),
    }
}

fn not_applied(detail: impl std::fmt::Display) -> ErrorObject {
    warn!("time zone not applied: {detail}");
    ErrorObject::new(ErrorCode::InternalError, "The time zone did not change.")
}

impl Service {
    pub(super) fn time_zone_get(&self) -> Result<TimeZoneInfo, ErrorObject> {
        let (state, _) = self.load_state()?;
        Ok(timezone::effective(
            &self.paths.zoneinfo_dir,
            &state,
            job::now(),
        ))
    }

    pub(super) async fn time_zone_list(&self) -> Result<TimeZoneListResult, ErrorObject> {
        let dir = self.paths.zoneinfo_dir.clone();
        let zones = tokio::task::spawn_blocking(move || timezone::list(&dir, job::now()))
            .await
            .map_err(internal)?;
        Ok(TimeZoneListResult { zones })
    }

    /// Send `time_zone_changed` when the zone that applies is not the last one announced.
    pub(super) fn time_zone_announce(&self, now: &TimeZoneInfo) {
        let mut last = self.last_zone.lock().unwrap();
        if last.as_deref() == Some(now.zone.as_str()) {
            return;
        }
        *last = Some(now.zone.clone());
        if let Ok(params) = serde_json::to_value(now) {
            let _ = self.events.send((EventName::TimeZoneChanged, params));
        }
    }

    /// After a job: a rollback may have restored a `state.json` with another zone.
    pub(super) fn time_zone_recheck(&self) {
        if let Ok(now) = self.time_zone_get() {
            self.time_zone_announce(&now);
        }
    }

    pub(super) async fn time_zone_set(
        &self,
        peer: &Peer,
        p: TimeZoneSetParams,
    ) -> Result<Value, ErrorObject> {
        let (state, _) = self.load_state()?;
        let first_boot = self.profiles_file().first_boot(&state);
        if !first_boot {
            self.check_box_admin(peer.uid, system::TimeZoneSet::NAME, p.admin_pin.as_deref())
                .await?;
        }
        // The zone must be one the system's tzdata has (UTC always); the helper checks again.
        timezone::info(&self.paths.zoneinfo_dir, &p.zone, job::now()).map_err(zone_error)?;
        let before = self.time_zone_get()?.zone;
        {
            // One request at a time.
            let _one = self.tz_apply.lock().await;
            timezone::write_request(&self.paths, &p.zone).map_err(not_applied)?;
            // The unit binds /etc/hinteros when it starts: a job that swaps that directory
            // before the helper takes the state.json lock makes the helper fail (it changes
            // nothing). Once more, then it is a failure.
            if let Err(e) = self
                .run_oneshot(APPLY_UNIT, "StartUnit", APPLY_TIMEOUT)
                .await
            {
                info!("time zone: {e}; once more");
                self.run_oneshot(APPLY_UNIT, "StartUnit", APPLY_TIMEOUT)
                    .await
                    .map_err(not_applied)?;
            }
        }
        let new = self.time_zone_get()?;
        if new.zone != p.zone {
            return Err(not_applied(format!(
                "the helper ended, but {} applies, not {}",
                new.zone, p.zone
            )));
        }
        if before != new.zone {
            info!(uid = peer.uid, from = %before, to = %new.zone, first_boot, "time zone set");
        }
        self.time_zone_announce(&new);
        serde_json::to_value(new).map_err(internal)
    }

    /// Start (`method`: `StartUnit`, `RestartUnit`) a oneshot helper unit and wait up to
    /// `limit` for its job to end (`JobRemoved`, result `done`). Also `lan_filter.*`.
    pub(super) async fn run_oneshot(
        &self,
        unit: &str,
        method: &str,
        limit: Duration,
    ) -> Result<(), String> {
        let conn = self.bus.get().await.map_err(|e| format!("no bus: {e}"))?;
        let manager: zbus::Proxy<'_> = zbus::proxy::Builder::new(&conn)
            .destination("org.freedesktop.systemd1")
            .and_then(|b| b.path("/org/freedesktop/systemd1"))
            .and_then(|b| b.interface("org.freedesktop.systemd1.Manager"))
            .map_err(|e| e.to_string())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| format!("systemd: {e}"))?;
        // systemd sends job signals only to subscribed clients.
        if let Err(e) = manager.call_method("Subscribe", &()).await {
            warn!("systemd Subscribe: {e}");
        }
        let mut removed = manager
            .receive_signal("JobRemoved")
            .await
            .map_err(|e| format!("systemd JobRemoved: {e}"))?;
        let job: OwnedObjectPath = manager
            .call(method, &(unit, "replace"))
            .await
            .map_err(|e| format!("starting {unit}: {e}"))?;
        let wait = async {
            while let Some(msg) = removed.next().await {
                let Ok((_, path, _, result)) = msg
                    .body()
                    .deserialize::<(u32, OwnedObjectPath, String, String)>()
                else {
                    continue;
                };
                if path == job {
                    return Some(result);
                }
            }
            None
        };
        match tokio::time::timeout(limit, wait).await {
            Err(_) => Err(format!("{unit} did not end within {limit:?}")),
            Ok(None) => Err("the systemd signal stream ended".into()),
            Ok(Some(r)) if r == "done" => Ok(()),
            Ok(Some(r)) => Err(format!("{unit} ended with {r}")),
        }
    }
}
