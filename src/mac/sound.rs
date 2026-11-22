//! Optional subtle macOS audio cues when background coding agents transition
//! to `blocked`, `done`, or `error`.
//!
//! Plays through `NSSound` at a configurable subtle volume, rate-limited by a
//! global cooldown and state-transition gate so duplicate polling frames or the
//! currently active pane never chime unexpectedly.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::NSSound;
use objc2_foundation::NSString;
use rustc_hash::FxHashMap;

use crate::config::MacConfig;
use crate::term::ProgramState;

/// Curated macOS system sounds presented in Settings -> Notifications.
pub const SYSTEM_SOUNDS: &[&str] = &[
    "Tink",
    "Glass",
    "Pop",
    "Ping",
    "Purr",
    "Submarine",
    "Morse",
    "Hero",
    "Basso",
    "None",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CueKind {
    Blocked,
    Done,
    Error,
}

impl CueKind {
    pub fn from_state(state: ProgramState) -> Option<Self> {
        match state {
            ProgramState::Blocked => Some(Self::Blocked),
            ProgramState::Done => Some(Self::Done),
            ProgramState::Error => Some(Self::Error),
            ProgramState::Working | ProgramState::Idle => None,
        }
    }
}

/// Maps `CueKind` to the configured macOS sound name in `MacConfig`.
pub fn sound_name_for(kind: CueKind, cfg: &MacConfig) -> &str {
    match kind {
        CueKind::Blocked => &cfg.sound_blocked,
        CueKind::Done => &cfg.sound_done,
        CueKind::Error => &cfg.sound_error,
    }
}

/// Decides whether an agent state update represents a new background transition
/// worthy of playing an audio cue, enforcing per-block state deduplication and
/// a global cooldown between chimes.
pub struct SoundCueGate {
    last_state: FxHashMap<(String, String), ProgramState>,
    last_played: Option<Instant>,
    cooldown: Duration,
}

impl Default for SoundCueGate {
    fn default() -> Self {
        Self::new(Duration::from_millis(1200))
    }
}

impl SoundCueGate {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            last_state: FxHashMap::default(),
            last_played: None,
            cooldown,
        }
    }

    /// Evaluate a status event for `(host, block_id)`. Always updates tracked
    /// state so intermediate `Working`/`Idle` transitions re-arm future alerts.
    pub fn evaluate(
        &mut self,
        host: &str,
        block_id: &str,
        state: ProgramState,
        is_front_focused: bool,
        now: Instant,
    ) -> Option<CueKind> {
        let key = (host.to_string(), block_id.to_string());
        let prev = self.last_state.insert(key, state);
        if prev == Some(state) {
            return None;
        }
        let kind = CueKind::from_state(state)?;
        if is_front_focused {
            return None;
        }
        if let Some(last) = self.last_played {
            if now.saturating_duration_since(last) < self.cooldown {
                return None;
            }
        }
        self.last_played = Some(now);
        Some(kind)
    }
}

thread_local! {
    /// Retain the active `NSSound` on the main thread until the next cue replaces
    /// it so ARC does not drop playback mid-chime.
    static ACTIVE_SOUND: RefCell<Option<Retained<NSSound>>> = const { RefCell::new(None) };
}

/// Play a named macOS system sound (e.g. `"Tink"`, `"Glass"`, `"Basso"`) or
/// `.aiff` path at `volume` (`0.0..=1.0`). Must be called from the main thread
/// when invoking AppKit `NSSound`.
pub fn play_cue(sound_name: &str, volume: f32) -> bool {
    let trimmed = sound_name.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("off")
        || volume <= 0.001
    {
        return false;
    }

    let sound = NSSound::soundNamed(&NSString::from_str(trimmed)).or_else(|| {
        let path = if trimmed.contains('/') {
            trimmed.to_string()
        } else {
            format!("/System/Library/Sounds/{trimmed}.aiff")
        };
        if !std::path::Path::new(&path).exists() {
            return None;
        }
        NSSound::initWithContentsOfFile_byReference(
            NSSound::alloc(),
            &NSString::from_str(&path),
            true,
        )
    });
    let Some(sound) = sound else {
        return false;
    };

    ACTIVE_SOUND.with(|slot| {
        if let Some(prev) = slot.borrow_mut().take() {
            let _ = prev.stop();
        }
        sound.setVolume(volume.clamp(0.0, 1.0));
        let ok = sound.play();
        if ok {
            *slot.borrow_mut() = Some(sound);
        }
        ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_gate_deduplicates_suppresses_focus_and_enforces_cooldown() {
        let mut gate = SoundCueGate::new(Duration::from_millis(1200));
        let t0 = Instant::now();

        // Working / Idle never trigger chimes.
        assert_eq!(
            gate.evaluate("local", "b_1", ProgramState::Working, false, t0),
            None
        );

        // First transition to Blocked triggers Blocked cue.
        assert_eq!(
            gate.evaluate("local", "b_1", ProgramState::Blocked, false, t0),
            Some(CueKind::Blocked)
        );

        // Repeated Blocked state for the same block is suppressed.
        assert_eq!(
            gate.evaluate(
                "local",
                "b_1",
                ProgramState::Blocked,
                false,
                t0 + Duration::from_secs(5),
            ),
            None
        );

        // Foreground-focused transitions are suppressed even after cooldown.
        assert_eq!(
            gate.evaluate(
                "local",
                "b_2",
                ProgramState::Done,
                true,
                t0 + Duration::from_secs(6),
            ),
            None
        );

        // Transition back to Working re-arms `b_1`, then global cooldown
        // suppresses immediate back-to-back cues from another block.
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(
            gate.evaluate("local", "b_1", ProgramState::Working, false, t1),
            None
        );
        assert_eq!(
            gate.evaluate("local", "b_1", ProgramState::Done, false, t1),
            Some(CueKind::Done)
        );
        assert_eq!(
            gate.evaluate(
                "kagutsuchi",
                "b_9",
                ProgramState::Error,
                false,
                t1 + Duration::from_millis(300),
            ),
            None
        );

        // Once cooldown elapses, a new block transition plays Error.
        assert_eq!(
            gate.evaluate(
                "kagutsuchi",
                "b_8",
                ProgramState::Error,
                false,
                t1 + Duration::from_millis(1500),
            ),
            Some(CueKind::Error)
        );

        // None / zero-volume cues return false without touching AppKit playback.
        assert!(!play_cue("None", 0.5));
        assert!(!play_cue("Tink", 0.0));
    }
}
