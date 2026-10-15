//! The App Store as the launcher sees it (`store.list`, `store_changed`).
//!
//! `store.list` merges the running generation's catalog index ([`hinteros_api::catalog`]) with
//! the installed apps from the system service (`settings.get`, relayed). Installing and
//! removing are the relayed `system.apps.install` / `system.apps.remove`; this module watches
//! their results and the job events to mark apps `installing`/`removing` and to send
//! `store_changed`. `more_after_update` comes from the last update check's `new_apps`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hinteros_api::catalog::{Entry, Index, IndexError, Status};
use hinteros_api::server::encode_result;
use hinteros_api::session::{
    AppKind, CatalogStatus, EventName, Service, StoreApp, StoreIcon, StoreList, StoreListResult,
    StoreState, StoreTerms,
};
use hinteros_api::system::{self, JobFinishedParams, JobKind, JobResult, UpdateCheckResult};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::debug;

use crate::icons::IconStore;
use crate::permissions::Permissions;
use crate::system::Relay;
use crate::{Audience, Broadcast};

#[derive(Default)]
struct State {
    /// Running apps jobs relayed through this daemon: job id → (install?, ids).
    jobs: HashMap<String, (bool, Vec<String>)>,
    /// Recently finished jobs (a fast job may finish before its relayed reply is seen).
    finished: std::collections::VecDeque<String>,
    more_after_update: bool,
}

struct Inner {
    index: PathBuf,
    icons: IconStore,
    permissions: Permissions,
    events: broadcast::Sender<Broadcast>,
    state: Mutex<State>,
    /// Look at the profile's unaccepted terms again (an `apps.accept_terms` went through).
    terms_wake: tokio::sync::Notify,
}

/// Unaccepted vendor terms of the profile's apps, by app id.
pub type NeedsTerms = std::collections::BTreeMap<String, hinteros_api::session::StoreTerms>;

/// Called once with the first known [`NeedsTerms`] (Steam's autostart waits for it).
pub type FirstTerms = Box<dyn FnOnce(&NeedsTerms) + Send>;

