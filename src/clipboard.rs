//! Put selected transcript text on the clipboard.
//!
//! rho is a terminal application, so the clipboard it can reach is the
//! terminal's: the OSC 52 escape sequence asks the terminal emulator to hold the
//! text, which also works across ssh because the sequence travels with the
//! display. Terminals that ignore OSC 52 are covered by running a clipboard
//! helper (`wl-copy`, `xclip`, `xsel`, `pbcopy`) when a desktop session is
//! actually present.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Which copy path to use; `clipboard` in config.yaml.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipboardMode {
    /// Prefer a desktop clipboard helper when one is reachable, else OSC 52.
    #[default]
    Auto,
    /// Always ask the terminal emulator (works over ssh).
    Terminal,
    /// Always run a clipboard helper program.
    System,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Env {
    pub wayland: bool,
    pub x11: bool,
    pub macos: bool,
    pub ssh: bool,
}

impl Env {
    pub fn detect() -> Self {
        let var = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        Self {
            wayland: var("WAYLAND_DISPLAY"),
            x11: var("DISPLAY"),
            macos: cfg!(target_os = "macos"),
            ssh: var("SSH_CONNECTION") || var("SSH_TTY"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send an OSC 52 sequence to the terminal.
    Terminal,
    /// Pipe the text into a helper program's stdin.
    Run(&'static [&'static str]),
    /// Nothing reachable.
    Unavailable,
}

/// Choose how a copy should happen for a given environment.
pub fn choose(mode: ClipboardMode, env: &Env, found: &dyn Fn(&str) -> bool) -> Action {
    if mode == ClipboardMode::Terminal {
        return Action::Terminal;
    }
    match helper(env, found) {
        Some(command) => Action::Run(command),
        None if mode == ClipboardMode::System => Action::Unavailable,
        None => Action::Terminal,
    }
}

fn helper(env: &Env, found: &dyn Fn(&str) -> bool) -> Option<&'static [&'static str]> {
    if env.wayland && found("wl-copy") {
        return Some(&["wl-copy"]);
    }
    if env.x11 {
        if found("xclip") {
            return Some(&["xclip", "-selection", "clipboard"]);
        }
        if found("xsel") {
            return Some(&["xsel", "--clipboard", "--input"]);
        }
    }
    // pbcopy on an ssh session would fill the far away pasteboard.
    if env.macos && !env.ssh && found("pbcopy") {
        return Some(&["pbcopy"]);
    }
    None
}

pub fn find_binary(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let pad = |i: usize| chunk.get(i).copied().unwrap_or(0);
        let n = (u32::from(chunk[0]) << 16) | (u32::from(pad(1)) << 8) | u32::from(pad(2));
        let take = |shift: u32| ALPHABET[((n >> shift) & 0x3f) as usize] as char;
        out.push(take(18));
        out.push(take(12));
        out.push(if chunk.len() > 1 { take(6) } else { '=' });
        out.push(if chunk.len() > 2 { take(0) } else { '=' });
    }
    out
}

/// The OSC 52 "set the clipboard" sequence for `text`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\u{7}", base64(text.as_bytes()))
}

/// Where the copied text ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Terminal,
    Helper(&'static str),
}

