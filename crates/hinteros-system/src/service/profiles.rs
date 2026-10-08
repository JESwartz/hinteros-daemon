//! Profiles (system API 0.4, profiles.md "API changes"): `profiles.*`, `guest.set`,
//! `starter_set.*`, `display.save_mode`, `audio.save_output`, the public view and the switch
//! hint. PINs are checked here with the shared backoff (never logged; the audit records which
//! PIN was checked and the outcome).

use std::collections::BTreeSet;

use hinteros_api::system::{
    AudioSaveOutputParams, CallerInfo, DisplaySaveModeParams, Empty, GuestInfo, GuestSetParams,
    JobKind, PinParams, ProfileIdResult, ProfileInfo, ProfileUsage, ProfilesCreateParams,
    ProfilesCreateResult, ProfilesListResult, ProfilesRemoveParams, ProfilesRenameParams,
    ProfilesResetPinParams, ProfilesSetAppearanceParams, ProfilesSetLockedParams,
    ProfilesSetPinParams, ProfilesSetupAdminParams, ProfilesSwitchParams, StarterSetGetResult,
    StarterSetSetParams,
};
use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::{Value, json};
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::{Service, internal, job_ref, state_conflict};
use crate::appreg::{self, Owner, StarterFile};
use crate::job::{self, Action, AppsJob, AppsOp, ProfileCreateJob, ProfileRemoveJob};
use crate::peer::Peer;
use crate::pins::{self, Pins, Verify};
use crate::profiles::{self, Appearance, Meta, ProfilesFile, Role};
use crate::public::{self, SwitchHint};
use crate::state::{self, MAX_PROFILES, Profile, State};
use crate::usage;

const PROFILES_ACTION: &str = "org.hinteros.system.profiles";
const RECOVER_ACTION: &str = "org.hinteros.system.profiles.recover";

/// Which PIN a method checks (`target`, the profile being entered, is checked in PAM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Own,
    Admin,
}

impl Which {
    fn name(self) -> &'static str {
        match self {
            Which::Own => "self",
            Which::Admin => "admin",
        }
    }
}

fn pin_required(which: Which, retry_after_ms: Option<i64>, message: &str) -> ErrorObject {
    let mut data = json!({ "pin": which.name() });
    if let Some(ms) = retry_after_ms {
        data["retry_after_ms"] = json!(ms);
    }
    ErrorObject::with_data(ErrorCode::PinRequired, message, data)
}

fn bad_pin(field: &str) -> ErrorObject {
    ErrorObject::invalid_field(field, "bad_format", "a PIN is 4 to 8 digits")
}

/// A decoded profiles request (params already decoded strictly by the gate).
pub enum ProfilesRequest {
    Create(ProfilesCreateParams),
    SetupAdmin(ProfilesSetupAdminParams),
    Rename(ProfilesRenameParams),
    SetAppearance(ProfilesSetAppearanceParams),
    SetPin(ProfilesSetPinParams),
    SetLocked(ProfilesSetLockedParams),
    ResetPin(ProfilesResetPinParams),
    Remove(ProfilesRemoveParams),
    Switch(ProfilesSwitchParams),
    Unlock(PinParams),
    GuestSet(GuestSetParams),
    SaveMode(DisplaySaveModeParams),
    SaveOutput(AudioSaveOutputParams),
    StarterSet(StarterSetSetParams),
}

fn check_id(id: &str, field: &str) -> Result<(), ErrorObject> {
    if state::is_profile_id(id) {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            field,
            "bad_format",
            "profile ids look like p1",
        ))
    }
}

fn check_pin_field(pin: &str, field: &str) -> Result<(), ErrorObject> {
    if pins::is_pin(pin) {
        Ok(())
    } else {
        Err(bad_pin(field))
    }
}