/// Handle to the Store; cheap to clone.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl Store {
    /// Start watching job events.
    pub fn spawn(
        index: PathBuf,
        icons: IconStore,
        permissions: Permissions,
        events: broadcast::Sender<Broadcast>,
    ) -> Self {
        let store = Self {
            inner: Arc::new(Inner {
                index,
                icons,
                permissions,
                events: events.clone(),
                state: Mutex::new(State::default()),
                terms_wake: tokio::sync::Notify::new(),
            }),
        };
        let rx = events.subscribe();
        tokio::spawn(watch(store.clone(), rx));
        store
    }

    fn changed(&self) {
        let _ = self
            .inner
            .events
            .send((EventName::StoreChanged, json!({}), Audience::All));
    }

    /// After a relayed call succeeded: remember apps jobs, and the update check's result.
    pub fn relayed(&self, session_method: &str, params: Option<&Value>, result: &Value) {
        let target = hinteros_api::session::relay_target(session_method);
        if target == Some(system::UpdateCheck::NAME) {
            if let Some(r) = result
                .get("result")
                .and_then(|r| serde_json::from_value::<UpdateCheckResult>(r.clone()).ok())
            {
                self.update_checked(&r);
            }
            return;
        }
        if target == Some(system::AppsAcceptTerms::NAME) {
            self.inner.terms_wake.notify_one();
            return;
        }
        let install = match target {
            Some(system::AppsInstall::NAME) => true,
            Some(system::AppsRemove::NAME) => false,
            _ => return,
        };
        let Some(job) = result.get("job_id").and_then(Value::as_str) else {
            return;
        };
        let ids: Vec<String> = params
            .and_then(|p| p.get("ids"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        {
            let mut s = self.inner.state.lock().unwrap();
            if !s.finished.iter().any(|j| j == job) {
                s.jobs.insert(job.to_owned(), (install, ids));
            }
        }
        self.changed();
    }

    fn update_checked(&self, r: &UpdateCheckResult) {
        let more = r
            .available
            .as_ref()
            .and_then(|a| a.new_apps.as_ref())
            .is_some_and(|n| !n.is_empty());
        let changed = {
            let mut s = self.inner.state.lock().unwrap();
            std::mem::replace(&mut s.more_after_update, more) != more
        };
        if changed {
            self.changed();
        }
    }

    fn job_finished(&self, f: &JobFinishedParams) {
        {
            let mut s = self.inner.state.lock().unwrap();
            s.jobs.remove(&f.job_id);
            s.finished.push_back(f.job_id.clone());
            if s.finished.len() > 16 {
                s.finished.pop_front();
            }
        }
        match (&f.kind, &f.result) {
            (JobKind::UpdateCheck, Some(JobResult::UpdateCheck(r))) => self.update_checked(r),
            (JobKind::UpdateCheck, _) => {}
            // Anything that switched (or tried to) may change the catalog or the installed apps.
            _ => self.changed(),
        }
    }

    /// The profile's apps whose vendor terms it has not accepted (`system.apps.list`'s
    /// `needs_terms` with the catalog's terms). Empty on a schema-1 box; `Err` while the system
    /// service cannot say.
    pub async fn needs_terms(&self, relay: &Relay) -> Result<NeedsTerms, ErrorObject> {
        let list = match relay.call(system::AppsList::NAME, None).await {
            Ok(l) => l,
            Err(e) if e.kind() == Some(ErrorCode::Unavailable) => return Ok(NeedsTerms::new()),
            Err(e) => return Err(e),
        };
        let ids = strings(&list["needs_terms"]);
        if ids.is_empty() {
            return Ok(NeedsTerms::new());
        }
        let index = Index::load(&self.inner.index).map_err(catalog_unavailable)?;
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                let t = index.get(&id)?.terms.as_ref()?;
                Some((
                    id,
                    hinteros_api::session::StoreTerms {
                        title: t.title.clone(),
                        version: t.version.clone(),
                        url: t.url.clone(),
                        retrieved: t.retrieved.clone(),
                        file: index.terms_file(t).to_string_lossy().into_owned(),
                    },
                ))
            })
            .collect())
    }

    /// Keep the shell's unaccepted terms current: at start (retrying until the system service
    /// answers), after any job, after `apps.accept_terms`, and every minute. `first` runs once,
    /// with the first answer (or with none after 15 s without one).
    pub fn spawn_terms(
        &self,
        relay: Relay,
        shell: tokio::sync::mpsc::UnboundedSender<crate::shell::Msg>,
        first: FirstTerms,
    ) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut first = Some(first);
            let mut rx = store.inner.events.subscribe();
            let mut last: Option<NeedsTerms> = None;
            let started = std::time::Instant::now();
            loop {
                match store.needs_terms(&relay).await {
                    Ok(map) => {
                        if last.as_ref() != Some(&map) {
                            let _ = shell.send(crate::shell::Msg::SetNeedsTerms(map.clone()));
                            if last.is_some() || !map.is_empty() {
                                let _ = store.inner.events.send((
                                    EventName::AppsChanged,
                                    json!({}),
                                    Audience::All,
                                ));
                            }
                            last = Some(map);
                        }
                        if let (Some(f), Some(map)) = (first.take(), &last) {
                            f(map);
                        }
                    }
                    Err(e) => {
                        debug!("unaccepted terms unknown: {}", e.message);
                        if started.elapsed() > std::time::Duration::from_secs(15)
                            && let Some(f) = first.take()
                        {
                            f(&NeedsTerms::new());
                        }
                    }
                }
                let wait = std::time::Duration::from_secs(if last.is_none() { 2 } else { 60 });
                let sleep = tokio::time::sleep(wait);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        () = store.inner.terms_wake.notified() => break,
                        () = &mut sleep => break,
                        ev = rx.recv() => match ev {
                            Ok((EventName::JobFinished, ..)) => break,
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(_)) => break,
                            Err(broadcast::error::RecvError::Closed) => return,
                        },
                    }
                }
            }
        });
    }

    /// `store.list`.
    pub async fn list(&self, relay: &Relay) -> Result<Value, ErrorObject> {
        let index = Index::load(&self.inner.index).map_err(catalog_unavailable)?;
        let settings = relay.call(system::SettingsGet::NAME, None).await?;
        // Schema 1: the box-wide apps; schema 2: the caller's own profile's.
        let installed: Vec<String> = match settings["settings"].get("apps") {
            Some(apps) => strings(apps),
            None => strings(&relay.call(system::AppsList::NAME, None).await?["ids"]),
        };
        let (installing, removing, more) = {
            let s = self.inner.state.lock().unwrap();
            let mut ins = BTreeSet::new();
            let mut rem = BTreeSet::new();
            for (install, ids) in s.jobs.values() {
                let set = if *install { &mut ins } else { &mut rem };
                set.extend(ids.iter().cloned());
            }
            (ins, rem, s.more_after_update)
        };
        let apps = merge(
            &index,
            &installed,
            &installing,
            &removing,
            &self.inner.icons,
            &self.inner.permissions,
        );
        encode_result::<StoreList>(StoreListResult {
            index_version: index.version,
            apps,
            more_after_update: more,
        })
    }
}