impl Placement {
    pub fn label(&self) -> &'static str {
        match self {
            Placement::Terminal => "terminal",
            Placement::Helper(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Live,
    /// Records copies without touching the terminal or spawning programs.
    Record,
}

pub struct Clipboard {
    mode: ClipboardMode,
    backend: Backend,
    history: Vec<String>,
}

impl Clipboard {
    pub fn new(mode: ClipboardMode) -> Self {
        Self {
            mode,
            backend: Backend::Live,
            history: Vec::new(),
        }
    }

    pub fn memory() -> Self {
        Self {
            mode: ClipboardMode::Auto,
            backend: Backend::Record,
            history: Vec::new(),
        }
    }

    pub fn put(&mut self, text: &str) -> Result<Placement, String> {
        self.put_with(text, &Env::detect(), &find_binary)
    }

    /// Copy `text` using an explicit environment; `put` uses the real one.
    pub fn put_with(
        &mut self,
        text: &str,
        env: &Env,
        found: &dyn Fn(&str) -> bool,
    ) -> Result<Placement, String> {
        let action = choose(self.mode, env, found);
        let placement = match &action {
            Action::Terminal => Placement::Terminal,
            Action::Run(command) => Placement::Helper(command[0]),
            Action::Unavailable => {
                return Err("no clipboard program found (try wl-clipboard, xclip or xsel)".into())
            }
        };
        if self.backend == Backend::Live {
            match action {
                Action::Terminal => write_to_terminal(text)?,
                Action::Run(command) => write_to_helper(command, text)?,
                Action::Unavailable => {}
            }
        }
        self.history.push(text.to_string());
        Ok(placement)
    }

    /// Everything this instance has copied, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn last(&self) -> Option<&str> {
        self.history.last().map(String::as_str)
    }
}

fn write_to_terminal(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    out.write_all(osc52(text).as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| format!("terminal clipboard write failed: {e}"))
}

fn write_to_helper(command: &[&str], text: &str) -> Result<(), String> {
    let Some((program, args)) = command.split_first() else {
        return Err("empty clipboard command".to_string());
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program} failed to start: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let status = child.wait().map_err(|e| format!("{program} failed: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} exited with {}",
            status.code().unwrap_or(-1)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_BINS: &[&str] = &[];

    fn finder(available: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| available.contains(&name)
    }

    fn env(wayland: bool, x11: bool, macos: bool, ssh: bool) -> Env {
        Env {
            wayland,
            x11,
            macos,
            ssh,
        }
    }

    #[test]
    fn auto_prefers_a_desktop_helper_when_a_display_is_reachable() {
        let wayland = env(true, false, false, false);
        assert_eq!(
            choose(ClipboardMode::Auto, &wayland, &finder(&["wl-copy"])),
            Action::Run(&["wl-copy"])
        );
        let x11 = env(false, true, false, false);
        assert_eq!(
            choose(ClipboardMode::Auto, &x11, &finder(&["xclip"])),
            Action::Run(&["xclip", "-selection", "clipboard"])
        );
        assert_eq!(
            choose(ClipboardMode::Auto, &x11, &finder(&["xsel"])),
            Action::Run(&["xsel", "--clipboard", "--input"])
        );
        let mac = env(false, false, true, false);
        assert_eq!(
            choose(ClipboardMode::Auto, &mac, &finder(&["pbcopy"])),
            Action::Run(&["pbcopy"])
        );
    }

    #[test]
    fn auto_falls_back_to_the_terminal_without_a_display_or_helper() {
        let bare = env(false, false, false, true);
        assert_eq!(
            choose(ClipboardMode::Auto, &bare, &finder(&["xclip", "wl-copy"])),
            Action::Terminal
        );
        let x11_without_helpers = env(false, true, false, false);
        assert_eq!(
            choose(ClipboardMode::Auto, &x11_without_helpers, &finder(NO_BINS)),
            Action::Terminal
        );
    }

    #[test]
    fn ssh_sessions_never_use_the_remote_pbcopy() {
        let mac_ssh = env(false, false, true, true);
        assert_eq!(
            choose(ClipboardMode::Auto, &mac_ssh, &finder(&["pbcopy"])),
            Action::Terminal
        );
    }

    #[test]
    fn forced_modes_override_the_probe() {
        let bare = env(false, false, false, false);
        assert_eq!(
            choose(ClipboardMode::Terminal, &bare, &finder(&["xclip"])),
            Action::Terminal
        );
        assert_eq!(
            choose(ClipboardMode::System, &bare, &finder(&["xclip"])),
            Action::Unavailable
        );
        assert_eq!(
            choose(
                ClipboardMode::System,
                &env(true, false, false, true),
                &finder(&["wl-copy"])
            ),
            Action::Run(&["wl-copy"])
        );
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("héllo →".as_bytes()), "aMOpbGxvIOKGkg==");
    }

    #[test]
    fn osc52_carries_the_selection_on_the_clipboard_buffer() {
        assert_eq!(osc52("abc"), "\x1b]52;c;YWJj\u{7}");
        assert_eq!(osc52("héllo\nworld"), "\x1b]52;c;aMOpbGxvCndvcmxk\u{7}");
    }

    #[test]
    fn record_backend_keeps_history_without_side_effects() {
        let mut clipboard = Clipboard::memory();
        assert_eq!(clipboard.last(), None);
        clipboard
            .put_with("first", &Env::default(), &finder(NO_BINS))
            .unwrap();
        clipboard
            .put_with("second", &Env::default(), &finder(NO_BINS))
            .unwrap();
        assert_eq!(
            clipboard.history(),
            &["first".to_string(), "second".to_string()]
        );
        assert_eq!(clipboard.last(), Some("second"));
    }

    #[test]
    fn missing_helper_is_reported_as_an_error() {
        let mut clipboard = Clipboard::memory();
        clipboard.mode = ClipboardMode::System;
        let err = clipboard
            .put_with("rho", &Env::default(), &finder(NO_BINS))
            .unwrap_err();
        assert!(err.contains("no clipboard program"), "{err}");
        assert!(clipboard.history().is_empty());
    }

    #[test]
    fn forced_terminal_mode_reports_the_terminal_as_the_placement() {
        let mut clipboard = Clipboard::memory();
        clipboard.mode = ClipboardMode::Terminal;
        let placement = clipboard
            .put_with("rho", &Env::default(), &finder(NO_BINS))
            .unwrap();
        assert_eq!(placement, Placement::Terminal);
        assert_eq!(placement.label(), "terminal");
    }

    #[test]
    fn binary_lookup_rejects_names_that_are_not_on_path() {
        assert!(!find_binary("rho-definitely-not-a-binary"));
    }
}