impl ProfilesRequest {
    /// Validation of the formats (before the rate limit and polkit).
    pub fn validate(&self) -> Result<(), ErrorObject> {
        use ProfilesRequest as R;
        match self {
            R::Create(p) => {
                profiles::check_name(&p.name)?;
                if let Some(c) = &p.color {
                    profiles::check_color(c)?;
                }
                check_pin_field(&p.pin, "pin")?;
                super::check_revision(&p.revision)
            }
            R::SetupAdmin(p) => {
                profiles::check_name(&p.name)?;
                if let Some(c) = &p.color {
                    profiles::check_color(c)?;
                }
                check_pin_field(&p.pin, "pin")
            }
            R::Rename(p) => {
                check_id(&p.id, "id")?;
                profiles::check_name(&p.name)?;
                p.pin
                    .as_deref()
                    .map_or(Ok(()), |x| check_pin_field(x, "pin"))
            }
            R::SetAppearance(p) => {
                if let Some(c) = &p.color {
                    profiles::check_color(c)?;
                }
                if let Some(s) = p.text_scale
                    && !(0.5..=3.0).contains(&s)
                {
                    return Err(ErrorObject::invalid_field(
                        "text_scale",
                        "out_of_range",
                        "text_scale is 0.5 to 3.0",
                    ));
                }
                Ok(())
            }
            R::SetPin(p) => {
                check_pin_field(&p.pin, "pin")?;
                p.current_pin
                    .as_deref()
                    .map_or(Ok(()), |x| check_pin_field(x, "current_pin"))
            }
            R::SetLocked(p) => check_pin_field(&p.pin, "pin"),
            R::ResetPin(p) => {
                check_id(&p.id, "id")?;
                check_pin_field(&p.new_pin, "new_pin")?;
                p.admin_pin
                    .as_deref()
                    .map_or(Ok(()), |x| check_pin_field(x, "admin_pin"))
            }
            R::Remove(p) => {
                check_id(&p.id, "id")?;
                if p.confirm_id != p.id {
                    return Err(ErrorObject::invalid_field(
                        "confirm_id",
                        "mismatch",
                        "confirm_id must equal id",
                    ));
                }
                check_pin_field(&p.pin, "pin")?;
                super::check_revision(&p.revision)
            }
            R::Switch(p) => {
                if p.id == "guest" {
                    Ok(())
                } else {
                    check_id(&p.id, "id")
                }
            }
            R::Unlock(p) => check_pin_field(&p.pin, "pin"),
            R::GuestSet(p) => check_pin_field(&p.admin_pin, "admin_pin"),
            R::SaveMode(p) => {
                if p.display.is_empty()
                    || p.display.chars().count() > 128
                    || p.display.chars().any(|c| c == ':' || c.is_control())
                {
                    return Err(ErrorObject::invalid_field(
                        "display",
                        "bad_format",
                        "a display name",
                    ));
                }
                if !(320..=16384).contains(&p.width)
                    || !(200..=16384).contains(&p.height)
                    || !(1..=1000).contains(&p.refresh)
                {
                    return Err(ErrorObject::invalid_field(
                        "width",
                        "out_of_range",
                        "not a display mode",
                    ));
                }
                Ok(())
            }
            R::SaveOutput(p) => {
                let ok = (1..=128).contains(&p.node.len())
                    && p.node.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
                    });
                if ok {
                    Ok(())
                } else {
                    Err(ErrorObject::invalid_field(
                        "node",
                        "bad_format",
                        "a PipeWire node name",
                    ))
                }
            }
            R::StarterSet(p) => {
                if p.ids.len() > 16 || p.ids.iter().any(|i| !hinteros_api::catalog::is_app_id(i)) {
                    return Err(ErrorObject::invalid_field(
                        "ids",
                        "bad_format",
                        "0 to 16 catalog ids",
                    ));
                }
                check_pin_field(&p.pin, "pin")
            }
        }
    }
}

impl Service {
    fn require_schema2(&self) -> Result<(State, String), ErrorObject> {
        let (state, rev) = self.load_state()?;
        if !state.is_schema2() {
            // Before the migration the box has one implicit profile; nothing to manage yet.
            return Err(profiles::schema1());
        }
        Ok((state, rev))
    }

