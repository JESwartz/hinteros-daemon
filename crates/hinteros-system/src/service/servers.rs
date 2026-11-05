//! The Server section (server.md): `services.*`, `media.folders`,
//! `jellyfin.*`, `plex.claim`, `tailscale.*`, `cloudflare.*`. Every write needs the admin PIN
//! (polkit action `org.hinteros.system.services`). Reads talk to the services on loopback or their
//! sockets with a 1 s timeout, on the blocking pool (the service's runtime is single-threaded).

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::Duration;

use hinteros_api::catalog::{EntryKind, Index, Status};
use hinteros_api::system::{
    CatalogApp, CloudflareDetail, CloudflareSetTokenParams, JellyfinAccount,
    JellyfinClientTokenResult, JellyfinDetail, JellyfinLibrariesAddParams,
    JellyfinLibrariesRemoveParams, JellyfinProfileAccount, JellyfinUserRef,
    JellyfinUsersRevealResult, JobKind, JobSubject, MediaFoldersParams, MediaFoldersResult,
    PinParams, PlexClaimParams, PlexDetail, ServiceDetail, ServiceIcon, ServiceInfo, ServiceTerms,
    ServicesDisableParams, ServicesEnableParams, ServicesListResult, Setup, SubjectLibrary,
    UnitState,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::{Value, json};
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::{Service, internal, job_ref, state_conflict};
use crate::config::ServerPaths;
use crate::job::{
    self, Action, Apply, CloudflareJob, DeleteTarget, JellyfinJob, JellyfinOp, PlexJob,
    ServicesJob, TailscaleJob,
};
use crate::peer::Peer;
use crate::servers::{self, cloudflare, jellyfin, media, netinfo, plex, tailscale};

/// How long a read waits for a service.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

const SERVICES_ACTION: &str = "org.hinteros.system.services";

/// A decoded Server section request.
pub enum ServersRequest {
    Enable(ServicesEnableParams),
    Disable(ServicesDisableParams),
    LibraryAdd(JellyfinLibrariesAddParams),
    LibraryRemove(JellyfinLibrariesRemoveParams),
    Reveal(PinParams),
    PlexClaim(PlexClaimParams),
    TailscaleLogin(PinParams),
    TailscaleLogout(PinParams),
    SetToken(CloudflareSetTokenParams),
    ClearToken(PinParams),
}

fn pin_field(pin: &str) -> Result<(), ErrorObject> {
    if crate::pins::is_pin(pin) {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            "pin",
            "bad_format",
            "a PIN is 4 to 8 digits",
        ))
    }
}

/// `unavailable {service, reason}`: the service is off (`off`) or not set up yet (`not_ready`).
fn unavailable(service: &str, reason: &str) -> ErrorObject {
    let message = match reason {
        "off" => "the service is off",
        _ => "the service is not ready",
    };
    ErrorObject::with_data(
        ErrorCode::Unavailable,
        message,
        json!({ "service": service, "reason": reason }),
    )
}

impl ServersRequest {
    /// The request of a Server section method (`params` taken), `None` (`params` left) for
    /// any other method.
    pub fn decode(method: &str, params: &mut Option<Value>) -> Option<Result<Self, ErrorObject>> {
        use hinteros_api::system as s;
        let mut params = || params.take();
        let r = match method {
            s::ServicesEnable::NAME => {
                super::decode::<s::ServicesEnable>(params()).map(Self::Enable)
            }
            s::ServicesDisable::NAME => {
                super::decode::<s::ServicesDisable>(params()).map(Self::Disable)
            }
            s::JellyfinLibrariesAdd::NAME => {
                super::decode::<s::JellyfinLibrariesAdd>(params()).map(Self::LibraryAdd)
            }
            s::JellyfinLibrariesRemove::NAME => {
                super::decode::<s::JellyfinLibrariesRemove>(params()).map(Self::LibraryRemove)
            }
            s::JellyfinUsersReveal::NAME => {
                super::decode::<s::JellyfinUsersReveal>(params()).map(Self::Reveal)
            }
            s::PlexClaim::NAME => super::decode::<s::PlexClaim>(params()).map(Self::PlexClaim),
            s::TailscaleLogin::NAME => {
                super::decode::<s::TailscaleLogin>(params()).map(Self::TailscaleLogin)
            }
            s::TailscaleLogout::NAME => {
                super::decode::<s::TailscaleLogout>(params()).map(Self::TailscaleLogout)
            }
            s::CloudflareSetToken::NAME => {
                super::decode::<s::CloudflareSetToken>(params()).map(Self::SetToken)
            }
            s::CloudflareClearToken::NAME => {
                super::decode::<s::CloudflareClearToken>(params()).map(Self::ClearToken)
            }
            _ => return None,
        };
        Some(r.and_then(|r| r.validate().map(|()| r)))
    }

