//! Audio outputs, volume and mute through PipeWire's PulseAudio interface, with `pactl`.
//!
//! Why `pactl` and not `wpctl`: `pactl --format=json list sinks` is structured output of a
//! stable protocol (PipeWire's pulse server emulates it for every desktop), with each sink's
//! name, description, volume, mute state, ports and device properties (bus, form factor), and
//! `set-default-sink`, `set-sink-volume` and `set-sink-mute` cover the rest. `wpctl` has no
//! machine-readable listing (`wpctl status` is a tree for humans) and reports volume as text.
//! The system needs `pactl` on the daemon's PATH (`pkgs.pulseaudio`; the server is PipeWire's,
//! enabled by `services.pipewire.pulse.enable`). There is no subscription: changes made
//! elsewhere are found by polling while a client is connected, and our own changes are sent
//! at once.
//!
//! A sound card has one profile at a time, and only that profile's outputs are sinks: with
//! the analog profile on, the TV on HDMI or DisplayPort has no sink. So the outputs are the
//! sinks plus the outputs of each ALSA card's other available profiles (`list cards`), with
//! the id their sink will have (`alsa_output.<card>.<mapping>`, PipeWire's naming); picking
//! one switches the card's profile (`set-card-profile`) first.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use hinteros_api::ErrorObject;
use hinteros_api::session::{
    AudioKind, AudioMuteParams, AudioOutput, AudioSetOutputParams, AudioState, AudioVolumeParams,
    EventName, Service, VOLUME_MAX,
};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tracing::debug;

use super::Ctx;
use super::dbus::{failed, not_found, unavailable};

const SVC: Service = Service::Audio;
const PACTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `pactl` with `args`; its stdout.
async fn pactl(ctx: &Ctx, args: &[&str]) -> Result<String, ErrorObject> {
    let mut cmd = Command::new(&ctx.audio.pactl);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        unavailable(
            SVC,
            format!(
                "cannot run {} ({e}); pactl must be on the daemon's PATH",
                ctx.audio.pactl.display()
            ),
        )
    })?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let run = async {
        let (mut out, mut err) = (String::new(), String::new());
        // At most 1 MiB of either: a sink list is a few kilobytes. Both at once, so that a
        // full stderr pipe cannot stop pactl before it closes stdout.
        let (mut o, mut e) = ((&mut stdout).take(1 << 20), (&mut stderr).take(1 << 20));
        let _ = tokio::join!(o.read_to_string(&mut out), e.read_to_string(&mut err));
        let status = child.wait().await;
        (status, out, err)
    };
    let (status, out, err) = timeout(PACTL_TIMEOUT, run)
        .await
        .map_err(|_| failed("timeout", "pactl did not answer"))?;
    match status {
        Ok(s) if s.success() => Ok(out),
        Ok(_) => {
            let err = err.trim();
            // pactl cannot reach the sound server at all.
            if err.contains("Connection") || err.contains("connect") {
                Err(unavailable(SVC, err.to_owned()))
            } else {
                Err(failed("pactl_failed", format!("pactl: {err}")))
            }
        }
        Err(e) => Err(failed("pactl_failed", format!("pactl: {e}"))),
    }
}

fn percent(v: &Value) -> Option<u64> {
    let s = v.get("value_percent")?.as_str()?;
    s.trim().trim_end_matches('%').trim().parse().ok()
}

/// The loudest channel, in percent, at most [`VOLUME_MAX`].
fn volume_of(sink: &Value) -> u8 {
    let loudest = sink
        .get("volume")
        .and_then(Value::as_object)
        .map(|channels| channels.values().filter_map(percent).max())
        .unwrap_or(None);
    loudest.map_or(100, |p| p.min(u64::from(VOLUME_MAX)) as u8)
}

