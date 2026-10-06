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
        // At most 1 MiB of either: a sink list is a few kilobytes.
        let _ = (&mut stdout).take(1 << 20).read_to_string(&mut out).await;
        let _ = (&mut stderr).take(1 << 20).read_to_string(&mut err).await;
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

async fn read(ctx: &Ctx) -> Result<AudioState, ErrorObject> {
    let sinks = sinks(ctx).await?;
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

pub async fn set_output(ctx: &Ctx, p: AudioSetOutputParams) -> Result<AudioState, ErrorObject> {
    let sinks = sinks(ctx).await?;
    if !sinks.iter().any(|s| s.id == p.id) {
        return Err(not_found(format!("no audio output {}", p.id)));
    }
    change(ctx, &["set-default-sink", &p.id]).await
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