    pub fn method(&self) -> &'static str {
        use hinteros_api::system as s;
        match self {
            Self::Enable(_) => s::ServicesEnable::NAME,
            Self::Disable(_) => s::ServicesDisable::NAME,
            Self::LibraryAdd(_) => s::JellyfinLibrariesAdd::NAME,
            Self::LibraryRemove(_) => s::JellyfinLibrariesRemove::NAME,
            Self::Reveal(_) => s::JellyfinUsersReveal::NAME,
            Self::PlexClaim(_) => s::PlexClaim::NAME,
            Self::TailscaleLogin(_) => s::TailscaleLogin::NAME,
            Self::TailscaleLogout(_) => s::TailscaleLogout::NAME,
            Self::SetToken(_) => s::CloudflareSetToken::NAME,
            Self::ClearToken(_) => s::CloudflareClearToken::NAME,
        }
    }

    fn pin(&self) -> &str {
        match self {
            Self::Enable(p) => &p.pin,
            Self::Disable(p) => &p.pin,
            Self::LibraryAdd(p) => &p.pin,
            Self::LibraryRemove(p) => &p.pin,
            Self::PlexClaim(p) => &p.pin,
            Self::SetToken(p) => &p.pin,
            Self::Reveal(p)
            | Self::TailscaleLogin(p)
            | Self::TailscaleLogout(p)
            | Self::ClearToken(p) => &p.pin,
        }
    }

    /// The formats (before the rate limit and polkit).
    pub fn validate(&self) -> Result<(), ErrorObject> {
        pin_field(self.pin())?;
        match self {
            Self::Enable(p) => {
                check_service_ids(&p.ids)?;
                super::check_revision(&p.revision)?;
                crate::store::check_accepted_terms(&p.accepted_terms)
            }
            Self::Disable(p) => {
                check_service_ids(&p.ids)?;
                super::check_revision(&p.revision)
            }
            Self::LibraryAdd(p) => {
                jellyfin::check_library_name(&p.name).map_err(|reason| {
                    ErrorObject::invalid_field(
                        "name",
                        reason,
                        "a name is 1 to 64 characters, without control characters or /\\:*?\"<>|",
                    )
                })?;
                if !media::is_folder_id(&p.folder) {
                    return Err(ErrorObject::invalid_field(
                        "folder",
                        "bad_format",
                        "folder must be a media.folders id",
                    ));
                }
                Ok(())
            }
            Self::LibraryRemove(p) => {
                if jellyfin::is_id(&p.library_id) {
                    Ok(())
                } else {
                    Err(ErrorObject::invalid_field(
                        "library_id",
                        "bad_format",
                        "library_id must be 32 hex digits",
                    ))
                }
            }
            Self::PlexClaim(p) => {
                if plex::is_claim_code(&p.code) {
                    Ok(())
                } else {
                    Err(ErrorObject::invalid_field(
                        "code",
                        "bad_format",
                        "a claim code is claim- and 20 letters, digits, _ or -",
                    ))
                }
            }
            Self::SetToken(p) => cloudflare::check_token(&p.token).map_err(|reason| {
                ErrorObject::invalid_field("token", reason, "not a tunnel token")
            }),
            Self::Reveal(_)
            | Self::TailscaleLogin(_)
            | Self::TailscaleLogout(_)
            | Self::ClearToken(_) => Ok(()),
        }
    }
}

/// 1 to 8 catalog ids, each once.
fn check_service_ids(ids: &[String]) -> Result<(), ErrorObject> {
    let unique: BTreeSet<&String> = ids.iter().collect();
    if ids.is_empty()
        || ids.len() > hinteros_api::system::MAX_SERVICE_IDS
        || unique.len() != ids.len()
    {
        return Err(ErrorObject::invalid_field(
            "ids",
            "out_of_range",
            "1 to 8 different ids",
        ));
    }
    if ids.iter().any(|i| !hinteros_api::catalog::is_app_id(i)) {
        return Err(ErrorObject::invalid_field(
            "ids",
            "bad_format",
            "ids must match ^[a-z0-9][a-z0-9-]{0,31}$",
        ));
    }
    Ok(())
}

/// What one enabled service's detail needs (moved to the blocking pool).
struct DetailCtx {
    server: ServerPaths,
    services_dir: std::path::PathBuf,
    jellyfin: jellyfin::Files,
    users: crate::users::Users,
    local_ip: Option<Ipv4Addr>,
    port: Option<u16>,
}

impl DetailCtx {
    fn url(&self, default_port: u16) -> Option<String> {
        self.local_ip
            .map(|ip| format!("http://{ip}:{}", self.port.unwrap_or(default_port)))
    }
}

