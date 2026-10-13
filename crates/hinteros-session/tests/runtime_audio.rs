//! Audio against a fake `pactl` (a shell script with the sinks in a state directory): the same
//! commands and JSON output as the real one for `list sinks`, `get-default-sink`,
//! `set-default-sink`, `set-sink-volume` and `set-sink-mute`. Never the desktop's PipeWire.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

mod common;

use common::*;

/// The fake: state in `<dir>/default`, `<dir>/volume-<sink>`, `<dir>/mute-<sink>`, calls
/// appended to `<dir>/calls`.
fn fake_pactl(dir: &Path) -> PathBuf {
    let state = dir.join("pactl-state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("default"),
        "alsa_output.pci-0000_01_00.1.hdmi-stereo",
    )
    .unwrap();
    for (sink, vol) in [
        ("alsa_output.pci-0000_01_00.1.hdmi-stereo", "60"),
        ("alsa_output.usb-Foo-00.analog-stereo", "30"),
        ("bluez_output.AA_BB_CC_DD_EE_02.1", "130"),
    ] {
        std::fs::write(state.join(format!("volume-{sink}")), vol).unwrap();
        std::fs::write(state.join(format!("mute-{sink}")), "no").unwrap();
    }
    let path = dir.join("pactl");
    script(
        &path,
        &format!(
            r#"S='{state}'
echo "$@" >> "$S/calls"
if [ -f "$S/broken" ]; then echo "Connection failure: Connection refused" >&2; exit 1; fi
sink_json() {{
  vol=$(cat "$S/volume-$1"); mute=$(cat "$S/mute-$1")
  case $mute in yes) m=true;; *) m=false;; esac
  echo "$2" | sed "s|@VOL@|$vol|g; s|@MUTE@|$m|g"
}}
case "$1" in
  --format=json)
    [ "$2 $3" = "list sinks" ] || exit 2
    echo '['
    sink_json alsa_output.pci-0000_01_00.1.hdmi-stereo '{{"index":50,"state":"RUNNING","name":"alsa_output.pci-0000_01_00.1.hdmi-stereo","description":"HDMI / DisplayPort","mute":@MUTE@,"volume":{{"front-left":{{"value":39321,"value_percent":"@VOL@%","db":"-13 dB"}},"front-right":{{"value":39321,"value_percent":"@VOL@%","db":"-13 dB"}}}},"active_port":"hdmi-output-0","ports":[{{"name":"hdmi-output-0","type":"HDMI"}}],"properties":{{"device.bus":"pci"}}}},'
    sink_json alsa_output.usb-Foo-00.analog-stereo '{{"index":51,"state":"IDLE","name":"alsa_output.usb-Foo-00.analog-stereo","description":"USB DAC Analog Stereo","mute":@MUTE@,"volume":{{"mono":{{"value":19660,"value_percent":"@VOL@%","db":"-31 dB"}}}},"active_port":null,"ports":[],"properties":{{"device.bus":"usb"}}}},'
    sink_json bluez_output.AA_BB_CC_DD_EE_02.1 '{{"index":52,"state":"SUSPENDED","name":"bluez_output.AA_BB_CC_DD_EE_02.1","description":"Headphones","mute":@MUTE@,"volume":{{"front-left":{{"value":85196,"value_percent":"@VOL@%","db":"7 dB"}}}},"active_port":null,"ports":[],"properties":{{"device.bus":"bluetooth","api.bluez5.address":"AA:BB:CC:DD:EE:02"}}}}'
    echo ']'
    ;;
  get-default-sink) cat "$S/default"; echo ;;
  set-default-sink) echo "$2" > "$S/default" ;;
  set-sink-volume) echo "${{3%\%}}" > "$S/volume-$2" ;;
  set-sink-mute) if [ "$3" = 1 ]; then echo yes > "$S/mute-$2"; else echo no > "$S/mute-$2"; fi ;;
  *) echo "unknown command $1" >&2; exit 2 ;;
esac"#,
            state = state.display()
        ),
    );
    path
}

fn start() -> (Daemon, PathBuf) {
    let mut state = PathBuf::new();
    let d = Daemon::start_env(
        |_| Vec::new(),
        |dir| {
            let pactl = fake_pactl(dir);
            state = dir.join("pactl-state");
            vec![("HINTEROS_PACTL".into(), pactl.into_os_string())]
        },
    );
    (d, state)
}