    /// Check `pin` against profile `id`'s with the shared backoff. A profile without a PIN yet
    /// (migrated p1) passes. The audit records which PIN and the outcome, never the PIN.
    pub(super) async fn check_pin(
        &self,
        uid: u32,
        method: &str,
        id: &str,
        pin: Option<&str>,
        which: Which,
    ) -> Result<(), ErrorObject> {
        let pins = Pins::new(self.paths.pins_dir());
        if !pins.is_set(id) {
            self.audit.pin_check(uid, method, which.name(), "not_set");
            return Ok(());
        }
        let Some(pin) = pin else {
            self.audit.pin_check(uid, method, which.name(), "missing");
            return Err(pin_required(which, None, "a PIN is needed"));
        };
        let pin = Zeroizing::new(pin.to_owned());
        let id2 = id.to_owned();
        let now = job::now();
        let outcome = tokio::task::spawn_blocking(move || pins.verify(&id2, &pin, now))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        let (result, label) = match outcome {
            Verify::Ok | Verify::NoPin => (Ok(()), "verified"),
            Verify::Wrong { locked_for } => (
                Err(pin_required(
                    which,
                    (locked_for > 0).then_some(locked_for * 1000),
                    "that PIN didn't match",
                )),
                "failed",
            ),
            Verify::Locked { seconds } => (
                Err(pin_required(which, Some(seconds * 1000), "too many tries")),
                "locked",
            ),
        };
        self.audit.pin_check(uid, method, which.name(), label);
        result
    }

    /// Rewrite the public view and tell subscribers.
    pub(super) fn profiles_changed(&self) {
        if let Ok((state, _)) = self.load_state() {
            public::publish(&self.paths, &state, &self.profiles_file(), job::now());
        }
        let _ = self
            .events
            .send((hinteros_api::system::EventName::ProfilesChanged, json!({})));
    }

    fn save_file(&self, file: &ProfilesFile) -> Result<(), ErrorObject> {
        file.save(&self.paths.profiles_json()).map_err(internal)
    }

    /// The caller's own profile (id, metadata).
    fn own_profile(&self, uid: u32, state: &State) -> Result<(String, ProfilesFile), ErrorObject> {
        let caller = self.caller(uid, state);
        match (caller.role, caller.id) {
            (Role::Profile, Some(id)) => Ok((id, self.profiles_file())),
            _ => Err(profiles::not_a_profile(PROFILES_ACTION)),
        }
    }

    /// The admin profile's id, if there is one.
    fn admin_id(&self, state: &State, file: &ProfilesFile) -> Option<String> {
        file.admin(state).map(|(a, _)| a.id)
    }

    pub(super) async fn profiles_list(
        &self,
        peer: &Peer,
    ) -> Result<ProfilesListResult, ErrorObject> {
        let (state, _) = self.load_state()?;
        let file = self.profiles_file();
        let caller = self.caller(peer.uid, &state);
        let pins = Pins::new(self.paths.pins_dir());
        let now = job::now();
        let usage = usage::load(&self.paths.usage_json());
        let stale = usage
            .values()
            .map(|u| u.at)
            .min()
            .is_none_or(|at| now - at > 3600);
        if stale {
            self.start_usage_scan().await;
        }
        let to_api = |u: &usage::Usage| ProfileUsage {
            total_bytes: u.total_bytes,
            steam_games_bytes: u.steam_games_bytes,
            at: u.at,
        };
        let active_uid = crate::authz::seat_owner(&self.bus).await.ok().flatten();
        let mut list: Vec<ProfileInfo> = file
            .visible(&state)
            .into_iter()
            .map(|(a, m)| {
                let uid = self.users.by_name(&a.user).map(|u| u.uid);
                ProfileInfo {
                    id: a.id.clone(),
                    name: m.name.clone(),
                    color: m.color.clone(),
                    user: a.user.clone(),
                    admin: m.admin,
                    locked: m.locked,
                    locked_until: Some(pins.backoff(&a.id).locked_until).filter(|t| *t > now),
                    active: uid.is_some() && uid == active_uid,
                    last_used_at: m.last_used_at,
                    usage: usage.get(&a.id).map(to_api),
                    // Schema 1 has no PINs yet: nothing to set until the migration ran.
                    pin_set: pins.is_set(&a.id) || !state.is_schema2(),
                }
            })
            .collect();
        // Schema 1 before profiles.json: p1 as an implicit profile.
        if list.is_empty() && !state.is_schema2() {
            let a = &state.accounts()[0];
            list.push(ProfileInfo {
                id: a.id.clone(),
                name: "Profile 1".into(),
                color: profiles::COLORS[0].into(),
                user: a.user.clone(),
                admin: true,
                locked: false,
                locked_until: None,
                active: true,
                last_used_at: None,
                usage: usage.get(&a.id).map(to_api),
                pin_set: true,
            });
        }
        Ok(ProfilesListResult {
            profiles: list,
            guest: GuestInfo {
                enabled: state.guest_enabled(),
                usage: usage.get("guest").map(to_api),
            },
            max: MAX_PROFILES as u32,
            first_boot: state.is_schema2() && file.first_boot(&state),
            caller: CallerInfo {
                id: caller.id.clone(),
                user: caller.user_name(),
                guest: caller.role == Role::Guest,
                spare: caller.role == Role::Spare,
            },
        })
    }