fn jellyfin_detail(c: &DetailCtx) -> JellyfinDetail {
    let files = &c.jellyfin;
    let live = jellyfin::Client::new(files, c.server.jellyfin, &c.users, READ_TIMEOUT)
        .and_then(|cl| cl.virtual_folders());
    // Live, else what the last job or read saw (Jellyfin restarting, an update).
    let folders = match live {
        Ok(f) => {
            jellyfin::write_cache(files, &f);
            f
        }
        Err(_) => crate::fsutil::read_cache(&files.libraries_cache()).unwrap_or_default(),
    };
    let users = jellyfin::UsersFile::load(&files.users_json()).unwrap_or_default();
    JellyfinDetail {
        setup_done: files.setup_done(),
        url: c.url(8096),
        libraries: jellyfin::libraries(&folders, &c.server.media_dir),
        users: users
            .users
            .iter()
            .filter(|(_, r)| !r.is_pending())
            .map(|(p, r)| JellyfinUserRef {
                profile_id: p.clone(),
                jellyfin_user: r.name.clone(),
            })
            .collect(),
    }
}

fn plex_detail(c: &DetailCtx) -> PlexDetail {
    let prefs = plex::read(&c.server.plex_preferences).unwrap_or_default();
    PlexDetail {
        claimed: prefs.claimed,
        server_name: prefs.friendly_name,
        url: c.url(32400),
    }
}

fn cloudflare_detail(c: &DetailCtx) -> CloudflareDetail {
    let token_set = c
        .services_dir
        .join(servers::CLOUDFLARE)
        .join("token")
        .is_file();
    CloudflareDetail {
        token_set,
        connected: token_set && cloudflare::connected(c.server.cloudflared_metrics, READ_TIMEOUT),
        local_ip: c.local_ip.map(|i| i.to_string()),
    }
}

fn detail(setup: Setup, c: &DetailCtx) -> ServiceDetail {
    match setup {
        Setup::JellyfinServer => ServiceDetail::Jellyfin(jellyfin_detail(c)),
        Setup::PlexMediaServer => ServiceDetail::Plex(plex_detail(c)),
        Setup::Tailscale => ServiceDetail::Tailscale(tailscale::detail(
            tailscale::status(&c.server.tailscale_socket, READ_TIMEOUT)
                .ok()
                .as_ref(),
        )),
        Setup::CloudflareTunnel => ServiceDetail::Cloudflare(cloudflare_detail(c)),
    }
}

impl Service {
    fn server_paths(&self) -> ServerPaths {
        self.paths.server.clone()
    }

    fn service_enabled(&self, id: &str) -> bool {
        self.load_state()
            .is_ok_and(|(s, _)| s.services.iter().any(|i| i == id))
    }

    /// A unit's `ActiveState` (`inactive` when not loaded; `None` without systemd).
    async fn unit_state(&self, unit: &str) -> Option<UnitState> {
        let conn = self.bus.get().await.ok()?;
        match crate::freeze::unit_property(&conn, unit, "ActiveState").await {
            Ok(Some(s)) => Some(UnitState::from_systemd(&s)),
            Ok(None) => Some(UnitState::Inactive),
            Err(e) => {
                warn!(unit, "no unit state: {e}");
                None
            }
        }
    }