fn calls(state: &Path) -> Vec<String> {
    std::fs::read_to_string(state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn output<'a>(state: &'a Value, id: &str) -> &'a Value {
    state["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == id)
        .unwrap()
}

const HDMI: &str = "alsa_output.pci-0000_01_00.1.hdmi-stereo";
const USB: &str = "alsa_output.usb-Foo-00.analog-stereo";
const BT: &str = "bluez_output.AA_BB_CC_DD_EE_02.1";

#[tokio::test]
async fn outputs_volume_and_mute() {
    let (d, state) = start();
    let mut c = d.client().await;
    let s = call(&mut c, "audio.outputs", json!({})).await.unwrap();
    assert_eq!(s["volume"], 60);
    assert_eq!(s["muted"], false);
    assert_eq!(s["outputs"].as_array().unwrap().len(), 3);
    assert_eq!(
        output(&s, HDMI),
        &json!({"id": HDMI, "name": "HDMI / DisplayPort", "kind": "hdmi", "default": true})
    );
    assert_eq!(output(&s, USB)["kind"], "usb");
    assert_eq!(output(&s, USB)["default"], false);
    assert_eq!(output(&s, BT)["kind"], "bluetooth");

    // Volume: set, report, never above 100.
    let s = call(&mut c, "audio.volume", json!({"percent": 35}))
        .await
        .unwrap();
    assert_eq!(s["volume"], 35);
    assert!(calls(&state).contains(&format!("set-sink-volume {HDMI} 35%")));
    let s = call(&mut c, "audio.volume", json!({})).await.unwrap();
    assert_eq!(s["volume"], 35);
    let before = calls(&state).len();
    let e = call(&mut c, "audio.volume", json!({"percent": 150}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], -32602);
    assert_eq!(
        e["data"],
        json!({"field": "percent", "reason": "too_large"})
    );
    let e = call(&mut c, "audio.volume", json!({"percent": 101}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], -32602);
    assert!(
        calls(&state)[before..]
            .iter()
            .all(|c| !c.starts_with("set-sink-volume")),
        "a refused volume reached pactl"
    );
    let s = call(&mut c, "audio.volume", json!({"percent": 100}))
        .await
        .unwrap();
    assert_eq!(s["volume"], 100);
    let s = call(&mut c, "audio.volume", json!({"percent": 0}))
        .await
        .unwrap();
    assert_eq!(s["volume"], 0);

    let s = call(&mut c, "audio.mute", json!({"muted": true}))
        .await
        .unwrap();
    assert_eq!(s["muted"], true);
    let s = call(&mut c, "audio.mute", json!({"muted": false}))
        .await
        .unwrap();
    assert_eq!(s["muted"], false);
    assert!(calls(&state).contains(&format!("set-sink-mute {HDMI} 0")));
}

#[tokio::test]
async fn switching_outputs_follows_the_new_default() {
    let (d, state) = start();
    let mut c = d.client().await;
    let s = call(&mut c, "audio.set_output", json!({"id": BT}))
        .await
        .unwrap();
    assert!(output(&s, BT)["default"] == true && output(&s, HDMI)["default"] == false);
    // The volume shown is the new default's (130% on the sink is shown as 100).
    assert_eq!(s["volume"], 100);
    assert_eq!(
        std::fs::read_to_string(state.join("default"))
            .unwrap()
            .trim(),
        BT
    );
    // Volume and mute now go to it.
    call(&mut c, "audio.volume", json!({"percent": 20}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(state.join(format!("volume-{BT}")))
            .unwrap()
            .trim(),
        "20"
    );
    assert_eq!(
        std::fs::read_to_string(state.join(format!("volume-{HDMI}")))
            .unwrap()
            .trim(),
        "60"
    );

    // Unknown output: nothing is changed.
    let e = call(&mut c, "audio.set_output", json!({"id": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3010);
    assert!(calls(&state).iter().all(|c| !c.contains("nope")));
    // An id that looks like an option is just an unknown output.
    let e = call(&mut c, "audio.set_output", json!({"id": "--help"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3010);
}

#[tokio::test]
async fn changes_are_pushed_by_us_and_found_by_polling() {
    let (d, state) = start();
    let mut watcher = d.client().await;
    call(
        &mut watcher,
        "subscribe",
        json!({"events": ["audio_changed"]}),
    )
    .await
    .unwrap();
    let mut c = d.client().await;

    // Our own change reaches the other clients at once.
    call(&mut c, "audio.volume", json!({"percent": 45}))
        .await
        .unwrap();
    // (A poll that read the old state just before may report it first, when the daemon took
    // longer than a poll interval to start.)
    let mut ev = event_named(&mut watcher, "audio_changed").await;
    if ev["volume"] == 60 {
        ev = event_named(&mut watcher, "audio_changed").await;
    }
    assert_eq!(ev["volume"], 45);
    assert_eq!(output(&ev, HDMI)["default"], true);

    // Something else changes the volume: polling finds it.
    std::fs::write(state.join(format!("volume-{HDMI}")), "10").unwrap();
    let ev = event_named(&mut watcher, "audio_changed").await;
    assert_eq!(ev["volume"], 10);
    // And no event without a change.
    let quiet = tokio::time::timeout(Duration::from_millis(3500), watcher.next_event_raw()).await;
    assert!(quiet.is_err(), "event without a change: {quiet:?}");
}

#[tokio::test]
async fn pactl_missing_or_the_sound_server_down() {
    // The default for tests: no pactl at all.
    let d = Daemon::start();
    let mut c = d.client().await;
    for (method, params) in [
        ("audio.outputs", json!({})),
        ("audio.volume", json!({"percent": 10})),
        ("audio.mute", json!({"muted": true})),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        assert_eq!(e["code"], 4001, "{method}: {e}");
        assert_eq!(e["data"]["service"], "audio");
    }
    let e = call(&mut c, "audio.set_output", json!({"id": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4001);

    // pactl is there, the sound server is not.
    let (d, state) = start();
    std::fs::write(state.join("broken"), "").unwrap();
    let mut c = d.client().await;
    let e = call(&mut c, "audio.outputs", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4001);
    assert!(
        e["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("Connection refused")
    );
    // It comes up later.
    std::fs::remove_file(state.join("broken")).unwrap();
    call(&mut c, "audio.outputs", json!({})).await.unwrap();
}

/// Start the daemon with the box-wide output file holding `content` (written before start).
fn start_with_shared(content: Option<&str>) -> (Daemon, PathBuf) {
    let mut state = PathBuf::new();
    let d = Daemon::start_env(
        |_| Vec::new(),
        |dir| {
            let pactl = fake_pactl(dir);
            state = dir.join("pactl-state");
            let file = dir.join("audio-output");
            if let Some(c) = content {
                std::fs::write(&file, c).unwrap();
            }
            vec![
                ("HINTEROS_PACTL".into(), pactl.into_os_string()),
                ("HINTEROS_AUDIO_OUTPUT_FILE".into(), file.into_os_string()),
            ]
        },
    );
    (d, state)
}

async fn eventually_default(state: &Path, want: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = std::fs::read_to_string(state.join("default")).unwrap_or_default();
        if now.trim() == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "default stays {now:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The box-wide output (`/run/hinteros/audio-output`, any profile's choice) becomes the default
/// sink when the session starts, without any client.
#[tokio::test]
async fn the_shared_output_is_the_default_at_start() {
    let (_d, state) = start_with_shared(Some(&format!("{USB}\n")));
    eventually_default(&state, USB).await;
    assert!(calls(&state).contains(&format!("set-default-sink {USB}")));
}

/// A sink that appears later (headphones reconnecting) is still picked up while the session
/// starts; one that is already the default is left alone; no file or junk changes nothing.
#[tokio::test]
async fn the_shared_output_waits_for_its_sink() {
    let gone = "bluez_output.AA_BB_CC_DD_EE_09.1";
    let (_d, state) = start_with_shared(Some(&format!("{gone}\n")));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        std::fs::read_to_string(state.join("default"))
            .unwrap()
            .trim(),
        HDMI,
        "not there: the default stays"
    );
    // It shows up: the fake lists BT as itself, so point the file's node at BT's name by
    // making the fake answer for it (rename the sink in the fake's list).
    let pactl = state.parent().unwrap().join("pactl");
    let script = std::fs::read_to_string(&pactl).unwrap().replace(BT, gone);
    let tmp = state.parent().unwrap().join("pactl.new");
    std::fs::write(&tmp, script).unwrap();
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &pactl).unwrap();
    for f in ["volume", "mute"] {
        std::fs::copy(
            state.join(format!("{f}-{BT}")),
            state.join(format!("{f}-{gone}")),
        )
        .unwrap();
    }
    eventually_default(&state, gone).await;

    for content in [
        None,
        Some(format!("{HDMI}\n")),
        Some("x y; rm -rf /\n".to_string()),
    ] {
        let (_d, state) = start_with_shared(content.as_deref());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            calls(&state)
                .iter()
                .all(|c| !c.starts_with("set-default-sink")),
            "{content:?}: {:?}",
            calls(&state)
        );
    }
}