fn kind_of(sink: &Value) -> AudioKind {
    let name = sink["name"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let prop = |k: &str| {
        sink["properties"][k]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let port = sink["active_port"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let port_types: Vec<String> = sink["ports"]
        .as_array()
        .map(|ports| {
            ports
                .iter()
                .filter_map(|p| p["type"].as_str().map(str::to_ascii_lowercase))
                .collect()
        })
        .unwrap_or_default();
    let bus = prop("device.bus");
    let profile = prop("device.profile.name");
    if bus == "bluetooth" || name.starts_with("bluez_") || !prop("api.bluez5.address").is_empty() {
        AudioKind::Bluetooth
    } else if name.contains("hdmi")
        || port.contains("hdmi")
        || profile.contains("hdmi")
        || port.contains("displayport")
        || port_types.iter().any(|t| t == "hdmi" || t == "displayport")
    {
        AudioKind::Hdmi
    } else if bus == "usb" || name.contains(".usb-") {
        AudioKind::Usb
    } else if name.contains("analog")
        || port.contains("analog")
        || port_types
            .iter()
            .any(|t| t == "headphones" || t == "speaker" || t == "line")
    {
        AudioKind::Analog
    } else {
        AudioKind::Other
    }
}

struct Sink {
    id: String,
    name: String,
    kind: AudioKind,
    volume: u8,
    muted: bool,
}

async fn sinks(ctx: &Ctx) -> Result<Vec<Sink>, ErrorObject> {
    let out = pactl(ctx, &["--format=json", "list", "sinks"]).await?;
    let list: Vec<Value> = serde_json::from_str(&out)
        .map_err(|e| failed("bad_output", format!("pactl gave no sink list ({e})")))?;
    Ok(list
        .iter()
        .filter_map(|s| {
            let id = s["name"].as_str()?.to_owned();
            Some(Sink {
                name: s["description"]
                    .as_str()
                    .filter(|d| !d.is_empty())
                    .unwrap_or(&id)
                    .to_owned(),
                kind: kind_of(s),
                volume: volume_of(s),
                muted: s["mute"].as_bool().unwrap_or(false),
                id,
            })
        })
        .collect())
}

async fn default_sink(ctx: &Ctx) -> Result<String, ErrorObject> {
    Ok(pactl(ctx, &["get-default-sink"]).await?.trim().to_owned())
}

/// An output of a card profile that is not on: its sink appears once one of `profiles` is.
#[derive(Debug, PartialEq)]
struct Inactive {
    /// The name its sink will have: the output's id.
    id: String,
    name: String,
    kind: AudioKind,
    card: String,
    /// The available profiles with this output, the one to switch to first.
    profiles: Vec<String>,
}

/// The outputs of every ALSA card's available profiles that have no sink now (`cards` is
/// `pactl --format=json list cards`). Surround variants are left out (the stereo profile of
/// the same port plays everything), and so are `pro-audio` and `off`.
fn inactive_outputs(cards: &Value, sinks: &[&str]) -> Vec<Inactive> {
    let mut out: Vec<Inactive> = Vec::new();
    for card in cards.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(card_name) = card["name"].as_str() else {
            continue;
        };
        let Some(suffix) = card_name.strip_prefix("alsa_card.") else {
            continue;
        };
        let props = &card["properties"];
        let card_desc = props["device.description"].as_str().unwrap_or(suffix);
        let usb = props["device.bus"].as_str() == Some("usb");
        // The input the card has now ("input:analog-stereo"), kept when switching.
        let active_input = card["active_profile"]
            .as_str()
            .and_then(|p| p.split_once('+'))
            .map(|(_, input)| input);
        let Some(profiles) = card["profiles"].as_object() else {
            continue;
        };
        for (profile, info) in profiles {
            if info["available"].as_bool() != Some(true) {
                continue;
            }
            let output = profile.split('+').next().unwrap_or(profile);
            let Some(mapping) = output.strip_prefix("output:") else {
                continue;
            };
            if mapping.contains("surround") {
                continue;
            }
            let id = format!("alsa_output.{suffix}.{mapping}");
            if sinks.contains(&id.as_str()) {
                continue;
            }
            // "Digital Stereo (HDMI) Output + Analog Stereo Input" -> "Digital Stereo (HDMI)",
            // as the sink's own description will be. The output-only profile's wins ("Analog
            // Stereo Output", not "Analog Stereo Duplex").
            let desc = info["description"].as_str().unwrap_or(mapping);
            let desc = desc.split(" + ").next().unwrap_or(desc);
            let desc = desc.strip_suffix(" Output").unwrap_or(desc);
            let name = format!("{card_desc} {desc}");
            let entry = match out.iter_mut().find(|o| o.id == id) {
                Some(e) => {
                    if !profile.contains('+') {
                        e.name = name;
                    }
                    e
                }
                None => {
                    let kind = if mapping.contains("hdmi") {
                        AudioKind::Hdmi
                    } else if usb {
                        AudioKind::Usb
                    } else if mapping.contains("analog") {
                        AudioKind::Analog
                    } else {
                        AudioKind::Other
                    };
                    out.push(Inactive {
                        id,
                        name,
                        kind,
                        card: card_name.to_owned(),
                        profiles: Vec::new(),
                    });
                    out.last_mut().expect("pushed")
                }
            };
            let rank = profile_rank(profile, active_input);
            let at = entry
                .profiles
                .iter()
                .position(|p| profile_rank(p, active_input) > rank)
                .unwrap_or(entry.profiles.len());
            entry.profiles.insert(at, profile.clone());
        }
    }
    out
}

/// Which profile to switch to first: one that keeps the card's input (the microphone stays),
/// then output only, then one with another input.
fn profile_rank(profile: &str, active_input: Option<&str>) -> u8 {
    match (profile.split_once('+').map(|(_, i)| i), active_input) {
        (Some(i), Some(a)) if i == a => 0,
        (None, _) => 1,
        _ => 2,
    }
}

/// [`inactive_outputs`] now; none if the cards cannot be listed (the sinks still work).
async fn inactive(ctx: &Ctx, sinks: &[Sink]) -> Vec<Inactive> {
    let names: Vec<&str> = sinks.iter().map(|s| s.id.as_str()).collect();
    match pactl(ctx, &["--format=json", "list", "cards"]).await {
        Ok(out) => match serde_json::from_str::<Value>(&out) {
            Ok(cards) => inactive_outputs(&cards, &names),
            Err(e) => {
                debug!("pactl gave no card list ({e})");
                Vec::new()
            }
        },
        Err(e) => {
            debug!("audio cards: {}", e.message);
            Vec::new()
        }
    }
}

/// Switch the card of the inactive output `id` to its profile; whether there was one.
async fn activate(ctx: &Ctx, id: &str) -> Result<bool, ErrorObject> {
    let sinks = sinks(ctx).await?;
    let Some(output) = inactive(ctx, &sinks).await.into_iter().find(|o| o.id == id) else {
        return Ok(false);
    };
    let profile = output.profiles.first().expect("one profile at least");
    tracing::info!(card = %output.card, %profile, output = %id, "switching the sound card's profile");
    pactl(ctx, &["set-card-profile", &output.card, profile]).await?;
    Ok(true)
}

async fn read(ctx: &Ctx) -> Result<AudioState, ErrorObject> {
    let sinks = sinks(ctx).await?;
    let inactive = inactive(ctx, &sinks).await;
    let default = default_sink(ctx).await?;
    // With no default set (or one that went away), PipeWire plays on the first output.
    let current = sinks
        .iter()
        .find(|s| s.id == default)
        .or_else(|| sinks.first());
    let (volume, muted, default_id) = current.map_or((0, false, String::new()), |s| {
        (s.volume, s.muted, s.id.clone())
    });
    Ok(AudioState {
        outputs: sinks
            .iter()
            .map(|s| AudioOutput {
                default: s.id == default_id,
                id: s.id.clone(),
                name: s.name.clone(),
                kind: s.kind,
            })
            .chain(inactive.into_iter().map(|o| AudioOutput {
                default: false,
                id: o.id,
                name: o.name,
                kind: o.kind,
            }))
            .collect(),
        volume,
        muted,
    })
}

/// Send `audio_changed` if the state is not what the clients last heard.
fn publish(ctx: &Ctx, state: &AudioState) {
    let changed = {
        let mut last = ctx.audio_last.lock().unwrap_or_else(|e| e.into_inner());
        let changed = last.as_ref() != Some(state);
        *last = Some(state.clone());
        changed
    };
    if changed {
        ctx.emit(EventName::AudioChanged, state);
    }
}

/// Run a changing command, then report the new state.
async fn change(ctx: &Ctx, args: &[&str]) -> Result<AudioState, ErrorObject> {
    pactl(ctx, args).await?;
    let state = read(ctx).await?;
    publish(ctx, &state);
    Ok(state)
}

async fn default_name(ctx: &Ctx) -> Result<String, ErrorObject> {
    let state = read(ctx).await?;
    state
        .outputs
        .iter()
        .find(|o| o.default)
        .map(|o| o.id.clone())
        .ok_or_else(|| not_found("there is no audio output"))
}

pub async fn outputs(ctx: &Ctx) -> Result<AudioState, ErrorObject> {
    let state = read(ctx).await?;
    publish(ctx, &state);
    Ok(state)
}

/// How long a picked output of another card profile may take to become a sink.
const SWITCH_WAIT: Duration = Duration::from_secs(5);
const SWITCH_RETRY: Duration = Duration::from_millis(100);

pub async fn set_output(ctx: &Ctx, p: AudioSetOutputParams) -> Result<AudioState, ErrorObject> {
    let sinks = sinks(ctx).await?;
    if sinks.iter().any(|s| s.id == p.id) {
        return change(ctx, &["set-default-sink", &p.id]).await;
    }
    if !inactive(ctx, &sinks).await.iter().any(|o| o.id == p.id) {
        return Err(not_found(format!("no audio output {}", p.id)));
    }
    // Switches the card's profile, then waits for the sink.
    if make_default(ctx, &p.id, SWITCH_WAIT, SWITCH_RETRY).await {
        let state = read(ctx).await?;
        publish(ctx, &state);
        Ok(state)
    } else {
        Err(failed(
            "output_unavailable",
            format!("audio output {} did not appear", p.id),
        ))
    }
}

pub async fn volume(ctx: &Ctx, p: AudioVolumeParams) -> Result<AudioState, ErrorObject> {
    let Some(percent) = p.percent else {
        return outputs(ctx).await;
    };
    if percent > VOLUME_MAX {
        return Err(ErrorObject::invalid_field(
            "percent",
            "too_large",
            format!("invalid params: percent above {VOLUME_MAX}"),
        ));
    }
    let sink = default_name(ctx).await?;
    change(ctx, &["set-sink-volume", &sink, &format!("{percent}%")]).await
}

pub async fn mute(ctx: &Ctx, p: AudioMuteParams) -> Result<AudioState, ErrorObject> {
    let sink = default_name(ctx).await?;
    change(
        ctx,
        &["set-sink-mute", &sink, if p.muted { "1" } else { "0" }],
    )
    .await
}

/// How long the session start waits for the shared output to appear (PipeWire starting, an
/// HDMI sink after the display's modeset, Bluetooth headphones reconnecting).
const SHARED_WAIT: Duration = Duration::from_secs(60);
const SHARED_RETRY: Duration = Duration::from_secs(2);

/// The node name in the shared output file (`<node>\n`), if it is one.
pub fn shared_node(text: &str) -> Option<&str> {
    let node = text.trim();
    let ok = (1..=128).contains(&node.len())
        && node
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    ok.then_some(node)
}

/// What [`apply_shared`] needs from the sound server (the seam for its tests).
trait SharedAudio {
    /// The sinks' node names.
    async fn sinks(&self) -> Result<Vec<String>, ErrorObject>;
    async fn default_sink(&self) -> Result<String, ErrorObject>;
    async fn set_default(&self, node: &str) -> Result<(), ErrorObject>;
    /// Switch a card to the profile that has `node` ([`activate`]); whether one did.
    async fn activate(&self, node: &str) -> Result<bool, ErrorObject>;
}

impl SharedAudio for Ctx {
    async fn sinks(&self) -> Result<Vec<String>, ErrorObject> {
        Ok(sinks(self).await?.into_iter().map(|s| s.id).collect())
    }
    async fn default_sink(&self) -> Result<String, ErrorObject> {
        default_sink(self).await
    }
    async fn set_default(&self, node: &str) -> Result<(), ErrorObject> {
        change(self, &["set-default-sink", node]).await.map(|_| ())
    }
    async fn activate(&self, node: &str) -> Result<bool, ErrorObject> {
        activate(self, node).await
    }
}

/// At session start: make the box-wide output (`/run/hinteros/audio-output`, written by the
/// system service when any profile picks an output) this session's default sink, as soon as
/// that sink exists. Headphones picked in one profile are used by all; volume stays per profile.
/// Without the file, or when the sink never shows up, WirePlumber's own choice stays.
pub async fn apply_shared(ctx: Arc<Ctx>) {
    let Some(path) = ctx.audio.shared_output.clone() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        debug!(file = %path.display(), "no shared audio output");
        return;
    };
    let Some(node) = shared_node(&text).map(str::to_owned) else {
        tracing::warn!(file = %path.display(), "the shared audio output is not a node name");
        return;
    };
    make_default(&*ctx, &node, SHARED_WAIT, SHARED_RETRY).await;
}

/// Make `node` the default sink, trying every `retry` for up to `wait`; returns whether it is.
///
/// The sink can be listed before WirePlumber's default metadata is ready: `set-default-sink`
/// then fails ("Not supported") and works a moment later, or seems to work and the default
/// stays another sink. So a failure is retried like a missing sink, and a success is checked
/// by reading the default back. Only giving up is logged above debug. A node that is the
/// output of a card profile that is not on switches the card to it, once.
async fn make_default(
    audio: &impl SharedAudio,
    node: &str,
    wait: Duration,
    retry: Duration,
) -> bool {
    let give_up = tokio::time::Instant::now() + wait;
    let mut set = false;
    let mut activated = false;
    let mut last_error: Option<String> = None;
    loop {
        match audio.sinks().await {
            Ok(list) if list.iter().any(|s| s == node) => {
                if audio.default_sink().await.ok().as_deref() == Some(node) {
                    if set {
                        tracing::info!(%node, "the audio output is the default sink");
                    } else {
                        debug!(%node, "the audio output is already the default");
                    }
                    return true;
                }
                match audio.set_default(node).await {
                    Ok(()) => {
                        set = true;
                        match audio.default_sink().await {
                            Ok(default) if default == node => {
                                tracing::info!(%node, "the audio output is the default sink");
                                return true;
                            }
                            Ok(default) => {
                                debug!(%node, %default, "set the audio output, but the default is still another sink");
                                last_error = Some(format!("the default stayed {default}"));
                            }
                            Err(e) => {
                                debug!(%node, "reading the default sink: {}", e.message);
                                last_error = Some(e.message);
                            }
                        }
                    }
                    Err(e) => {
                        debug!(%node, "cannot use the audio output yet: {}", e.message);
                        last_error = Some(e.message);
                    }
                }
            }
            // Not a sink: the output of a card profile that is not on, or not there yet.
            Ok(_) if !activated => match audio.activate(node).await {
                Ok(true) => activated = true,
                Ok(false) => debug!(%node, "the audio output is not there (yet)"),
                Err(e) => {
                    debug!(%node, "cannot switch to the audio output: {}", e.message);
                    last_error = Some(e.message);
                }
            },
            Ok(_) => debug!(%node, "the audio output is not there (yet)"),
            Err(e) => debug!("audio not ready: {}", e.message),
        }
        if tokio::time::Instant::now() >= give_up {
            match last_error {
                Some(e) => tracing::warn!(%node, "cannot use the audio output: {e}"),
                None => {
                    tracing::info!(%node, "the audio output did not appear; keeping the default")
                }
            }
            return false;
        }
        sleep(retry).await;
    }
}

/// Look for changes made elsewhere (the TV's volume keys through HDMI-CEC, other apps).
pub async fn watch(ctx: Arc<Ctx>) {
    let interval = Duration::from_millis(ctx.audio.poll_ms.max(250));
    loop {
        sleep(interval).await;
        if !ctx.has_clients() || ctx.audio.poll_ms == 0 {
            continue;
        }
        match read(&ctx).await {
            Ok(state) => publish(&ctx, &state),
            Err(e) => debug!("audio poll: {}", e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// A sound server whose `set-default-sink` fails the first `failures` times (WirePlumber's
    /// default metadata not ready yet), then works.
    struct FakeAudio {
        default: Mutex<String>,
        failures: Mutex<u32>,
        /// Accepted, but the default stays (another way of not being ready).
        ignored: Mutex<u32>,
        calls: Mutex<u32>,
    }

    impl FakeAudio {
        fn new(failures: u32, ignored: u32) -> Self {
            Self {
                default: Mutex::new("hinteros_sink_a".into()),
                failures: Mutex::new(failures),
                ignored: Mutex::new(ignored),
                calls: Mutex::new(0),
            }
        }
    }

    impl SharedAudio for FakeAudio {
        async fn sinks(&self) -> Result<Vec<String>, ErrorObject> {
            Ok(vec!["hinteros_sink_a".into(), "hinteros_sink_b".into()])
        }
        async fn default_sink(&self) -> Result<String, ErrorObject> {
            Ok(self.default.lock().unwrap().clone())
        }
        async fn set_default(&self, node: &str) -> Result<(), ErrorObject> {
            *self.calls.lock().unwrap() += 1;
            let mut failures = self.failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(failed("pactl_failed", "pactl: Failure: Not supported"));
            }
            let mut ignored = self.ignored.lock().unwrap();
            if *ignored > 0 {
                *ignored -= 1;
                return Ok(());
            }
            *self.default.lock().unwrap() = node.into();
            Ok(())
        }
        async fn activate(&self, _node: &str) -> Result<bool, ErrorObject> {
            Ok(false)
        }
    }

    /// A card whose second output is a sink only after `activate`.
    struct FakeCard {
        on: Mutex<bool>,
        activations: Mutex<u32>,
        default: Mutex<String>,
    }

    impl SharedAudio for FakeCard {
        async fn sinks(&self) -> Result<Vec<String>, ErrorObject> {
            let mut list = vec!["analog".to_string()];
            if *self.on.lock().unwrap() {
                list.push("hdmi".into());
            }
            Ok(list)
        }
        async fn default_sink(&self) -> Result<String, ErrorObject> {
            Ok(self.default.lock().unwrap().clone())
        }
        async fn set_default(&self, node: &str) -> Result<(), ErrorObject> {
            *self.default.lock().unwrap() = node.into();
            Ok(())
        }
        async fn activate(&self, node: &str) -> Result<bool, ErrorObject> {
            *self.activations.lock().unwrap() += 1;
            if node != "hdmi" {
                return Ok(false);
            }
            *self.on.lock().unwrap() = true;
            Ok(true)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_output_of_another_profile_switches_the_card_once() {
        let card = FakeCard {
            on: Mutex::new(false),
            activations: Mutex::new(0),
            default: Mutex::new("analog".into()),
        };
        assert!(make_default(&card, "hdmi", SWITCH_WAIT, SWITCH_RETRY).await);
        assert_eq!(*card.default.lock().unwrap(), "hdmi");
        assert_eq!(*card.activations.lock().unwrap(), 1);
        // No card has it (yet: PipeWire still starting): asked again at every retry.
        let card = FakeCard {
            on: Mutex::new(false),
            activations: Mutex::new(0),
            default: Mutex::new("analog".into()),
        };
        assert!(!make_default(&card, "usb", SWITCH_WAIT, SWITCH_RETRY).await);
        assert_eq!(*card.activations.lock().unwrap(), 51);
        assert_eq!(*card.default.lock().unwrap(), "analog");
    }

    /// The living-room box's Intel HDA card (pactl 17 against PipeWire), analog on, the TV on
    /// HDMI 1 (DisplayPort through a cable): the TV is the output to switch to.
    #[test]
    fn outputs_of_profiles_that_are_not_on() {
        let cards: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pactl-cards-hda.json"))
                .unwrap();
        let analog = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        assert_eq!(
            inactive_outputs(&cards, &[analog]),
            vec![Inactive {
                id: "alsa_output.pci-0000_00_1f.3.hdmi-stereo".into(),
                name: "Built-in Audio Digital Stereo (HDMI)".into(),
                kind: AudioKind::Hdmi,
                card: "alsa_card.pci-0000_00_1f.3".into(),
                // The microphone input stays.
                profiles: vec![
                    "output:hdmi-stereo+input:analog-stereo".into(),
                    "output:hdmi-stereo".into()
                ],
            }]
        );
        // The other way round: HDMI on, analog offered by its own name.
        let mut cards = cards;
        cards[0]["active_profile"] = json!("output:hdmi-stereo");
        let out = inactive_outputs(&cards, &["alsa_output.pci-0000_00_1f.3.hdmi-stereo"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, analog);
        assert_eq!(out[0].name, "Built-in Audio Analog Stereo");
        assert_eq!(out[0].kind, AudioKind::Analog);
        // No input now: output only first.
        assert_eq!(
            out[0].profiles,
            vec![
                "output:analog-stereo",
                "output:analog-stereo+input:analog-stereo"
            ]
        );
        // Not ALSA, or no cards: nothing.
        assert!(
            inactive_outputs(
                &json!([{"name": "bluez_card.AA", "profiles": {"a2dp-sink": {"available": true}}}]),
                &[]
            )
            .is_empty()
        );
        assert!(inactive_outputs(&json!({}), &[]).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_shared_output_is_retried_until_the_default_takes() {
        // Fails first, then works.
        let audio = FakeAudio::new(1, 0);
        assert!(make_default(&audio, "hinteros_sink_b", SHARED_WAIT, SHARED_RETRY).await);
        assert_eq!(*audio.default.lock().unwrap(), "hinteros_sink_b");
        assert_eq!(*audio.calls.lock().unwrap(), 2);
        // Seems to work, but the default stays: tried again.
        let audio = FakeAudio::new(0, 2);
        assert!(make_default(&audio, "hinteros_sink_b", SHARED_WAIT, SHARED_RETRY).await);
        assert_eq!(*audio.calls.lock().unwrap(), 3);
        // Already the default: nothing to do.
        let audio = FakeAudio::new(0, 0);
        *audio.default.lock().unwrap() = "hinteros_sink_b".into();
        assert!(make_default(&audio, "hinteros_sink_b", SHARED_WAIT, SHARED_RETRY).await);
        assert_eq!(*audio.calls.lock().unwrap(), 0);
        // Never works: gives up after the wait (every retry tried).
        let audio = FakeAudio::new(u32::MAX, 0);
        let start = tokio::time::Instant::now();
        assert!(!make_default(&audio, "hinteros_sink_b", SHARED_WAIT, SHARED_RETRY).await);
        assert!(start.elapsed() >= SHARED_WAIT);
        assert_eq!(*audio.calls.lock().unwrap(), 31);
        assert_eq!(*audio.default.lock().unwrap(), "hinteros_sink_a");
    }

    #[test]
    fn volume_is_the_loudest_channel_capped_at_100() {
        let s = json!({"volume": {"front-left": {"value_percent": "40%"}, "front-right": {"value_percent": "42%"}}});
        assert_eq!(volume_of(&s), 42);
        let s = json!({"volume": {"mono": {"value_percent": "130%"}}});
        assert_eq!(volume_of(&s), 100);
        assert_eq!(volume_of(&json!({})), 100);
    }

    #[test]
    fn kinds() {
        let hdmi = json!({"name": "alsa_output.pci-0000_01_00.1.hdmi-stereo", "properties": {"device.bus": "pci"}});
        assert_eq!(kind_of(&hdmi), AudioKind::Hdmi);
        let bt = json!({"name": "bluez_output.AA_BB_CC_DD_EE_FF.1", "properties": {"device.bus": "bluetooth"}});
        assert_eq!(kind_of(&bt), AudioKind::Bluetooth);
        let usb = json!({"name": "alsa_output.usb-Foo-00.analog-stereo", "properties": {"device.bus": "usb"}});
        assert_eq!(kind_of(&usb), AudioKind::Usb);
        let analog = json!({"name": "alsa_output.pci-0000_00_1f.3.analog-stereo", "properties": {}, "active_port": "analog-output-headphones"});
        assert_eq!(kind_of(&analog), AudioKind::Analog);
        let by_port = json!({"name": "x", "active_port": "hdmi-output-1", "properties": {}});
        assert_eq!(kind_of(&by_port), AudioKind::Hdmi);
        assert_eq!(kind_of(&json!({"name": "null"})), AudioKind::Other);
    }
}