    async fn start_usage_scan(&self) {
        if let Ok(conn) = self.bus.get().await {
            let _ = conn
                .call_method(
                    Some("org.freedesktop.systemd1"),
                    "/org/freedesktop/systemd1",
                    Some("org.freedesktop.systemd1.Manager"),
                    "StartUnit",
                    &("hinteros-usage.service", "replace"),
                )
                .await;
        }
    }

    pub(super) fn starter_get(&self) -> Result<StarterSetGetResult, ErrorObject> {
        let (state, _) = self.load_state()?;
        if !state.is_schema2() {
            return Err(profiles::schema1());
        }
        let f = StarterFile::load(&self.paths.starter_json());
        Ok(StarterSetGetResult {
            ids: f.as_ref().map(|f| f.ids.clone()).unwrap_or_default(),
            updated_at: f.and_then(|f| f.updated_at),
        })
    }

    pub(super) async fn profiles_request(
        &self,
        peer: &Peer,
        method: &str,
        request: ProfilesRequest,
        from_greeter: bool,
    ) -> Result<Value, ErrorObject> {
        use ProfilesRequest as R;
        let uid = peer.uid;
        let empty = || serde_json::to_value(Empty {}).map_err(internal);
        match request {
            R::SaveMode(p) => {
                self.save_runtime(|r| {
                    r.display_modes.insert(
                        p.display.clone(),
                        format!("{}x{}@{}", p.width, p.height, p.refresh),
                    );
                })?;
                empty()
            }
            R::SaveOutput(p) => {
                self.save_runtime(|r| r.audio_output = Some(p.node.clone()))?;
                empty()
            }
            R::Create(p) => {
                let (state, revision) = self.require_schema2()?;
                if p.revision != revision {
                    return Err(state_conflict());
                }
                let mut file = self.profiles_file();
                if file.visible(&state).len() >= MAX_PROFILES
                    || state.profiles.len() >= MAX_PROFILES
                {
                    return Err(ErrorObject::with_data(
                        ErrorCode::ProfileLimit,
                        format!("there are already {MAX_PROFILES} profiles"),
                        json!({ "max": MAX_PROFILES }),
                    ));
                }
                self.preflight()?;
                if let Some(e) = self.jobs.lock().unwrap().busy_error() {
                    return Err(e);
                }
                let name = profiles::check_name(&p.name)?;
                let id = file.take_id(&state, &self.users);
                // The counter keeps the number even if the job fails.
                self.save_file(&file)?;
                let user = state::user_for(&id);
                let mut new = state.clone();
                new.profiles.push(Profile {
                    id: id.clone(),
                    name: None,
                    user: user.clone(),
                });
                let new = self.requirements_update(&new).unwrap_or(new);
                let pin = Zeroizing::new(p.pin.clone());
                let pin_hash = tokio::task::spawn_blocking(move || pins::hash(&pin))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?;
                let meta = Meta {
                    name,
                    color: p.color.unwrap_or_else(|| profiles::COLORS[0].into()),
                    admin: false,
                    locked: p.locked,
                    created_at: job::now(),
                    last_used_at: None,
                    appearance: Appearance::default(),
                };
                let spec = self.spec(
                    JobKind::ProfilesCreate,
                    uid,
                    Action::ProfileCreate(ProfileCreateJob {
                        state: new,
                        id: id.clone(),
                        user,
                        meta,
                        pin_hash,
                        token: from_greeter,
                    }),
                );
                let job_id = self.start(spec).await?;
                info!(job = %job_id, profile = %id, "creating a profile");
                serde_json::to_value(ProfilesCreateResult { job_id, id }).map_err(internal)
            }
            R::SetupAdmin(p) => {
                let (state, _) = self.require_schema2()?;
                let caller = self.caller(uid, &state);
                let mut file = self.profiles_file();
                let (Role::Spare, Some(id)) = (&caller.role, &caller.id) else {
                    return Err(profiles::not_a_profile(PROFILES_ACTION));
                };
                if !file.first_boot(&state) {
                    return Err(profiles::not_a_profile(PROFILES_ACTION));
                }
                let pin = Zeroizing::new(p.pin.clone());
                let pins = Pins::new(self.paths.pins_dir());
                let id2 = id.clone();
                tokio::task::spawn_blocking(move || pins.set(&id2, &pin))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?;
                file.profiles.insert(
                    id.clone(),
                    Meta {
                        name: profiles::check_name(&p.name)?,
                        color: p.color.unwrap_or_else(|| profiles::COLORS[0].into()),
                        admin: true,
                        locked: p.locked,
                        created_at: job::now(),
                        last_used_at: Some(job::now()),
                        appearance: Appearance::default(),
                    },
                );
                self.save_file(&file)?;
                info!(profile = %id, "admin profile created (first boot)");
                self.profiles_changed();
                // Its apps: the starter env, as soon as the box is idle.
                self.ensure_envs().await;
                serde_json::to_value(ProfileIdResult { id: id.clone() }).map_err(internal)
            }
            R::Rename(p) => {
                let (state, _) = self.require_schema2()?;
                let caller = self.caller(uid, &state);
                let mut file = self.profiles_file();
                if !file.visible(&state).iter().any(|(a, _)| a.id == p.id) {
                    return Err(ErrorObject::new(ErrorCode::NotFound, "no such profile"));
                }
                if caller.id.as_deref() != Some(&p.id) || caller.role != Role::Profile {
                    let admin = self.admin_id(&state, &file).ok_or_else(|| {
                        pin_required(Which::Admin, None, "there is no admin profile")
                    })?;
                    self.check_pin(uid, method, &admin, p.pin.as_deref(), Which::Admin)
                        .await?;
                }
                file.profiles.get_mut(&p.id).expect("visible").name =
                    profiles::check_name(&p.name)?;
                self.save_file(&file)?;
                self.profiles_changed();
                empty()
            }
            R::SetAppearance(p) => {
                let (state, _) = self.require_schema2()?;
                let (id, mut file) = self.own_profile(uid, &state)?;
                let m = file.profiles.get_mut(&id).expect("own profile");
                if let Some(c) = p.color {
                    m.color = c;
                }
                if let Some(s) = p.text_scale {
                    m.appearance.text_scale = s;
                }
                if let Some(b) = p.high_contrast {
                    m.appearance.high_contrast = b;
                }
                if let Some(b) = p.reduce_motion {
                    m.appearance.reduce_motion = b;
                }
                self.save_file(&file)?;
                self.profiles_changed();
                empty()
            }
            R::SetPin(p) => {
                let (state, _) = self.require_schema2()?;
                let (id, _) = self.own_profile(uid, &state)?;
                self.check_pin(uid, method, &id, p.current_pin.as_deref(), Which::Own)
                    .await?;
                let pin = Zeroizing::new(p.pin.clone());
                let pins = Pins::new(self.paths.pins_dir());
                tokio::task::spawn_blocking(move || pins.set(&id, &pin))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?;
                self.profiles_changed();
                empty()
            }
            R::SetLocked(p) => {
                let (state, _) = self.require_schema2()?;
                let (id, mut file) = self.own_profile(uid, &state)?;
                self.check_pin(uid, method, &id, Some(&p.pin), Which::Own)
                    .await?;
                file.profiles.get_mut(&id).expect("own profile").locked = p.locked;
                self.save_file(&file)?;
                self.profiles_changed();
                empty()
            }
            R::ResetPin(p) => {
                let (state, _) = self.require_schema2()?;
                let file = self.profiles_file();
                let Some((_, meta)) = file.visible(&state).into_iter().find(|(a, _)| a.id == p.id)
                else {
                    return Err(ErrorObject::new(ErrorCode::NotFound, "no such profile"));
                };
                if meta.admin {
                    // The admin profile's own PIN: the admin account's password.
                    crate::authz::authorize(
                        &self.bus,
                        peer,
                        RECOVER_ACTION,
                        Some(self.args.auth_timeout()),
                    )
                    .await
                    .map_err(|d| d.error(RECOVER_ACTION))?;
                } else {
                    let admin = self.admin_id(&state, &file).ok_or_else(|| {
                        pin_required(Which::Admin, None, "there is no admin profile")
                    })?;
                    self.check_pin(uid, method, &admin, p.admin_pin.as_deref(), Which::Admin)
                        .await?;
                }
                let pin = Zeroizing::new(p.new_pin.clone());
                let pins = Pins::new(self.paths.pins_dir());
                let id = p.id.clone();
                tokio::task::spawn_blocking(move || pins.set(&id, &pin))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?;
                info!(profile = %p.id, "PIN reset");
                self.profiles_changed();
                empty()
            }
            R::Remove(p) => {
                let (state, revision) = self.require_schema2()?;
                if p.revision != revision {
                    return Err(state_conflict());
                }
                let file = self.profiles_file();
                let Some((account, meta)) =
                    file.visible(&state).into_iter().find(|(a, _)| a.id == p.id)
                else {
                    return Err(ErrorObject::new(ErrorCode::NotFound, "no such profile"));
                };
                if meta.admin {
                    return Err(ErrorObject::invalid_field(
                        "id",
                        "admin",
                        "the admin profile cannot be removed",
                    ));
                }
                let caller = self.caller(uid, &state);
                if caller.id.as_deref() == Some(&p.id) {
                    self.check_pin(uid, method, &p.id, Some(&p.pin), Which::Own)
                        .await?;
                } else {
                    let admin = self.admin_id(&state, &file).ok_or_else(|| {
                        pin_required(Which::Admin, None, "there is no admin profile")
                    })?;
                    self.check_pin(uid, method, &admin, Some(&p.pin), Which::Admin)
                        .await?;
                }
                self.preflight()?;
                let mut new = state.clone();
                new.profiles.retain(|x| x.id != p.id);
                // Box features only this profile needed go with this rebuild.
                let needed: BTreeSet<String> = appreg::owners(&new)
                    .iter()
                    .filter_map(|o| appreg::current(o, &self.paths))
                    .flat_map(|c| c.manifest.box_features)
                    .collect();
                new.requirements = Some(needed.into_iter().collect());
                let user = self.users.by_name(&account.user);
                let spec = self.spec(
                    JobKind::ProfilesRemove,
                    uid,
                    Action::ProfileRemove(ProfileRemoveJob {
                        state: new,
                        id: p.id.clone(),
                        user: account.user.clone(),
                        uid: user.as_ref().map(|u| u.uid),
                        home: user
                            .filter(|u| u.uid >= 1000 && u.home.is_absolute())
                            .map(|u| u.home)
                            .filter(|h| h != std::path::Path::new("/")),
                    }),
                );
                let job_id = self.start(spec).await?;
                info!(job = %job_id, profile = %p.id, user = %account.user, "removing a profile");
                job_ref(Some(job_id))
            }
            R::Switch(p) => {
                let (state, _) = self.require_schema2()?;
                let file = self.profiles_file();
                let caller = self.caller(uid, &state);
                let target_user = if p.id == "guest" {
                    if !state.guest_enabled() {
                        return Err(ErrorObject::new(ErrorCode::NotFound, "the guest is off"));
                    }
                    state::GUEST_USER.to_string()
                } else {
                    file.visible(&state)
                        .into_iter()
                        .find(|(a, _)| a.id == p.id)
                        .map(|(a, _)| a.user)
                        .ok_or_else(|| ErrorObject::new(ErrorCode::NotFound, "no such profile"))?
                };
                let now = job::now();
                let own = caller.id.as_deref() == Some(&p.id)
                    || (p.id == "guest" && caller.role == Role::Guest);
                SwitchHint {
                    target: p.id.clone(),
                    from: caller.owner_key(),
                    expires: now + public::HINT_TTL_S,
                    token: own.then(|| public::Token {
                        user: target_user,
                        expires: now + public::HINT_TTL_S,
                    }),
                }
                .save(&self.paths.switch_json())
                .map_err(internal)?;
                info!(from = ?caller.owner_key(), to = %p.id, resume = own, "switch hint");
                public::publish(&self.paths, &state, &file, now);
                empty()
            }
            R::Unlock(p) => {
                let (state, _) = self.require_schema2()?;
                let (id, _) = self.own_profile(uid, &state)?;
                self.check_pin(uid, method, &id, Some(&p.pin), Which::Own)
                    .await?;
                empty()
            }
            R::GuestSet(p) => {
                let (state, _) = self.require_schema2()?;
                let file = self.profiles_file();
                let admin = self
                    .admin_id(&state, &file)
                    .ok_or_else(|| pin_required(Which::Admin, None, "there is no admin profile"))?;
                self.check_pin(uid, method, &admin, Some(&p.admin_pin), Which::Admin)
                    .await?;
                if state.guest_enabled() == p.enabled {
                    return job_ref(None);
                }
                self.preflight()?;
                let mut new = state.clone();
                new.settings.guest = Some(state::GuestState { enable: p.enabled });
                let new = self.requirements_update(&new).unwrap_or(new);
                let spec = self.spec(
                    JobKind::GuestSet,
                    uid,
                    Action::Apply(job::Apply {
                        state: Some(new),
                        ..Default::default()
                    }),
                );
                job_ref(Some(self.start(spec).await?))
            }
            R::StarterSet(p) => {
                let (state, _) = self.require_schema2()?;
                let file = self.profiles_file();
                let (admin, _) = file
                    .admin(&state)
                    .ok_or_else(|| pin_required(Which::Admin, None, "there is no admin profile"))?;
                self.check_pin(uid, method, &admin.id, Some(&p.pin), Which::Admin)
                    .await?;
                let admin_ids = appreg::current(
                    &Owner::Profile {
                        id: admin.id.clone(),
                        user: admin.user.clone(),
                    },
                    &self.paths,
                )
                .map(|c| c.manifest.ids)
                .unwrap_or_default();
                if let Some(id) = p.ids.iter().find(|i| !admin_ids.contains(i)) {
                    return Err(ErrorObject::with_data(
                        ErrorCode::InvalidParams,
                        format!("{id} is not installed in the admin profile"),
                        json!({ "field": "ids", "reason": "not_in_admin_profile" }),
                    ));
                }
                let ids = appreg::normalize(p.ids.iter().cloned());
                let index = self.catalog().ok();
                let wanted = index
                    .as_ref()
                    .map(|i| i.box_features(&ids))
                    .unwrap_or_default();
                let box_state = self.box_state(&state, &wanted);
                if box_state.is_some() {
                    self.preflight()?;
                }
                let by = self.caller(uid, &state).user_name();
                let spec = self.spec(
                    JobKind::StarterSet,
                    uid,
                    Action::Apps(AppsJob {
                        owner: Owner::Starter,
                        op: AppsOp::Set { ids: ids.clone() },
                        box_state,
                        terms: Vec::new(),
                        delete_data: None,
                        starter: Some(StarterFile {
                            version: appreg::STARTER_VERSION,
                            ids,
                            updated_at: Some(job::now()),
                            by: Some(by),
                        }),
                    }),
                );
                job_ref(Some(self.start(spec).await?))
            }
        }
    }

