//! Time zone (system API 0.8, time-zone.md): `time_zone.get`, `time_zone.list` and
//! `time_zone.set`, which changes `state.json` and `/etc/localtime` at once, without a job.
//!
//! The admin PIN guards `time_zone.set` like every box-wide setting, except in first-boot mode
//! (no admin profile yet, the rule of `profiles.setup_admin`): the first-boot wizard sets the
//! zone before there is an admin.

use hinteros_api::system::{
    self, EventName, TimeZoneInfo, TimeZoneListResult, TimeZoneSetParams, is_zone_name,
};
use hinteros_api::{ErrorObject, Method};
use serde_json::Value;
use tracing::info;

use super::{Service, internal};
use crate::config::Paths;
use crate::job;
use crate::peer::Peer;
use crate::state::{self, State};
use crate::timezone::{self, ZoneError};

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

/// Under the `state.json` lock: write `settings.timeZone` (when it changes) and point the
/// localtime link at the zone. Returns the zone that applied before.
fn write_zone(paths: &Paths, zone: &str) -> std::io::Result<String> {
    let _lock = state::lock(&paths.state_lock())?;
    let path = paths.state_json();
    let (mut st, _) = State::load(&path).map_err(std::io::Error::other)?;
    let before = timezone::effective(&paths.zoneinfo_dir, &st, job::now()).zone;
    if st.time_zone() != zone {
        st.settings.time_zone = Some(zone.to_owned());
        crate::fsutil::write_atomic(&path, &st.to_canonical_json(), 0o644)?;
    }
    timezone::set_localtime(&paths.localtime, &paths.zoneinfo_dir, zone)?;
    Ok(before)
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
        let first_boot = state.is_schema2() && self.profiles_file().first_boot(&state);
        if !first_boot {
            self.check_box_admin(peer.uid, system::TimeZoneSet::NAME, p.admin_pin.as_deref())
                .await?;
        }
        // The zone must be one the system's tzdata has (UTC always).
        let new =
            timezone::info(&self.paths.zoneinfo_dir, &p.zone, job::now()).map_err(zone_error)?;
        let paths = self.paths.clone();
        let zone = p.zone.clone();
        let before = tokio::task::spawn_blocking(move || write_zone(&paths, &zone))
            .await
            .map_err(internal)?
            .map_err(|e| internal(format!("cannot set the time zone: {e}")))?;
        if before != new.zone {
            info!(uid = peer.uid, from = %before, to = %new.zone, first_boot, "time zone set");
        }
        self.time_zone_announce(&new);
        serde_json::to_value(new).map_err(internal)
    }
}