    pub(super) async fn services_list(&self) -> Result<ServicesListResult, ErrorObject> {
        let (state, revision) = self.load_state()?;
        let enabled = state.services.clone();
        let index: Option<Index> = self.catalog().ok();
        let mut ids: Vec<String> = index
            .iter()
            .flat_map(|i| i.entries.values())
            .filter(|e| e.kind == EntryKind::Service && e.status == Status::Available)
            .map(|e| e.id.clone())
            .collect();
        for id in &enabled {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        let local_ip = netinfo::local_ipv4(&self.paths.proc_dir);
        let mut services = Vec::new();
        let mut pending = Vec::new();
        for id in ids {
            let entry = index.as_ref().and_then(|i| i.get(&id));
            let on = enabled.contains(&id);
            let setup = entry.and_then(|e| e.setup_kind());
            let status = match (on, servers::unit_for(&id)) {
                (true, Some(unit)) => self.unit_state(unit).await,
                _ => None,
            };
            if on && let Some(setup) = setup {
                let ctx = DetailCtx {
                    server: self.server_paths(),
                    services_dir: self.paths.services_dir(),
                    jellyfin: jellyfin::Files::new(&self.paths),
                    users: self.paths.users.clone(),
                    local_ip,
                    port: entry.and_then(|e| e.tcp_port()),
                };
                pending.push((
                    services.len(),
                    tokio::task::spawn_blocking(move || detail(setup, &ctx)),
                ));
            }
            services.push(ServiceInfo {
                name: entry.map_or_else(|| id.clone(), |e| e.name.clone()),
                available: entry
                    .is_some_and(|e| e.status == Status::Available && e.kind == EntryKind::Service),
                enabled: on,
                status,
                setup,
                terms: entry.and_then(|e| e.terms.as_ref()).map(|t| ServiceTerms {
                    title: t.title.clone(),
                    version: t.version.clone(),
                    url: t.url.clone(),
                    file: index
                        .as_ref()
                        .map(|i| i.terms_file(t).display().to_string())
                        .unwrap_or_default(),
                }),
                detail: None,
                icon: entry.map(|e| ServiceIcon {
                    desktop_id: e.icon.desktop_id.clone(),
                    site_url: e.icon.site_url.clone(),
                    icon_url: e.icon.icon_url.clone(),
                    sha256: e.icon.sha256.clone(),
                    firefox_profile: e.icon.firefox_profile,
                    path: None,
                }),
                has_data: entry.is_some_and(|e| {
                    servers::delete_targets(e, &self.paths.server, &self.paths.state_dir)
                        .iter()
                        .any(|(base, rel)| servers::delete_allowed(base, rel, &self.paths.server))
                }),
                id,
            });
        }
        for (i, task) in pending {
            services[i].detail = task.await.ok();
        }
        Ok(ServicesListResult { revision, services })
    }

    /// `media.folders`: the folder scan runs on the blocking pool (a big or slow disk must not
    /// stall the service's one thread).
    pub(super) async fn media_folders(
        &self,
        p: MediaFoldersParams,
    ) -> Result<MediaFoldersResult, ErrorObject> {
        if let Some(id) = &p.folder_id
            && !media::is_folder_id(id)
        {
            return Err(ErrorObject::invalid_field(
                "folder_id",
                "bad_format",
                "not a folder id",
            ));
        }
        let dir = self.paths.server.media_dir.clone();
        let id = p.folder_id.clone();
        let listed = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || media::list(&dir, id.as_deref()))
                .await
                .map_err(internal)?
        };
        let folders = match listed {
            Ok(f) => f,
            // No media folder yet: nothing to pick.
            Err(media::Resolve::Missing) if p.folder_id.is_none() => Vec::new(),
            Err(media::Resolve::Missing | media::Resolve::NotAFolder) => {
                return Err(ErrorObject::new(ErrorCode::NotFound, "no such folder"));
            }
            Err(media::Resolve::BadId) => {
                return Err(ErrorObject::invalid_field(
                    "folder_id",
                    "bad_format",
                    "not a folder id",
                ));
            }
            Err(e) => return Err(internal(format!("{}: {e}", dir.display()))),
        };
        Ok(MediaFoldersResult {
            folder_id: p.folder_id,
            folders,
        })
    }

    /// `jellyfin.client_token`: the caller's own profile only (resolved from its uid; the guest,
    /// spare accounts and other users are refused).
    pub(super) async fn jellyfin_client_token(
        &self,
        peer: &Peer,
    ) -> Result<JellyfinClientTokenResult, ErrorObject> {
        let (state, _) = self.load_state()?;
        let caller = self.caller(peer.uid, &state);
        let id = match (caller.role, caller.id) {
            (crate::profiles::Role::Profile, Some(id)) => id,
            _ => return Err(crate::profiles::not_a_profile(SERVICES_ACTION)),
        };
        let on = state.services.iter().any(|i| i == jellyfin::ID);
        self.jellyfin_ready(on)?;
        let paths = self.paths.clone();
        let profile = id.clone();
        let made = tokio::task::spawn_blocking(move || {
            let files = jellyfin::Files::new(&paths);
            let c = jellyfin::Client::new(
                &files,
                paths.server.jellyfin,
                &paths.users,
                Duration::from_secs(5),
            )?;
            jellyfin::client_token(&c, &files, &jellyfin::profiles(&paths)?, &profile)
        })
        .await
        .map_err(internal)?;
        match made {
            Ok(t) => Ok(t),
            Err(jellyfin::Error::NotReady) => Err(unavailable("jellyfin", "not_ready")),
            Err(e) => {
                warn!(profile = %id, "no Jellyfin token: {e}");
                Err(unavailable("jellyfin", "unreachable"))
            }
        }
    }

    /// After a profile rename: the Jellyfin user follows (in the background; best effort).
    pub(super) fn jellyfin_sync_later(&self) {
        if !self.service_enabled(jellyfin::ID) {
            return;
        }
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || jellyfin::sync_if_ready(&paths, &paths.server));
    }

    pub(super) async fn servers_request(
        &self,
        peer: &Peer,
        r: ServersRequest,
    ) -> Result<Value, ErrorObject> {
        let uid = peer.uid;
        let method = r.method();
        let (state, revision) = self.load_state()?;
        self.check_box_admin(uid, method, Some(r.pin())).await?;
        let on = |id: &str| state.services.iter().any(|i| i == id);
        match r {
            ServersRequest::Enable(p) => {
                if p.revision != revision {
                    return Err(state_conflict());
                }
                let index = self.catalog()?;
                let mut new: Vec<String> = Vec::new();
                for id in &p.ids {
                    let Some(e) = index.get(id) else {
                        return Err(ErrorObject::new(
                            ErrorCode::NotFound,
                            format!("{id} is not in this system's catalog"),
                        ));
                    };
                    if e.kind != EntryKind::Service {
                        return Err(ErrorObject::with_data(
                            ErrorCode::InvalidParams,
                            format!("{id} is not a service"),
                            json!({ "field": "ids", "reason": "not_a_service" }),
                        ));
                    }
                    if e.status != Status::Available {
                        return Err(ErrorObject::with_data(
                            ErrorCode::InvalidParams,
                            format!("{id} is not available"),
                            json!({ "field": "ids", "reason": "not_available" }),
                        ));
                    }
                    if !on(id) {
                        new.push(id.clone());
                    }
                }
                if let Some(extra) = p.accepted_terms.keys().find(|k| !new.contains(k)) {
                    return Err(ErrorObject::with_data(
                        ErrorCode::InvalidParams,
                        format!("{extra} is not being turned on"),
                        json!({ "field": "accepted_terms", "reason": "not_requested" }),
                    ));
                }
                let mut terms = Vec::new();
                let admin = self.admin_id_of(&state);
                let by = self.caller(uid, &state).user_name();
                for id in &new {
                    if let Some(t) = index.get(id).and_then(|e| e.terms.as_ref()) {
                        if p.accepted_terms.get(id) != Some(&t.version) {
                            return Err(ErrorObject::with_data(
                                ErrorCode::TermsRequired,
                                format!("{id} needs its terms accepted (version {})", t.version),
                                json!({ "id": id, "version": t.version }),
                            ));
                        }
                        terms.push(job::TermsRecord {
                            id: id.clone(),
                            version: t.version.clone(),
                            accepted_at: job::now(),
                            by: by.clone(),
                            profile: admin.clone(),
                        });
                    }
                }
                if new.is_empty() {
                    return job_ref(None);
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                self.preflight()?;
                let mut next = state.clone();
                next.services.extend(new.iter().cloned());
                let next = self.requirements_update(&next).unwrap_or(next);
                let spec = self
                    .spec(
                        JobKind::ServicesEnable,
                        uid,
                        Action::Services(ServicesJob {
                            apply: Apply {
                                state: Some(next),
                                terms: terms.clone(),
                                ..Default::default()
                            },
                            enabled: new.clone(),
                            delete: Vec::new(),
                        }),
                    )
                    .about(JobSubject {
                        services: Some(self.subject_services(&index, &new)),
                        ..Default::default()
                    });
                let id = self.start(spec).await?;
                for t in &terms {
                    self.audit
                        .terms_accepted(uid, &by, admin.as_deref(), &t.id, &t.version, &id);
                }
                info!(job = %id, services = ?new, "turning services on");
                job_ref(Some(id))
            }
            ServersRequest::Disable(p) => {
                if p.revision != revision {
                    return Err(state_conflict());
                }
                if let Some(id) = p.ids.iter().find(|i| !on(i)) {
                    return Err(ErrorObject::new(
                        ErrorCode::NotFound,
                        format!("{id} is not on"),
                    ));
                }
                let index = match self.catalog() {
                    Ok(i) => Some(i),
                    Err(e) if p.delete_data => return Err(e),
                    Err(_) => None,
                };
                let mut delete = Vec::new();
                if p.delete_data
                    && let Some(index) = &index
                {
                    for id in &p.ids {
                        if let Some(e) = index.get(id) {
                            for (base, rel) in servers::delete_targets(
                                e,
                                &self.paths.server,
                                &self.paths.state_dir,
                            ) {
                                if servers::delete_allowed(&base, &rel, &self.paths.server) {
                                    delete.push(DeleteTarget { base, rel });
                                } else {
                                    warn!(id, path = %rel, "not deleting: outside /var/lib and /var/cache");
                                }
                            }
                        }
                    }
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                self.preflight()?;
                let mut next = state.clone();
                next.services.retain(|i| !p.ids.contains(i));
                let next = self.requirements_update(&next).unwrap_or(next);
                let names = match &index {
                    Some(i) => self.subject_services(i, &p.ids),
                    None => p
                        .ids
                        .iter()
                        .map(|i| CatalogApp {
                            id: i.clone(),
                            name: i.clone(),
                        })
                        .collect(),
                };
                let spec = self
                    .spec(
                        JobKind::ServicesDisable,
                        uid,
                        Action::Services(ServicesJob {
                            apply: Apply {
                                state: Some(next),
                                ..Default::default()
                            },
                            enabled: Vec::new(),
                            delete,
                        }),
                    )
                    .about(JobSubject {
                        services: Some(names),
                        delete_data: Some(p.delete_data),
                        ..Default::default()
                    });
                let id = self.start(spec).await?;
                info!(job = %id, services = ?p.ids, delete_data = p.delete_data, "turning services off");
                job_ref(Some(id))
            }
            ServersRequest::LibraryAdd(p) => {
                self.jellyfin_ready(on(jellyfin::ID))?;
                let name = jellyfin::check_library_name(&p.name)
                    .map_err(|r| ErrorObject::invalid_field("name", r, "bad name"))?;
                match media::resolve(&self.paths.server.media_dir, &p.folder) {
                    Ok(_) => {}
                    Err(media::Resolve::BadId) => {
                        return Err(ErrorObject::invalid_field(
                            "folder",
                            "bad_format",
                            "folder must be a media.folders id",
                        ));
                    }
                    Err(_) => return Err(ErrorObject::new(ErrorCode::NotFound, "no such folder")),
                }
                if let Some(folders) = self.jellyfin_folders().await
                    && folders
                        .iter()
                        .any(|f| f.name.to_lowercase() == name.to_lowercase())
                {
                    return Err(ErrorObject::invalid_field(
                        "name",
                        "exists",
                        "a library has this name",
                    ));
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let subject = JobSubject {
                    library: Some(SubjectLibrary {
                        id: None,
                        name: name.clone(),
                        kind: Some(p.kind),
                        folder: Some(p.folder.clone()),
                    }),
                    ..Default::default()
                };
                let spec = self
                    .spec(
                        JobKind::JellyfinLibraryAdd,
                        uid,
                        Action::Jellyfin(JellyfinJob {
                            op: JellyfinOp::AddLibrary {
                                name,
                                kind: p.kind,
                                folder: p.folder,
                            },
                        }),
                    )
                    .about(subject);
                job_ref(Some(self.start(spec).await?))
            }
            ServersRequest::LibraryRemove(p) => {
                self.jellyfin_ready(on(jellyfin::ID))?;
                let mut name = None;
                if let Some(folders) = self.jellyfin_folders().await {
                    match folders
                        .iter()
                        .find(|f| f.item_id.as_deref() == Some(p.library_id.as_str()))
                    {
                        Some(f) => name = Some(f.name.clone()),
                        None => {
                            return Err(ErrorObject::new(ErrorCode::NotFound, "no such library"));
                        }
                    }
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let subject = JobSubject {
                    library: Some(SubjectLibrary {
                        id: Some(p.library_id.clone()),
                        name: name.unwrap_or_default(),
                        kind: None,
                        folder: None,
                    }),
                    ..Default::default()
                };
                let spec = self
                    .spec(
                        JobKind::JellyfinLibraryRemove,
                        uid,
                        Action::Jellyfin(JellyfinJob {
                            op: JellyfinOp::RemoveLibrary {
                                library_id: p.library_id,
                            },
                        }),
                    )
                    .about(subject);
                job_ref(Some(self.start(spec).await?))
            }
            ServersRequest::Reveal(_) => {
                if !on(jellyfin::ID) {
                    return Err(unavailable("jellyfin", "off"));
                }
                let paths = self.paths.clone();
                let result = tokio::task::spawn_blocking(move || reveal(&paths))
                    .await
                    .map_err(internal)?;
                info!(uid, users = result.users.len(), "Jellyfin passwords shown");
                serde_json::to_value(result).map_err(internal)
            }
            ServersRequest::PlexClaim(p) => {
                if !on(servers::PLEX) {
                    return Err(unavailable("plex", "off"));
                }
                let prefs = self.paths.server.plex_preferences.clone();
                let claimed = tokio::task::spawn_blocking(move || {
                    plex::read(&prefs).is_some_and(|p| p.claimed)
                })
                .await
                .map_err(internal)?;
                if claimed {
                    return job_ref(None);
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let spec = self.spec(
                    JobKind::PlexClaim,
                    uid,
                    Action::Plex(PlexJob { code: p.code }),
                );
                job_ref(Some(self.start(spec).await?))
            }
            ServersRequest::TailscaleLogin(_) | ServersRequest::TailscaleLogout(_) => {
                let login = matches!(r, ServersRequest::TailscaleLogin(_));
                if !on(servers::TAILSCALE) {
                    return Err(unavailable("tailscale", "off"));
                }
                let socket = self.paths.server.tailscale_socket.clone();
                let backend = tokio::task::spawn_blocking(move || {
                    tailscale::status(&socket, READ_TIMEOUT)
                        .ok()
                        .map(|s| tailscale::backend_state(&s).to_owned())
                })
                .await
                .map_err(internal)?;
                // Already signed in (or out): nothing to do.
                match (login, backend.as_deref()) {
                    (true, Some("Running")) | (false, Some("NeedsLogin")) => return job_ref(None),
                    _ => {}
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let kind = if login {
                    JobKind::TailscaleLogin
                } else {
                    JobKind::TailscaleLogout
                };
                let spec = self.spec(kind, uid, Action::Tailscale(TailscaleJob { login }));
                job_ref(Some(self.start(spec).await?))
            }
            ServersRequest::SetToken(p) => {
                if !on(servers::CLOUDFLARE) {
                    return Err(unavailable("cloudflare", "off"));
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let dir = self.paths.services_dir().join(servers::CLOUDFLARE);
                // The job restarts the tunnel on the new token; if it cannot start, the old
                // token comes back, so a later restart does not pick up one that was refused.
                let previous = std::fs::read(dir.join("token")).ok().map(Zeroizing::new);
                write_token(&dir, &p.token)
                    .map_err(|e| internal(format!("writing the tunnel token: {e}")))?;
                info!(uid, "Cloudflare tunnel token written");
                let spec = self.spec(
                    JobKind::CloudflareSetToken,
                    uid,
                    Action::Cloudflare(CloudflareJob { set: true }),
                );
                let started = self.start(spec).await;
                if started.is_err()
                    && let Err(e) = restore_token(&dir, previous.as_deref().map(Vec::as_slice))
                {
                    warn!("cannot put the previous tunnel token back: {e}");
                }
                job_ref(Some(started?))
            }
            ServersRequest::ClearToken(_) => {
                if !on(servers::CLOUDFLARE) {
                    return Err(unavailable("cloudflare", "off"));
                }
                let token = self
                    .paths
                    .services_dir()
                    .join(servers::CLOUDFLARE)
                    .join("token");
                if !token.exists() {
                    return job_ref(None);
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let spec = self.spec(
                    JobKind::CloudflareClearToken,
                    uid,
                    Action::Cloudflare(CloudflareJob { set: false }),
                );
                job_ref(Some(self.start(spec).await?))
            }
        }
    }

    /// Jellyfin is on and set up, else `unavailable {service: jellyfin}`.
    fn jellyfin_ready(&self, on: bool) -> Result<(), ErrorObject> {
        if !on {
            return Err(unavailable("jellyfin", "off"));
        }
        if !jellyfin::Files::new(&self.paths).setup_done() {
            return Err(unavailable("jellyfin", "not_ready"));
        }
        Ok(())
    }

    /// Jellyfin's libraries now (`None`: it does not answer in time; the job checks again).
    async fn jellyfin_folders(&self) -> Option<Vec<jellyfin::VirtualFolder>> {
        let files = jellyfin::Files::new(&self.paths);
        let addr = self.paths.server.jellyfin;
        let users = self.paths.users.clone();
        tokio::task::spawn_blocking(move || {
            jellyfin::Client::new(&files, addr, &users, READ_TIMEOUT)
                .and_then(|c| c.virtual_folders())
                .ok()
        })
        .await
        .ok()
        .flatten()
    }

    fn admin_id_of(&self, state: &crate::state::State) -> Option<String> {
        self.profiles_file().admin(state).map(|(a, _)| a.id)
    }

    fn subject_services(&self, index: &Index, ids: &[String]) -> Vec<CatalogApp> {
        ids.iter()
            .map(|id| CatalogApp {
                id: id.clone(),
                name: index.get(id).map_or_else(|| id.clone(), |e| e.name.clone()),
            })
            .collect()
    }
}

/// `jellyfin.users.reveal`: users made first (best effort), then the admin's and each
/// profile's password. Profiles that cannot be read: the admin only, and no sync.
fn reveal(paths: &crate::config::Paths) -> JellyfinUsersRevealResult {
    let files = jellyfin::Files::new(paths);
    let profiles = match jellyfin::profiles(paths) {
        Ok(p) => {
            if let Ok(c) = jellyfin::Client::new(
                &files,
                paths.server.jellyfin,
                &paths.users,
                Duration::from_secs(3),
            ) && let Err(e) = jellyfin::sync(&c, &files, &p)
            {
                warn!("Jellyfin users not synced: {e}");
            }
            p
        }
        Err(e) => {
            warn!("Jellyfin users not synced: {e}");
            Vec::new()
        }
    };
    let stored = jellyfin::UsersFile::load(&files.users_json()).unwrap_or_default();
    JellyfinUsersRevealResult {
        admin: files.admin_password().map(|password| JellyfinAccount {
            name: jellyfin::ADMIN_USER.into(),
            password,
        }),
        users: profiles
            .iter()
            .filter_map(|p| {
                let r = stored.users.get(&p.id).filter(|r| !r.is_pending())?;
                Some(JellyfinProfileAccount {
                    profile_id: p.id.clone(),
                    profile_name: p.name.clone(),
                    name: r.name.clone(),
                    password: r.password.clone(),
                })
            })
            .collect(),
    }
}

/// The checked token, `0600` root, atomically (the directory `0700`).
fn write_token(dir: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    crate::fsutil::write_atomic(&dir.join("token"), token.as_bytes(), 0o600)
}

/// Put the token file back as it was before [`write_token`] (`None`: there was none).
fn restore_token(dir: &std::path::Path, previous: Option<&[u8]>) -> std::io::Result<()> {
    let path = dir.join("token");
    match previous {
        Some(bytes) => crate::fsutil::write_atomic(&path, bytes, 0o600),
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(method: &str, params: Value) -> Result<ServersRequest, ErrorObject> {
        ServersRequest::decode(method, &mut Some(params)).expect("a server method")
    }

    #[test]
    fn requests_are_checked() {
        let rev = "0".repeat(64);
        let token = crate::servers::cloudflare::tests::good();
        let ok = [
            (
                "services.enable",
                json!({"ids": ["jellyfin-server"], "pin": "1234", "revision": rev}),
            ),
            (
                "services.enable",
                json!({"ids": ["plex-media-server"], "pin": "1234", "revision": rev, "accepted_terms": {"plex-media-server": "2025-10-30"}}),
            ),
            (
                "services.disable",
                json!({"ids": ["tailscale"], "pin": "1234", "revision": rev, "delete_data": true}),
            ),
            (
                "jellyfin.libraries.add",
                json!({"pin": "1234", "name": "Movies", "kind": "movies", "folder": "Movies/HD"}),
            ),
            (
                "jellyfin.libraries.remove",
                json!({"pin": "1234", "library_id": "f137a2dd21bbc1b99aa5c0f6bf02a805"}),
            ),
            ("jellyfin.users.reveal", json!({"pin": "1234"})),
            (
                "plex.claim",
                json!({"pin": "1234", "code": "claim-AbCdEfGhIjKlMnOpQrSt"}),
            ),
            ("tailscale.login", json!({"pin": "1234"})),
            ("tailscale.logout", json!({"pin": "1234"})),
            (
                "cloudflare.set_token",
                json!({"pin": "1234", "token": token}),
            ),
            ("cloudflare.clear_token", json!({"pin": "1234"})),
        ];
        for (m, p) in ok {
            assert!(decode(m, p.clone()).is_ok(), "{m} {p}");
        }
        let bad = [
            (
                "services.enable",
                json!({"ids": [], "pin": "1234", "revision": rev}),
                "ids",
            ),
            (
                "services.enable",
                json!({"ids": ["a", "a"], "pin": "1234", "revision": rev}),
                "ids",
            ),
            (
                "services.enable",
                json!({"ids": ["../x"], "pin": "1234", "revision": rev}),
                "ids",
            ),
            (
                "services.enable",
                json!({"ids": ["a"], "pin": "12", "revision": rev}),
                "pin",
            ),
            (
                "services.enable",
                json!({"ids": ["a"], "pin": "1234", "revision": "x"}),
                "revision",
            ),
            (
                "services.disable",
                json!({"ids": ["a"], "pin": "1234", "revision": rev, "delete_data": true, "path": "/"}),
                "path",
            ),
            (
                "jellyfin.libraries.add",
                json!({"pin": "1234", "name": "M", "kind": "books", "folder": "M"}),
                "kind",
            ),
            (
                "jellyfin.libraries.add",
                json!({"pin": "1234", "name": "M", "kind": "movies", "folder": "../etc"}),
                "folder",
            ),
            (
                "jellyfin.libraries.add",
                json!({"pin": "1234", "name": "M", "kind": "movies", "folder": "/srv/media/M"}),
                "folder",
            ),
            (
                "jellyfin.libraries.add",
                json!({"pin": "1234", "name": "a/b", "kind": "movies", "folder": "M"}),
                "name",
            ),
            (
                "jellyfin.libraries.add",
                json!({"pin": "1234", "name": "M", "kind": "movies", "path": "/etc"}),
                "path",
            ),
            (
                "jellyfin.libraries.remove",
                json!({"pin": "1234", "library_id": "../x"}),
                "library_id",
            ),
            (
                "plex.claim",
                json!({"pin": "1234", "code": "claim-short"}),
                "code",
            ),
            (
                "plex.claim",
                json!({"pin": "1234", "code": "claim-AbCdEfGhIjKlMnOpQrSt", "url": "x"}),
                "url",
            ),
            (
                "cloudflare.set_token",
                json!({"pin": "1234", "token": "eyJhIjoiYiJ9"}),
                "token",
            ),
            (
                "tailscale.login",
                json!({"pin": "1234", "exit_node": "x"}),
                "exit_node",
            ),
        ];
        for (m, p, field) in bad {
            let e = decode(m, p.clone())
                .err()
                .unwrap_or_else(|| panic!("{m} {p} accepted"));
            assert_eq!(e.kind(), Some(ErrorCode::InvalidParams), "{m} {p}");
            // serde names the field for unknown fields; the validators always do.
            if let Some(d) = &e.data {
                assert_eq!(d["field"], json!(field), "{m} {p}: {e:?}");
            }
        }
        let mut params = Some(serde_json::json!({}));
        assert!(ServersRequest::decode("services.list", &mut params).is_none());
        assert!(params.is_some(), "left for the caller");
    }

    #[test]
    fn token_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("services/cloudflare-tunnel");
        write_token(&dir, "abc").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("token")).unwrap(), "abc");
        // A job that cannot start: the previous token (or none) comes back.
        write_token(&dir, "new").unwrap();
        restore_token(&dir, Some(b"abc")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("token")).unwrap(), "abc");
        restore_token(&dir, None).unwrap();
        assert!(!dir.join("token").exists());
        restore_token(&dir, None).unwrap();
        write_token(&dir, "abc").unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