    /// Profiles (and the guest) without an app env get the starter env, one short job at a
    /// time while the box is idle (after setup_admin, a failed create, a reboot for the guest).
    pub(super) async fn ensure_envs(&self) {
        if self.jobs.lock().unwrap().busy().is_some() {
            return;
        }
        let Ok((state, _)) = State::load(&self.paths.state_json()) else {
            return;
        };
        if !state.is_schema2() || appreg::current(&Owner::Starter, &self.paths).is_none() {
            return;
        }
        let file = self.profiles_file();
        let mut owners: Vec<Owner> = file
            .visible(&state)
            .into_iter()
            .map(|(a, _)| Owner::Profile {
                id: a.id,
                user: a.user,
            })
            .collect();
        if state.guest_enabled() {
            owners.push(Owner::Guest);
        }
        for owner in owners {
            let link = owner.link(&self.paths);
            if std::fs::symlink_metadata(&link).is_ok() {
                continue;
            }
            {
                let mut tried = self.env_tried.lock().unwrap();
                if !tried.insert(owner.key()) {
                    continue;
                }
            }
            let spec = self.spec(
                JobKind::AppsInstall,
                0,
                Action::Apps(AppsJob {
                    owner: owner.clone(),
                    op: AppsOp::CopyFrom {
                        from: Owner::Starter,
                    },
                    box_state: None,
                    terms: Vec::new(),
                    delete_data: None,
                    starter: None,
                }),
            );
            match self.start(spec).await {
                Ok(id) => info!(job = %id, owner = %owner.key(), "giving the starter env"),
                Err(e) => warn!(owner = %owner.key(), "cannot give the starter env: {}", e.message),
            }
            return;
        }
    }

