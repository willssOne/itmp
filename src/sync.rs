//! Word-accurate lyric timing: runs `scripts/align.py` (Whisper forced alignment)
//! in its own virtualenv, one song at a time, and stores `<stem>.words.json`.

use crate::cache::home_dir;
use std::{
    collections::VecDeque,
    env, fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
};

const SCRIPT: &str = include_str!("../scripts/align.py");

extern "C" {
    fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
}

fn align_home() -> PathBuf {
    env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"))
        .join("itmp-align")
}

fn python() -> PathBuf {
    align_home().join("venv/bin/python")
}

pub fn available() -> bool {
    python().is_file()
}

pub fn words_path(dir: &Path, filename: &str) -> PathBuf {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    dir.join(format!("{stem}.words.json"))
}

pub fn needs_alignment(dir: &Path, filename: &str) -> bool {
    let words = words_path(dir, filename);
    !words.is_file() && words.with_extension("").with_extension("lrc").is_file()
}

/// Aligns one song. Returns (matched lines, total lines).
pub fn align(dir: &Path, filename: &str) -> Result<(u64, u64), String> {
    let out = words_path(dir, filename);
    let lrc = out.with_extension("").with_extension("lrc");
    let script = align_home().join("align.py");
    fs::write(&script, SCRIPT).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(python());
    cmd.arg("-I")
        .arg(&script)
        .arg(dir.join(filename))
        .arg(&lrc)
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            // Die with itmp, and stay out of the way of audio playback.
            prctl(1, 9, 0, 0, 0);
            extern "C" {
                fn nice(inc: i32) -> i32;
            }
            nice(10);
            Ok(())
        });
    }
    let run = cmd.output().map_err(|e| e.to_string())?;
    if !run.status.success() {
        let err = String::from_utf8_lossy(&run.stderr);
        return Err(err.lines().last().unwrap_or("alignment failed").to_string());
    }
    let json: serde_json::Value = fs::read_to_string(&out)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .ok_or("alignment produced no output")?;
    Ok((
        json["matched"].as_u64().unwrap_or(0),
        json["total"].as_u64().unwrap_or(0),
    ))
}

type Queue = Arc<(Mutex<VecDeque<String>>, Condvar)>;

pub struct Aligner {
    queue: Queue,
}

impl Aligner {
    pub fn start(
        dir: PathBuf,
        done: impl Fn(String, Result<(u64, u64), String>) + Send + 'static,
    ) -> Self {
        let queue: Queue = Arc::default();
        let jobs = queue.clone();
        thread::spawn(move || loop {
            let next = {
                let (lock, wake) = &*jobs;
                let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                while q.is_empty() {
                    q = wake.wait(q).unwrap_or_else(|e| e.into_inner());
                }
                q.front().cloned()
            };
            let Some(name) = next else { continue };
            if needs_alignment(&dir, &name) {
                let result = align(&dir, &name);
                done(name.clone(), result);
            }
            let (lock, _) = &*jobs;
            lock.lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|f| f != &name);
        });
        Self { queue }
    }

    /// Queues a song; the one being listened to goes right after the running job.
    pub fn request(&self, filename: &str) {
        let (lock, wake) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if q.front().map(String::as_str) != Some(filename) {
            q.retain(|f| f != filename);
            let at = q.len().min(1);
            q.insert(at, filename.to_string());
        }
        wake.notify_one();
    }

    /// Queues a song behind everything already waiting.
    pub fn request_later(&self, filename: &str) {
        let (lock, wake) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !q.iter().any(|f| f == filename) {
            q.push_back(filename.to_string());
        }
        wake.notify_one();
    }

    pub fn is_pending(&self, filename: &str) -> bool {
        let (lock, _) = &*self.queue;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|f| f == filename)
    }
}

/// `itmp setup-sync`: creates the virtualenv with faster-whisper.
pub fn setup() -> Result<(), String> {
    let home = align_home();
    fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let venv = home.join("venv");
    let run = |cmd: &mut Command| -> Result<(), String> {
        match cmd.status() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("command failed: {s}")),
            Err(e) => Err(e.to_string()),
        }
    };
    let has_uv = Command::new("uv")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok();
    if has_uv {
        if !available() {
            run(Command::new("uv").args(["venv", "--python", "3.12"]).arg(&venv))?;
        }
        run(Command::new("uv")
            .args(["pip", "install", "--python"])
            .arg(python())
            .arg("faster-whisper"))
    } else {
        if !available() {
            run(Command::new("python3").args(["-m", "venv"]).arg(&venv))?;
        }
        run(Command::new(python()).args(["-m", "pip", "install", "faster-whisper"]))
    }
}
