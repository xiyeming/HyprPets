//! 极轻量音效引擎（rodio）。
//!
//! - 每个音效在独立线程解码播放，主线程零阻塞
//! - 无音频设备 / 解码失败时静默降级
//! - 内置 pop 音首次运行时生成到 ~/.config/hyprpet/sounds/pop.wav

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use rodio::{Decoder, Sink};

pub struct Audio {
    /// 音频输出流必须保活（drop 会停止播放）
    _stream: Option<rodio::OutputStream>,
    mixer: Option<rodio::mixer::Mixer>,
}

impl Default for Audio {
    fn default() -> Self {
        Audio::new()
    }
}

impl Audio {
    pub fn new() -> Audio {
        let stream = rodio::OutputStreamBuilder::open_default_stream().ok();
        if stream.is_none() {
            eprintln!("[hyprpet] 无可用音频输出，音效禁用");
        }
        let mixer = stream.as_ref().map(|s| s.mixer().clone());
        Audio { _stream: stream, mixer }
    }

    /// 非阻塞播放（文件不存在 / 解码失败静默）
    pub fn play(&self, path: &Path) {
        let Some(mixer) = self.mixer.clone() else { return };
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            let Ok(file) = fs::File::open(&path) else { return };
            let Ok(decoder) = Decoder::new(BufReader::new(file)) else { return };
            let sink = Sink::connect_new(&mixer);
            sink.set_volume(0.6);
            sink.append(decoder);
            sink.sleep_until_end();
        });
    }
}

/// 内置音效目录
pub fn sounds_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("hyprpet").join("sounds")
}

/// 内置音效集：(文件名, [(频率Hz, 时长s)], 音符间隔s)
const SOUNDS: &[(&str, &[(f32, f32)], f32)] = &[
    ("pop", &[(880.0, 0.07), (1568.0, 0.08)], 0.005),       // 通用提示
    ("success", &[(523.0, 0.09), (659.0, 0.09), (784.0, 0.15)], 0.02), // 上行琶音
    ("warn", &[(740.0, 0.12), (520.0, 0.2)], 0.05),         // 警告双音
    ("error", &[(233.0, 0.16), (196.0, 0.26)], 0.04),       // 错误低音
    ("confirm", &[(1180.0, 0.08)], 0.0),                    // 确认清脆
];

/// 首次运行生成内置提示音
pub fn ensure_builtin_sounds() {
    let dir = sounds_dir();
    fs::create_dir_all(&dir).ok();
    for (name, notes, gap) in SOUNDS {
        let path = dir.join(format!("{name}.wav"));
        if path.exists() {
            continue;
        }
        if let Ok(wav) = make_wav(notes, *gap) {
            fs::write(path, wav).ok();
        }
    }
}

/// 播放内置音效（按名，缺失则静默）
pub fn play_named(audio: &Audio, name: &str) {
    audio.play(&sounds_dir().join(format!("{name}.wav")));
}

/// 多音合成（16-bit PCM 单声道 22050Hz，指数衰减）
fn make_wav(notes: &[(f32, f32)], gap: f32) -> std::result::Result<Vec<u8>, ()> {
    const SR: u32 = 22050;
    let mut data = Vec::new();
    for &(freq, dur) in notes {
        let n = (SR as f32 * dur) as usize;
        for i in 0..n {
            let t = i as f32 / SR as f32;
            let phase = 2.0 * std::f32::consts::PI * freq * t;
            let env = (-t / (dur * 0.5)).exp();
            let sample = (phase.sin() * env * 0.5).clamp(-1.0, 1.0);
            data.extend_from_slice(&((sample * i16::MAX as f32) as i16).to_le_bytes());
        }
        let gaps = (SR as f32 * gap) as usize;
        data.extend(std::iter::repeat(0u8).take(gaps * 2));
    }
    let mut wav = Vec::with_capacity(44 + data.len());
    let push = |w: &mut Vec<u8>, bytes: &[u8]| w.extend_from_slice(bytes);
    push(&mut wav, b"RIFF");
    push(&mut wav, &((36 + data.len()) as u32).to_le_bytes());
    push(&mut wav, b"WAVE");
    push(&mut wav, b"fmt ");
    push(&mut wav, &16u32.to_le_bytes());
    push(&mut wav, &1u16.to_le_bytes());
    push(&mut wav, &1u16.to_le_bytes());
    push(&mut wav, &SR.to_le_bytes());
    push(&mut wav, &(SR * 2).to_le_bytes());
    push(&mut wav, &2u16.to_le_bytes());
    push(&mut wav, &16u16.to_le_bytes());
    push(&mut wav, b"data");
    push(&mut wav, &(data.len() as u32).to_le_bytes());
    push(&mut wav, &data);
    Ok(wav)
}