    fn save_runtime(&self, f: impl FnOnce(&mut Runtime)) -> Result<(), ErrorObject> {
        let path = self.paths.runtime_json();
        let mut r: Runtime = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        f(&mut r);
        crate::fsutil::write_atomic(
            &path,
            &serde_json::to_vec_pretty(&r).map_err(internal)?,
            0o600,
        )
        .map_err(internal)?;
        publish_runtime(&self.paths, &r);
        Ok(())
    }
}

/// `runtime.json`: the box's display mode and audio output (shared by every profile).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Runtime {
    /// Display name → `WxH@R`.
    #[serde(default)]
    pub display_modes: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub audio_output: Option<String>,
}

/// `/run/hinteros/gamescope-modes` and `/run/hinteros/audio-output` (0644).
pub fn publish_runtime(paths: &crate::config::Paths, r: &Runtime) {
    if !r.display_modes.is_empty() {
        // gamescope's mode save file: `<display>:WxH@R` lines.
        let text: String = r
            .display_modes
            .iter()
            .map(|(d, m)| format!("{d}:{m}\n"))
            .collect();
        let _ = crate::fsutil::write_atomic(
            &paths.run_dir.join("gamescope-modes"),
            text.as_bytes(),
            0o644,
        );
    }
    if let Some(node) = &r.audio_output {
        let _ = crate::fsutil::write_atomic(
            &paths.run_dir.join("audio-output"),
            format!("{node}\n").as_bytes(),
            0o644,
        );
    }
}

/// Re-publish what `runtime.json` holds (at start).
pub fn republish_runtime(paths: &crate::config::Paths) {
    let r: Runtime = std::fs::read(paths.runtime_json())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    publish_runtime(paths, &r);
}