fn catalog_unavailable(e: IndexError) -> ErrorObject {
    let (reason, message) = match &e {
        IndexError::Missing(_) => ("missing", "no catalog index on this system".to_owned()),
        IndexError::Version { found, .. } => (
            "unsupported_version",
            format!("the catalog index has version {found}, which this daemon does not read"),
        ),
        _ => ("invalid", format!("the catalog index cannot be read: {e}")),
    };
    debug!("store.list: {e}");
    ErrorObject::with_data(
        ErrorCode::Unavailable,
        message,
        json!({ "service": Service::Catalog.name(), "reason": reason }),
    )
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The Store's view: available apps, and installed ones whatever their status, by name.
pub fn merge(
    index: &Index,
    installed: &[String],
    installing: &BTreeSet<String>,
    removing: &BTreeSet<String>,
    icons: &IconStore,
    permissions: &Permissions,
) -> Vec<StoreApp> {
    let mut out: Vec<StoreApp> = Vec::new();
    for e in index.apps.values() {
        // Services are box-wide and live in the Server section (M10), not in the Store.
        if e.entry_kind != hinteros_api::catalog::EntryKind::App {
            continue;
        }
        let is_installed = installed.contains(&e.id);
        if e.status != Status::Available && !is_installed {
            continue;
        }
        let state = if installing.contains(&e.id) && !is_installed {
            StoreState::Installing
        } else if removing.contains(&e.id) && is_installed {
            StoreState::Removing
        } else if !is_installed {
            StoreState::Available
        } else {
            match e.status {
                Status::Available => StoreState::Installed,
                Status::Removed => StoreState::Unavailable,
                Status::Retired | Status::Unknown => StoreState::Retired,
            }
        };
        let mut a = app(index, e, state, icons.get(&e.id));
        // Steam and apps that opt out of the sandbox have none.
        if e.sandbox && e.id != hinteros_api::session::STEAM_CLIENT_APP_ID {
            a.permissions = permissions.list(&e.id, &e.permissions);
        }
        out.push(a);
    }
    // Installed, but unknown to this catalog: shown so it can be removed.
    for id in installed.iter().filter(|i| index.get(i).is_none()) {
        out.push(StoreApp {
            status: CatalogStatus::Removed,
            state: if removing.contains(id) {
                StoreState::Removing
            } else {
                StoreState::Unavailable
            },
            ..empty(id)
        });
    }
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    out
}

fn empty(id: &str) -> StoreApp {
    StoreApp {
        id: id.to_owned(),
        name: id.to_owned(),
        summary: String::new(),
        description: String::new(),
        category: String::new(),
        homepage: String::new(),
        kind: AppKind::Native,
        size_mib: 0,
        icon: StoreIcon::default(),
        requires: vec![],
        depends: vec![],
        conflicts: vec![],
        installer: false,
        on_iso: false,
        installer_default: false,
        data_paths: vec![],
        terms: None,
        builds_on_device: false,
        free: false,
        redistributable: false,
        status: CatalogStatus::Removed,
        status_reason: None,
        state: StoreState::Unavailable,
        permissions: Vec::new(),
    }
}

fn app(index: &Index, e: &Entry, state: StoreState, icon: Option<PathBuf>) -> StoreApp {
    StoreApp {
        id: e.id.clone(),
        name: e.name.chars().take(40).collect(),
        summary: e.summary.chars().take(80).collect(),
        description: e.description.chars().take(1000).collect(),
        category: e.category.chars().take(32).collect(),
        homepage: e.homepage.clone(),
        kind: if e.kind == "web" {
            AppKind::Web
        } else {
            AppKind::Native
        },
        size_mib: e.size_mib,
        icon: StoreIcon {
            desktop_id: e.icon.desktop_id.clone(),
            site_url: e.icon.site_url.clone(),
            icon_url: e.icon.icon_url.clone(),
            firefox_profile: e.icon.firefox_profile,
            path: icon.map(|p| p.to_string_lossy().into_owned()),
        },
        requires: e.requires.clone(),
        depends: e.depends.clone(),
        conflicts: e.conflicts.clone(),
        installer: e.installer,
        on_iso: e.on_iso,
        installer_default: e.installer_default,
        data_paths: e.data_paths.clone(),
        terms: e.terms.as_ref().map(|t| StoreTerms {
            title: t.title.chars().take(80).collect(),
            version: t.version.clone(),
            url: t.url.clone(),
            retrieved: t.retrieved.clone(),
            file: index.terms_file(t).display().to_string(),
        }),
        builds_on_device: e.builds_on_device,
        free: e.free,
        redistributable: e.redistributable,
        status: match e.status {
            Status::Available => CatalogStatus::Available,
            Status::Removed => CatalogStatus::Removed,
            Status::Retired | Status::Unknown => CatalogStatus::Retired,
        },
        status_reason: e
            .status_reason
            .as_ref()
            .map(|r| r.chars().take(200).collect()),
        state,
        permissions: Vec::new(),
    }
}

async fn watch(store: Store, mut rx: broadcast::Receiver<Broadcast>) {
    loop {
        match rx.recv().await {
            Ok((EventName::JobFinished, params, _)) => {
                if let Ok(f) = serde_json::from_value::<JobFinishedParams>(params) {
                    store.job_finished(&f);
                }
            }
            Ok((EventName::JobStarted, params, _)) => {
                if matches!(
                    params.get("kind").and_then(Value::as_str),
                    Some("apps_install" | "apps_remove")
                ) {
                    store.changed();
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_index_and_install_state() {
        let index = Index::parse(
            br#"{"indexVersion":2,"apps":[
              {"id":"steam","name":"Steam","status":"available","kind":"native","sizeMiB":1000,"permissions":{"gamepad":"optional"},
               "terms":{"title":"SSA","version":"2026-09-10","url":"https://x","retrieved":"2026-10-06","file":"terms/steam-2026-09-10.txt"}},
              {"id":"youtube","name":"YouTube","status":"available","kind":"web",
               "permissions":{"mic":"optional","gamepad":"required"}},
              {"id":"moonlight","name":"moonlight","status":"available"},
              {"id":"retired-in","name":"R1","status":"retired","statusReason":"old"},
              {"id":"retired-out","name":"R2","status":"retired","statusReason":"old"},
              {"id":"gone","name":"Gone","status":"removed","statusReason":"bye"}]}"#,
            "/run/current-system/sw/share/hinteros".into(),
        )
        .unwrap();
        let installed: Vec<String> = ["youtube", "retired-in", "gone", "mystery", "moonlight"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let installing = BTreeSet::from(["steam".to_string()]);
        let removing = BTreeSet::from(["moonlight".to_string()]);
        let icons = IconStore::default();
        icons.set("youtube", Some("/cache/youtube.png".into()));
        let perms = Permissions::new(None, vec!["gamepad", "mic", "downloads"]);
        perms
            .set(
                "youtube",
                Some(&index.get("youtube").unwrap().permissions),
                "mic",
                false,
            )
            .unwrap();
        let apps = merge(&index, &installed, &installing, &removing, &icons, &perms);
        let view: Vec<(&str, StoreState)> = apps.iter().map(|a| (a.id.as_str(), a.state)).collect();
        assert_eq!(
            view,
            [
                ("gone", StoreState::Unavailable),
                ("moonlight", StoreState::Removing),
                ("mystery", StoreState::Unavailable),
                ("retired-in", StoreState::Retired),
                ("steam", StoreState::Installing),
                ("youtube", StoreState::Installed),
            ]
        );
        let steam = apps.iter().find(|a| a.id == "steam").unwrap();
        assert_eq!(
            steam.terms.as_ref().unwrap().file,
            "/run/current-system/sw/share/hinteros/terms/steam-2026-09-10.txt"
        );
        let icon_of = |id: &str| apps.iter().find(|a| a.id == id).unwrap().icon.path.clone();
        assert_eq!(icon_of("youtube").as_deref(), Some("/cache/youtube.png"));
        assert_eq!(icon_of("steam"), None);
        assert_eq!(
            apps.iter().find(|a| a.id == "youtube").unwrap().kind,
            AppKind::Web
        );
        let perms_of = |id: &str| {
            serde_json::to_value(&apps.iter().find(|a| a.id == id).unwrap().permissions).unwrap()
        };
        assert_eq!(
            perms_of("youtube"),
            json!([{"name":"gamepad","required":true,"granted":true},{"name":"mic","required":false,"granted":false}])
        );
        assert_eq!(perms_of("steam"), json!([]), "never sandboxed");
        assert_eq!(perms_of("mystery"), json!([]));
        // Every app is valid against the schema.
        let schema = hinteros_api::conformance::ApiSchema::session();
        for a in &apps {
            schema
                .check("/$defs/StoreApp", &serde_json::to_value(a).unwrap())
                .unwrap();
        }
    }
}
