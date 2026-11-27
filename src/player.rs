use crate::cache::runtime_dir;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

#[cfg(target_os = "linux")]
extern "C" {
    fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
    fn getppid() -> i32;
    fn _exit(status: i32) -> !;
}

#[derive(Debug, Default, Clone)]
pub struct PlaybackState {
    pub active: bool,
    pub paused: bool,
    pub pos: f64,
    pub dur: f64,
    pub idle: bool,
}

pub struct MpvPlayer {
    sock_path: PathBuf,
    child: Option<Child>,
    pub volume: i32,
    pub current_file: Option<String>,
}

impl MpvPlayer {
    pub fn new() -> Self {
        let pid = std::process::id();
        let sock_path = runtime_dir().join(format!("itmp-player-{pid}.sock"));
        Self {
            sock_path,
            child: None,
            volume: 65,
            current_file: None,
        }
    }

    fn ensure_started(&mut self) -> Result<(), String> {
        if let Some(ch) = self.child.as_mut() {
            if ch.try_wait().ok().flatten().is_none() && self.sock_path.exists() {
                return Ok(());
            }
        }
        let _ = fs::remove_file(&self.sock_path);
        let parent_pid = std::process::id() as i32;
        let mut cmd = Command::new("mpv");
        cmd.args([
            "--no-video",
            "--no-terminal",
            "--really-quiet",
            "--audio-display=no",
            "--idle=yes",
            "--ao=pipewire",
            "--audio-channels=stereo",
            &format!("--volume={}", self.volume),
            &format!("--input-ipc-server={}", self.sock_path.display()),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

        // Guarantee mpv dies IMMEDIATELY if itmp exits for ANY reason
        // (q, Ctrl+C, closing terminal window / SIGHUP, SIGTERM, SIGKILL, crash).
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(move || {
                const PR_SET_PDEATHSIG: i32 = 1;
                const SIGKILL: usize = 9;
                prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0);
                if getppid() != parent_pid {
                    _exit(1);
                }
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("failed to launch mpv: {e}"))?;
        self.child = Some(child);
        for _ in 0..30 {
            if self.sock_path.exists() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(30));
        }
        Err("mpv IPC socket did not initialize".into())
    }

    fn send_cmd(&self, cmd: Value) -> Option<Value> {
        let mut stream = UnixStream::connect(&self.sock_path).ok()?;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(120)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(120)));
        let req = json!({ "command": cmd, "request_id": 1 });
        let line = format!("{req}\n");
        stream.write_all(line.as_bytes()).ok()?;

        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        for _ in 0..8 {
            buf.clear();
            if reader.read_line(&mut buf).ok()? == 0 {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&buf) {
                if v.get("request_id").and_then(|r| r.as_i64()) == Some(1) {
                    return Some(v);
                }
            }
        }
        None
    }

    pub fn play_file(&mut self, full_path: &Path, filename: &str) -> Result<(), String> {
        self.ensure_started()?;
        let path_str = full_path.to_str().ok_or("invalid path")?;
        self.send_cmd(json!(["loadfile", path_str, "replace"]));
        self.send_cmd(json!(["set_property", "volume", self.volume]));
        self.send_cmd(json!(["set_property", "pause", false]));
        self.current_file = Some(filename.to_string());
        Ok(())
    }

    pub fn toggle_pause(&self) {
        if self.sock_path.exists() {
            let _ = self.send_cmd(json!(["cycle", "pause"]));
        }
    }

    pub fn seek_rel(&self, delta_sec: f64) {
        if self.sock_path.exists() {
            let _ = self.send_cmd(json!(["seek", delta_sec, "relative"]));
        }
    }

    pub fn seek_percent(&self, pct: f64) {
        if self.sock_path.exists() {
            let p = pct.clamp(0.0, 100.0);
            let _ = self.send_cmd(json!(["seek", p, "absolute-percent"]));
        }
    }

    pub fn set_volume(&mut self, vol: i32) {
        self.volume = vol.clamp(0, 100);
        if self.sock_path.exists() {
            let _ = self.send_cmd(json!(["set_property", "volume", self.volume]));
        }
    }

    pub fn poll_state(&self) -> PlaybackState {
        if self.child.is_none() || !self.sock_path.exists() {
            return PlaybackState::default();
        }
        // Batch all 4 property queries over ONE UnixStream connection for low-jitter sync
        let Ok(mut stream) = UnixStream::connect(&self.sock_path) else {
            return PlaybackState::default();
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(80)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(80)));

        let batch = concat!(
            "{\"command\":[\"get_property\",\"idle-active\"],\"request_id\":1}\n",
            "{\"command\":[\"get_property\",\"pause\"],\"request_id\":2}\n",
            "{\"command\":[\"get_property\",\"playback-time\"],\"request_id\":3}\n",
            "{\"command\":[\"get_property\",\"duration\"],\"request_id\":4}\n"
        );
        if stream.write_all(batch.as_bytes()).is_err() {
            return PlaybackState::default();
        }

        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        let mut idle = true;
        let mut paused = true;
        let mut pos = 0.0f64;
        let mut dur = 0.0f64;
        let mut seen = 0u8;

        for _ in 0..24 {
            if seen == 0b1111 {
                break;
            }
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Ok(v) = serde_json::from_str::<Value>(&buf) {
                let ok = v.get("error").and_then(|e| e.as_str()) == Some("success");
                match v.get("request_id").and_then(|r| r.as_i64()) {
                    Some(1) => {
                        seen |= 0b0001;
                        if ok {
                            idle = v.get("data").and_then(|d| d.as_bool()).unwrap_or(true);
                        }
                    }
                    Some(2) => {
                        seen |= 0b0010;
                        if ok {
                            paused = v.get("data").and_then(|d| d.as_bool()).unwrap_or(true);
                        }
                    }
                    Some(3) => {
                        seen |= 0b0100;
                        if ok {
                            pos = v.get("data").and_then(|d| d.as_f64()).unwrap_or(0.0);
                        }
                    }
                    Some(4) => {
                        seen |= 0b1000;
                        if ok {
                            dur = v.get("data").and_then(|d| d.as_f64()).unwrap_or(0.0);
                        }
                    }
                    _ => {}
                }
            }
        }

        PlaybackState {
            active: !idle,
            paused,
            pos,
            dur,
            idle,
        }
    }

    pub fn stop(&mut self) {
        if self.sock_path.exists() {
            let _ = self.send_cmd(json!(["quit"]));
        }
        if let Some(mut ch) = self.child.take() {
            let _ = ch.kill();
            let _ = ch.wait();
        }
        let _ = fs::remove_file(&self.sock_path);
        self.current_file = None;
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}
