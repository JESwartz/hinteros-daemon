//! The local network filter ([`crate::lanfilter`]): `lan_filter.set` and
//! `lan_filter.clear`, the session daemon's own calls (not relayed).
//!
//! They only restrict the caller's own processes, so there is no seat or polkit check: a cgroup
//! must be the caller's ([`crate::lanfilter::check_cgroup`]). They are rate limited (a bucket of
//! their own) and audited like mutating methods. The service writes
//! `<run-dir>/lan-filter.json` and restarts `hinteros-lan-filter.service`, which loads the
//! table, and waits for it.

use std::time::{Duration, Instant};

use hinteros_api::server::method_not_found;
use hinteros_api::system::{LanFilterClear, LanFilterSet};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{Service, decode, summarize};
use crate::audit::Decision;
use crate::authz;
use crate::lanfilter::{self, APPLY_UNIT, Filters};
use crate::peer::Peer;

/// How long a call waits for the unit.
const APPLY_TIMEOUT: Duration = Duration::from_secs(30);

fn not_applied(detail: impl std::fmt::Display) -> ErrorObject {
    warn!("local network filter not changed: {detail}");
    ErrorObject::new(
        ErrorCode::InternalError,
        "The local network filter did not change.",
    )
}

impl Service {
    /// `lan_filter.set` / `lan_filter.clear`: params, rate limit, the change; audited.
    pub(super) async fn lan_filter(
        &self,
        peer: &Peer,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, ErrorObject> {
        let summary = summarize(&params);
        let exe = peer.exe_display();
        let audit = |decision: Decision| {
            self.audit.request(
                peer.uid, peer.pid, &exe, method, None, decision, None, &summary,
            );
        };
        let cgroup = match method {
            LanFilterSet::NAME => decode::<LanFilterSet>(params).map(|p| Some(p.cgroup)),
            LanFilterClear::NAME => decode::<LanFilterClear>(params).map(|_| None),
            // Never a clear by default: a name this version does not know changes nothing.
            _ => Err(method_not_found(method)),
        };
        // The cgroup is checked before the rate limit (a cheap stat), so a bad one is always
        // invalid_params.
        let root = &self.args.cgroup_root;
        let rel = cgroup.and_then(|c| {
            c.map(|c| lanfilter::check_cgroup(root, peer.uid, &c))
                .transpose()
                .map_err(|e| ErrorObject::invalid_field("cgroup", e.reason(), e.message()))
        });
        let rel = match rel {
            Ok(r) => r,
            Err(e) => {
                audit(Decision::Invalid);
                return Err(e);
            }
        };
        if let Err(retry) = self.lan_rate.take(peer.uid, Instant::now()) {
            audit(Decision::RateLimited);
            return Err(authz::rate_limited(retry));
        }
        match self.lan_filter_change(peer.uid, rel).await {
            Ok(()) => {
                audit(Decision::Allowed);
                Ok(json!({}))
            }
            Err(e) => {
                audit(Decision::of_error(&e));
                info!(method, error = %e.message, "request refused");
                Err(e)
            }
        }
    }

    /// Set (`Some`, checked, relative to the cgroup root) or clear (`None`) the cgroup of `uid`,
    /// then load the table.
    async fn lan_filter_change(&self, uid: u32, rel: Option<String>) -> Result<(), ErrorObject> {
        if !self.args.lan_filter {
            return Err(ErrorObject::new(
                ErrorCode::InternalError,
                "this box has no local network filter",
            ));
        }
        let root = &self.args.cgroup_root;
        // One change at a time (one state file, one unit).
        let _one = self.lan_apply.lock().await;
        let path = self.paths.run_dir.join(lanfilter::STATE_FILE);
        // Unreadable: refuse rather than drop the other users' cgroups.
        let mut filters = Filters::load(&path).map_err(not_applied)?;
        let had = match &rel {
            Some(rel) => filters.cgroups.insert(uid, rel.clone()).is_some(),
            None => {
                if filters.cgroups.remove(&uid).is_none() {
                    return Ok(());
                }
                true
            }
        };
        for (gone, path) in filters.prune(root) {
            info!(uid = gone, cgroup = %path, "local network filter: cgroup gone");
        }
        filters.save(&path).map_err(not_applied)?;
        // A run the path unit or NetworkManager's hook started is ended by the restart, which can
        // fail that job: once more, then it is a failure.
        if let Err(e) = self
            .run_oneshot(APPLY_UNIT, "RestartUnit", APPLY_TIMEOUT)
            .await
        {
            info!("local network filter: {e}; once more");
            self.run_oneshot(APPLY_UNIT, "RestartUnit", APPLY_TIMEOUT)
                .await
                .map_err(not_applied)?;
        }
        match rel {
            Some(rel) => info!(uid, cgroup = %rel, replaced = had, "local network filter set"),
            None => info!(uid, "local network filter cleared"),
        }
        Ok(())
    }
}
