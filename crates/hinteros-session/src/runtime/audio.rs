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
/// by reading the default back. Only giving up is logged above debug.
async fn make_default(
    audio: &impl SharedAudio,
    node: &str,
    wait: Duration,
    retry: Duration,
) -> bool {
    let give_up = tokio::time::Instant::now() + wait;
    let mut set = false;
    let mut last_error: Option<String> = None;
    loop {
        match audio.sinks().await {
            Ok(list) if list.iter().any(|s| s == node) => {
                if audio.default_sink().await.ok().as_deref() == Some(node) {
                    if set {
                        tracing::info!(%node, "the shared audio output is the default sink");
                    } else {
                        debug!(%node, "the shared audio output is already the default");
                    }
                    return true;
                }
                match audio.set_default(node).await {
                    Ok(()) => {
                        set = true;
                        match audio.default_sink().await {
                            Ok(default) if default == node => {
                                tracing::info!(%node, "the shared audio output is the default sink");
                                return true;
                            }
                            Ok(default) => {
                                debug!(%node, %default, "set the shared audio output, but the default is still another sink");
                                last_error = Some(format!("the default stayed {default}"));
                            }
                            Err(e) => {
                                debug!(%node, "reading the default sink: {}", e.message);
                                last_error = Some(e.message);
                            }
                        }
                    }
                    Err(e) => {
                        debug!(%node, "cannot use the shared audio output yet: {}", e.message);
                        last_error = Some(e.message);
                    }
                }
            }
            Ok(_) => debug!(%node, "the shared audio output is not there (yet)"),
            Err(e) => debug!("audio not ready: {}", e.message),
        }
        if tokio::time::Instant::now() >= give_up {
            match last_error {
                Some(e) => tracing::warn!(%node, "cannot use the shared audio output: {e}"),
                None => {
                    tracing::info!(%node, "the shared audio output did not appear; keeping the default")
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
