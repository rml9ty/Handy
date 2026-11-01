use crate::input::{self, EnigoState};
#[cfg(target_os = "linux")]
use crate::settings::TypingTool;
use crate::settings::{
    get_settings, AutoSubmitKey, CapitalizationMode, ClipboardHandling, OpenInputFieldKey,
    PasteMethod,
};
use enigo::{Direction, Enigo, Key, Keyboard};
use log::info;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::sync::{mpsc, Mutex, PoisonError};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[cfg(target_os = "linux")]
use crate::utils::{is_gnome_wayland, is_kde_wayland, is_wayland};

/// Serializes pastes: overlapping ones would restore each other's transcript as
/// the user's clipboard. Only taken by `paste`, which never runs on the main thread.
static PASTE_LOCK: Mutex<()> = Mutex::new(());

/// Runs `f` with the shared Enigo on the main thread (macOS TIS APIs require it)
/// and waits for the result. Runs inline if already on the main thread, so no deadlock.
fn with_enigo<T: Send + 'static>(
    app_handle: &AppHandle,
    f: impl FnOnce(&mut Enigo) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    let app = app_handle.clone();
    app_handle
        .run_on_main_thread(move || {
            let result = (|| {
                let enigo_state = app
                    .try_state::<EnigoState>()
                    .ok_or("Enigo state not initialized")?;
                let mut enigo = enigo_state
                    .0
                    .lock()
                    .map_err(|e| format!("Failed to lock Enigo: {}", e))?;
                f(&mut enigo)
            })();
            let _ = tx.send(result);
        })
        .map_err(|e| format!("Failed to run input on the main thread: {}", e))?;
    // A dropped closure (event loop shutting down) closes the channel.
    rx.recv()
        .map_err(|_| "Main thread dropped the input request".to_string())?
}

fn write_text_to_clipboard(app_handle: &AppHandle, text: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if is_wayland() && is_wl_copy_available() {
        info!("Using wl-copy for clipboard write on Wayland");
        return write_clipboard_via_wl_copy(text);
    }

    app_handle
        .clipboard()
        .write_text(text)
        .map_err(|e| format!("Failed to write to clipboard: {}", e))
}

fn finish_clipboard_paste<T>(
    paste_result: Result<T, String>,
    paste_delay_after_ms: u64,
    restore_clipboard: impl FnOnce(),
) -> Result<T, String> {
    std::thread::sleep(Duration::from_millis(paste_delay_after_ms));
    restore_clipboard();
    paste_result
}

/// Pastes text using the clipboard: saves current content, writes text, sends paste keystroke, restores clipboard.
fn paste_via_clipboard(
    text: &str,
    app_handle: &AppHandle,
    paste_method: &PasteMethod,
    paste_delay_ms: u64,
    paste_delay_after_ms: u64,
    open_sequence: Option<OpenSequence>,
    submit_key: Option<AutoSubmitKey>,
) -> Result<bool, String> {
    let clipboard = app_handle.clipboard();
    let saved_text = clipboard.read_text().ok().filter(|t| !t.is_empty());
    // Only probe for an image when there is no text to restore. Text is by far the
    // common case, and reading an image decodes the full bitmap, so this keeps the
    // text path exactly as cheap as it was before.
    let saved_image = if saved_text.is_none() {
        clipboard.read_image().ok().map(|image| image.to_owned())
    } else {
        None
    };

    // Write text to clipboard first
    write_text_to_clipboard(app_handle, text)?;

    std::thread::sleep(Duration::from_millis(paste_delay_ms));

    // Capture key injection errors so the original clipboard is restored before
    // propagating them to the caller.
    let paste_result = (|| -> Result<bool, String> {
        // Game mode: open the chat box, clear it, paste and submit in one
        // uninterrupted burst so stray key presses have no gap to land in.
        if let Some(open_sequence) = open_sequence {
            let paste_method = *paste_method;
            return with_enigo(app_handle, move |enigo| {
                open_sequence.send(enigo)?;
                match paste_method {
                    PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo, 100)?,
                    PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo, 100)?,
                    PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo, 100)?,
                    _ => return Err("Invalid paste method for clipboard paste".into()),
                }
                match submit_key {
                    Some(key) => {
                        if let Err(error) = send_return_key(enigo, key) {
                            log::warn!("Paste succeeded, but auto-submit failed: {error}");
                        }
                        Ok(true)
                    }
                    None => Ok(false),
                }
            });
        }

        // Send paste key combo
        #[cfg(target_os = "linux")]
        let key_combo_sent = try_send_key_combo_linux(paste_method)?;

        #[cfg(not(target_os = "linux"))]
        let key_combo_sent = false;

        // Fall back to enigo if no native tool handled it
        if !key_combo_sent {
            let paste_method = *paste_method;
            with_enigo(app_handle, move |enigo| match paste_method {
                // The legacy path cannot detect a mistimed chord, so it keeps the
                // conservative 100ms modifier hold.
                PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo, 100),
                PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo, 100),
                PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo, 100),
                _ => Err("Invalid paste method for clipboard paste".into()),
            })?;
        }

        Ok(false)
    })();

    finish_clipboard_paste(paste_result, paste_delay_after_ms, || {
        // Restore original clipboard content even when key injection failed.
        // Text takes priority so this path stays identical to the previous behavior;
        // an image is only restored when the clipboard held no text at all, which is
        // the case that used to silently wipe screenshots.
        if let Some(clipboard_content) = saved_text {
            let _ = write_text_to_clipboard(app_handle, &clipboard_content);
        } else if let Some(image) = saved_image {
            info!("Restoring image to clipboard");
            let _ = clipboard.write_image(&image);
        } else {
            // Nothing was there to begin with — don't leave the transcription behind.
            let _ = clipboard.clear();
        }
    })
}

/// Attempts to send a key combination using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_send_key_combo_linux(paste_method: &PasteMethod) -> Result<bool, String> {
    if is_wayland() {
        // Wayland: prefer wtype (but not on KDE or GNOME), then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        // or on GNOME/Mutter (same reason — Mutter deliberately does not implement
        // the virtual-keyboard-v1 protocol).
        if !is_kde_wayland() && !is_gnome_wayland() && is_wtype_available() {
            info!("Using wtype for key combo");
            send_key_combo_via_wtype(paste_method)?;
            return Ok(true);
        }
        if is_dotool_available() {
            info!("Using dotool for key combo");
            send_key_combo_via_dotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available() {
            info!("Using xdotool for key combo");
            send_key_combo_via_xdotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Attempts to type text directly using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_direct_typing_linux(text: &str, preferred_tool: TypingTool) -> Result<bool, String> {
    // If user specified a tool, try only that one
    if preferred_tool != TypingTool::Auto {
        return match preferred_tool {
            TypingTool::Wtype if is_wtype_available() => {
                info!("Using user-specified wtype");
                type_text_via_wtype(text)?;
                Ok(true)
            }
            TypingTool::Kwtype if is_kwtype_available() => {
                info!("Using user-specified kwtype");
                type_text_via_kwtype(text)?;
                Ok(true)
            }
            TypingTool::Dotool if is_dotool_available() => {
                info!("Using user-specified dotool");
                type_text_via_dotool(text)?;
                Ok(true)
            }
            TypingTool::Ydotool if is_ydotool_available() => {
                info!("Using user-specified ydotool");
                type_text_via_ydotool(text)?;
                Ok(true)
            }
            TypingTool::Xdotool if is_xdotool_available() => {
                info!("Using user-specified xdotool");
                type_text_via_xdotool(text)?;
                Ok(true)
            }
            _ => Err(format!(
                "Typing tool {:?} is not available on this system",
                preferred_tool
            )),
        };
    }

    // Auto mode - existing fallback chain
    if is_wayland() {
        // KDE Wayland: prefer kwtype (uses KDE Fake Input protocol, supports umlauts)
        if is_kde_wayland() && is_kwtype_available() {
            info!("Using kwtype for direct text input on KDE Wayland");
            type_text_via_kwtype(text)?;
            return Ok(true);
        }
        // Wayland: prefer wtype, then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        // or on GNOME/Mutter (same reason — Mutter deliberately does not implement
        // the virtual-keyboard-v1 protocol).
        if !is_kde_wayland() && !is_gnome_wayland() && is_wtype_available() {
            info!("Using wtype for direct text input");
            type_text_via_wtype(text)?;
            return Ok(true);
        }
        if is_dotool_available() {
            info!("Using dotool for direct text input");
            type_text_via_dotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available() {
            info!("Using xdotool for direct text input");
            type_text_via_xdotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Returns the list of available typing tools on this system.
/// Always includes "auto" as the first entry.
#[cfg(target_os = "linux")]
pub fn get_available_typing_tools() -> Vec<String> {
    let mut tools = vec!["auto".to_string()];
    if is_wtype_available() {
        tools.push("wtype".to_string());
    }
    if is_kwtype_available() {
        tools.push("kwtype".to_string());
    }
    if is_dotool_available() {
        tools.push("dotool".to_string());
    }
    if is_ydotool_available() {
        tools.push("ydotool".to_string());
    }
    if is_xdotool_available() {
        tools.push("xdotool".to_string());
    }
    tools
}

/// Check if wtype is available (Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_wtype_available() -> bool {
    Command::new("which")
        .arg("wtype")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if dotool is available (another Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_dotool_available() -> bool {
    Command::new("which")
        .arg("dotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YdotoolKeySyntax {
    Symbolic,
    RawKeycodes,
}

#[cfg(target_os = "linux")]
const YDOTOOL_UNKNOWN_HELP_FALLBACK: YdotoolKeySyntax = YdotoolKeySyntax::RawKeycodes;

#[cfg(target_os = "linux")]
static YDOTOOL_KEY_SYNTAX: OnceLock<YdotoolKeySyntax> = OnceLock::new();

/// Classifies `ydotool key --help` output without relying on version or distro metadata.
#[cfg(target_os = "linux")]
fn classify_ydotool_key_syntax(help: &str) -> Option<YdotoolKeySyntax> {
    let help = help.to_ascii_lowercase();

    // Check modern markers first in case a future help message mentions the legacy syntax.
    if help.contains("syntax: <keycode>:<pressed>")
        || help.contains("[keycodes]")
        || help.contains("using raw keycodes")
    {
        Some(YdotoolKeySyntax::RawKeycodes)
    } else if help.contains("separated by plus (+)")
        || (help.contains("<key sequence>") && help.contains("alt+r"))
    {
        Some(YdotoolKeySyntax::Symbolic)
    } else {
        None
    }
}

/// Detects and caches a recognized ydotool key syntax. Unknown or failed probes are not cached,
/// allowing a transient daemon or PATH problem to recover on a later paste attempt.
#[cfg(target_os = "linux")]
fn detect_ydotool_key_syntax() -> YdotoolKeySyntax {
    if let Some(syntax) = YDOTOOL_KEY_SYNTAX.get() {
        return *syntax;
    }

    match Command::new("ydotool").args(["key", "--help"]).output() {
        Ok(output) => {
            // ydotool 0.x writes help to stderr and its exit status varies by build.
            let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
            if !help.is_empty() && !output.stderr.is_empty() {
                help.push('\n');
            }
            help.push_str(&String::from_utf8_lossy(&output.stderr));

            if let Some(syntax) = classify_ydotool_key_syntax(&help) {
                *YDOTOOL_KEY_SYNTAX.get_or_init(|| {
                    info!("Detected ydotool key syntax: {:?}", syntax);
                    syntax
                })
            } else {
                // Preserve Handy's existing behavior and compatibility with current ydotool.
                log::warn!(
                    "Could not recognize ydotool key --help output (exit status {:?}); using raw-keycode syntax",
                    output.status.code()
                );
                YDOTOOL_UNKNOWN_HELP_FALLBACK
            }
        }
        Err(error) => {
            log::warn!(
                "Could not query ydotool key syntax: {}; using raw-keycode syntax",
                error
            );
            YDOTOOL_UNKNOWN_HELP_FALLBACK
        }
    }
}

/// Check if ydotool is available (uinput-based, works on both Wayland and X11)
#[cfg(target_os = "linux")]
fn is_ydotool_available() -> bool {
    Command::new("which")
        .arg("ydotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn is_xdotool_available() -> bool {
    Command::new("which")
        .arg("xdotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if kwtype is available (KDE Wayland virtual keyboard input tool)
#[cfg(target_os = "linux")]
fn is_kwtype_available() -> bool {
    Command::new("which")
        .arg("kwtype")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if wl-copy is available (Wayland clipboard tool)
#[cfg(target_os = "linux")]
fn is_wl_copy_available() -> bool {
    Command::new("which")
        .arg("wl-copy")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Type text directly via wtype on Wayland.
#[cfg(target_os = "linux")]
fn type_text_via_wtype(text: &str) -> Result<(), String> {
    let output = Command::new("wtype")
        .arg("--") // Protect against text starting with -
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via xdotool on X11.
#[cfg(target_os = "linux")]
fn type_text_via_xdotool(text: &str) -> Result<(), String> {
    let output = Command::new("xdotool")
        .arg("type")
        .arg("--clearmodifiers")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    // `--clearmodifiers` restores the modifiers that were held when xdotool
    // started. If the user releases one while xdotool is typing, that synthetic
    // restore can leave the modifier latched on the XTEST keyboard (#1817).
    // Release both sides of Handy's supported push-style modifiers to clear any
    // stale restore. Lock keys are intentionally excluded because key events
    // toggle them.
    //
    // The release is unconditional, so it can make a modifier that is still
    // physically held appear released until the next physical event. This is
    // preferable to leaving a synthetic modifier latched system-wide.
    //
    // Clean up before checking the typing status because xdotool may have
    // changed modifier state before returning an error. Cleanup remains
    // best-effort because the text may already have been partially or fully
    // typed, but failures are logged so they can be diagnosed.
    match Command::new("xdotool")
        .arg("keyup")
        .args([
            "Control_L",
            "Control_R",
            "Shift_L",
            "Shift_R",
            "Alt_L",
            "Alt_R",
            "Super_L",
            "Super_R",
        ])
        .output()
    {
        Ok(cleanup_output) if !cleanup_output.status.success() => {
            let stderr = String::from_utf8_lossy(&cleanup_output.stderr);
            log::warn!(
                "xdotool modifier cleanup failed with status {:?}: {}",
                cleanup_output.status.code(),
                stderr.trim()
            );
        }
        Err(error) => log::warn!("Failed to execute xdotool modifier cleanup: {}", error),
        _ => {}
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Builds the dotool stdin script that types `text`.
///
/// dotool reads one command per line, so each line of `text` gets its own
/// `type` command and line breaks are sent as `key enter`, like the other
/// typing tools do. Otherwise everything after the first newline would be run
/// as dotool commands instead of being typed.
#[cfg(target_os = "linux")]
fn dotool_type_script(text: &str) -> String {
    let mut script = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            script.push_str("key enter\n");
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.is_empty() {
            script.push_str("type ");
            script.push_str(line);
            script.push('\n');
        }
    }
    script
}

/// Type text directly via dotool (works on both Wayland and X11 via uinput).
#[cfg(target_os = "linux")]
fn type_text_via_dotool(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("dotool")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn dotool: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(dotool_type_script(text).as_bytes())
            .map_err(|e| format!("Failed to write to dotool stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for dotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("dotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via ydotool (uinput-based, requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn type_text_via_ydotool(text: &str) -> Result<(), String> {
    let output = Command::new("ydotool")
        .arg("type")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via kwtype (KDE Wayland virtual keyboard, uses KDE Fake Input protocol).
#[cfg(target_os = "linux")]
fn type_text_via_kwtype(text: &str) -> Result<(), String> {
    let output = Command::new("kwtype")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute kwtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kwtype failed: {}", stderr));
    }

    Ok(())
}

/// Write text to clipboard via wl-copy (Wayland clipboard tool).
/// Uses Stdio::null() to avoid blocking on repeated calls — wl-copy forks a
/// daemon that inherits piped fds, causing read_to_end to hang indefinitely.
#[cfg(target_os = "linux")]
fn write_clipboard_via_wl_copy(text: &str) -> Result<(), String> {
    use std::process::Stdio;
    let status = Command::new("wl-copy")
        .arg("--")
        .arg(text)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute wl-copy: {}", e))?;

    if !status.success() {
        return Err("wl-copy failed".into());
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via wtype on Wayland.
#[cfg(target_os = "linux")]
fn send_key_combo_via_wtype(paste_method: &PasteMethod) -> Result<(), String> {
    let args: Vec<&str> = match paste_method {
        PasteMethod::CtrlV => vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"],
        PasteMethod::ShiftInsert => vec!["-M", "shift", "-k", "Insert", "-m", "shift"],
        PasteMethod::CtrlShiftV => vec![
            "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
        ],
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("wtype")
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via dotool.
#[cfg(target_os = "linux")]
fn send_key_combo_via_dotool(paste_method: &PasteMethod) -> Result<(), String> {
    let command;
    match paste_method {
        PasteMethod::CtrlV => command = "echo key ctrl+v | dotool",
        PasteMethod::ShiftInsert => command = "echo key shift+insert | dotool",
        PasteMethod::CtrlShiftV => command = "echo key ctrl+shift+v | dotool",
        _ => return Err("Unsupported paste method".into()),
    }
    use std::process::Stdio;
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute dotool: {}", e))?;
    if !status.success() {
        return Err("dotool failed".into());
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn ydotool_key_args(
    paste_method: &PasteMethod,
    syntax: YdotoolKeySyntax,
) -> Result<&'static [&'static str], String> {
    let args = match (paste_method, syntax) {
        (PasteMethod::CtrlV, YdotoolKeySyntax::Symbolic) => &["key", "ctrl+v"][..],
        (PasteMethod::CtrlShiftV, YdotoolKeySyntax::Symbolic) => &["key", "ctrl+shift+v"][..],
        (PasteMethod::ShiftInsert, YdotoolKeySyntax::Symbolic) => &["key", "shift+insert"][..],
        (PasteMethod::CtrlV, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "29:1", "47:1", "47:0", "29:0"][..]
        }
        (PasteMethod::CtrlShiftV, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"][..]
        }
        (PasteMethod::ShiftInsert, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "42:1", "110:1", "110:0", "42:0"][..]
        }
        _ => return Err("Unsupported paste method".into()),
    };

    Ok(args)
}

/// Send a key combination (e.g., Ctrl+V) via ydotool (requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn send_key_combo_via_ydotool(paste_method: &PasteMethod) -> Result<(), String> {
    let syntax = detect_ydotool_key_syntax();
    let args = ydotool_key_args(paste_method, syntax)?;

    let output = Command::new("ydotool")
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via xdotool on X11.
#[cfg(target_os = "linux")]
fn send_key_combo_via_xdotool(paste_method: &PasteMethod) -> Result<(), String> {
    let key_combo = match paste_method {
        PasteMethod::CtrlV => "ctrl+v",
        PasteMethod::CtrlShiftV => "ctrl+shift+v",
        PasteMethod::ShiftInsert => "shift+Insert",
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("xdotool")
        .arg("key")
        .arg("--clearmodifiers")
        .arg(key_combo)
        .output()
        .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Pastes text by invoking an external script.
/// The script receives the text to paste as a single argument.
fn paste_via_external_script(text: &str, script_path: &str) -> Result<(), String> {
    info!("Pasting via external script: {}", script_path);

    // Do not capture the script's stdio. Wayland clipboard helpers such as
    // wl-copy may fork a background selection daemon that inherits those file
    // descriptors; waiting for captured output would then block until the
    // clipboard selection is replaced instead of returning after the script
    // itself exits.
    use std::process::Stdio;
    let status = Command::new(script_path)
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute external script '{}': {}", script_path, e))?;

    if !status.success() {
        return Err(format!(
            "External script '{}' failed with exit code {:?}",
            script_path,
            status.code()
        ));
    }

    Ok(())
}

/// Types text directly by simulating individual key presses.
fn paste_direct(
    text: &str,
    app_handle: &AppHandle,
    #[cfg(target_os = "linux")] typing_tool: TypingTool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if try_direct_typing_linux(text, typing_tool)? {
            return Ok(());
        }
        info!("Falling back to enigo for direct text input");
    }

    let text = text.to_string();
    with_enigo(app_handle, move |enigo| {
        input::paste_text_direct(enigo, &text)
    })
}

pub(crate) fn send_return_key(enigo: &mut Enigo, key_type: AutoSubmitKey) -> Result<(), String> {
    match key_type {
        AutoSubmitKey::Enter => {
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
        }
        AutoSubmitKey::CtrlEnter => {
            enigo
                .key(Key::Control, Direction::Press)
                .map_err(|e| format!("Failed to press Control key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Control, Direction::Release)
                .map_err(|e| format!("Failed to release Control key: {}", e))?;
        }
        AutoSubmitKey::CmdEnter => {
            enigo
                .key(Key::Meta, Direction::Press)
                .map_err(|e| format!("Failed to press Meta/Cmd key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Meta, Direction::Release)
                .map_err(|e| format!("Failed to release Meta/Cmd key: {}", e))?;
        }
    }

    Ok(())
}

/// Which text-insertion flavour a shortcut asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteMode {
    /// Insert the transcription as-is (after the usual output settings).
    Normal,
    /// Open chat with `/` and insert the transcription as a slash command
    /// (e.g. saying "dance" sends `/dance`).
    SlashCommand,
}

/// Maps the configured "open input field" key to a physical key. Windows and
/// macOS use virtual key codes / ANSI keycodes because many games read raw key
/// events and ignore Unicode text injection.
fn open_input_field_key(key: OpenInputFieldKey) -> Key {
    match key {
        OpenInputFieldKey::Enter => Key::Return,
        #[cfg(target_os = "windows")]
        OpenInputFieldKey::T => Key::Other(0x54), // VK_T
        #[cfg(target_os = "windows")]
        OpenInputFieldKey::Y => Key::Other(0x59), // VK_Y
        #[cfg(target_os = "windows")]
        OpenInputFieldKey::Slash => Key::Other(0xBF), // VK_OEM_2 ('/')
        #[cfg(target_os = "macos")]
        OpenInputFieldKey::T => Key::Other(17), // kVK_ANSI_T
        #[cfg(target_os = "macos")]
        OpenInputFieldKey::Y => Key::Other(16), // kVK_ANSI_Y
        #[cfg(target_os = "macos")]
        OpenInputFieldKey::Slash => Key::Other(44), // kVK_ANSI_Slash
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        OpenInputFieldKey::T => Key::Unicode('t'),
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        OpenInputFieldKey::Y => Key::Unicode('y'),
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        OpenInputFieldKey::Slash => Key::Unicode('/'),
    }
}

/// The keys sent right before the text: open the input field, wait for it to
/// appear, then optionally select everything already in it so the paste
/// replaces any stray characters.
#[derive(Debug, Clone, Copy)]
struct OpenSequence {
    key: OpenInputFieldKey,
    delay_ms: u64,
    clear: bool,
}

impl OpenSequence {
    fn from_settings(
        settings: &crate::settings::AppSettings,
        mode: PasteMode,
    ) -> Option<OpenSequence> {
        if settings.paste_method == PasteMethod::None {
            return None;
        }
        let key = match mode {
            PasteMode::SlashCommand => OpenInputFieldKey::Slash,
            PasteMode::Normal if settings.open_input_field => settings.open_input_field_key,
            PasteMode::Normal => return None,
        };
        Some(OpenSequence {
            key,
            delay_ms: settings.open_input_field_delay_ms.min(300),
            clear: settings.open_input_field_clear,
        })
    }

    fn send(&self, enigo: &mut Enigo) -> Result<(), String> {
        let key = open_input_field_key(self.key);
        enigo
            .key(key, Direction::Press)
            .map_err(|e| format!("Failed to press open input field key: {}", e))?;
        // Brief hold so games that poll keyboard state each frame still see it.
        std::thread::sleep(Duration::from_millis(30));
        enigo
            .key(key, Direction::Release)
            .map_err(|e| format!("Failed to release open input field key: {}", e))?;

        if self.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
        }

        if self.clear {
            send_select_to_start(enigo)?;
        }
        Ok(())
    }
}

/// Sends Shift+Home to select from the cursor back to the start of the line.
/// Shift+Home is used instead of Ctrl+A because the synthetic release of a
/// letter key (A) makes games think a held movement key was let go, stopping
/// e.g. a strafe; Home is practically never bound in games.
fn send_select_to_start(enigo: &mut Enigo) -> Result<(), String> {
    enigo
        .key(Key::Shift, Direction::Press)
        .map_err(|e| format!("Failed to press Shift: {}", e))?;
    let click = enigo
        .key(Key::Home, Direction::Click)
        .map_err(|e| format!("Failed to click Home: {}", e));
    std::thread::sleep(Duration::from_millis(30));
    enigo
        .key(Key::Shift, Direction::Release)
        .map_err(|e| format!("Failed to release Shift: {}", e))?;
    click
}

/// Applies the capitalization setting to normal transcriptions.
fn apply_capitalization(text: &str, mode: CapitalizationMode) -> String {
    match mode {
        CapitalizationMode::Default => text.to_string(),
        CapitalizationMode::Lowercase => text.to_lowercase(),
        CapitalizationMode::NoSentenceCaps => {
            let mut out = String::with_capacity(text.len());
            let mut sentence_start = true;
            for c in text.chars() {
                if sentence_start && c.is_alphabetic() {
                    out.extend(c.to_lowercase());
                    sentence_start = false;
                } else {
                    if matches!(c, '.' | '!' | '?') {
                        sentence_start = true;
                    } else if !c.is_whitespace() && !c.is_ascii_punctuation() {
                        sentence_start = false;
                    }
                    out.push(c);
                }
            }
            out
        }
    }
}

/// Turns a transcription like " Dance." into a slash command body ("dance").
/// `include_slash` adds the leading `/`, needed when the field was cleared
/// after `/` opened it. `digits` turns a spelled-out number at the start into
/// digits, so saying "two LFG tank" gives `/2 lfg tank` (WoW's channel
/// commands).
fn slash_command_text(text: &str, include_slash: bool, digits: bool) -> String {
    let lowered = text
        .trim()
        .trim_end_matches(|c: char| is_separator_punctuation(c) || c.is_whitespace())
        .to_lowercase();
    let command = fix_command_word(strip_slash_prefix(&lowered).unwrap_or(&lowered), digits);
    if include_slash {
        format!("/{}", command)
    } else {
        command
    }
}

/// If `text` starts like a slash command — a literal `/`, or the spoken word
/// "slash" — returns what follows it.
fn strip_slash_prefix(text: &str) -> Option<&str> {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('/') {
        return Some(rest.trim_start());
    }
    let word_end = text
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(text.len());
    if text[..word_end].eq_ignore_ascii_case("slash") {
        return Some(text[word_end..].trim_start_matches(|c: char| !c.is_alphanumeric()));
    }
    None
}

/// Cleans up the command word at the start of a slash command body: lowercase
/// it, convert a spoken number to digits, and drop any punctuation the speech
/// model attached, so "Yell, hello everyone" gives `yell hello everyone`
/// instead of the broken `yell, hello everyone`. The rest of the message keeps
/// its punctuation.
fn fix_command_word(body: &str, digits: bool) -> String {
    let body = body.trim_start();
    let word_end = body
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(body.len());
    let mut word = body[..word_end].to_lowercase();
    if digits {
        word = number_word_to_digits(&word).unwrap_or(word);
    }
    // Skip punctuation (ASCII or not) and spaces between the command and the
    // message, but keep e.g. an opening quote that starts the message.
    let rest = body[word_end..]
        .trim_start_matches(|c: char| c.is_whitespace() || is_separator_punctuation(c));
    if rest.is_empty() {
        word
    } else {
        format!("{} {}", word, rest)
    }
}

/// Punctuation a speech model may put right after a command word.
fn is_separator_punctuation(c: char) -> bool {
    matches!(
        c,
        ',' | '.'
            | '!'
            | '?'
            | ';'
            | ':'
            | '-'
            | '\u{2026}'
            | '\u{2013}'
            | '\u{2014}'
            | '\u{3001}'
            | '\u{FF0C}'
            | '\u{3002}'
    )
}

/// Normal transcriptions that happen to start with "/" or the spoken word
/// "slash" get the same command-word cleanup, so "/yell, hi" or "Slash yell.
/// Hi" become `/yell hi`.
fn fix_spoken_slash_command(text: &str, digits: bool) -> Option<String> {
    strip_slash_prefix(text).map(|body| format!("/{}", fix_command_word(body, digits)))
}

/// Maps a spelled-out number (or a common mishearing of one) to its digits:
/// "one" -> "1", "twelve" -> "12", "to" -> "2".
fn number_word_to_digits(word: &str) -> Option<String> {
    let digit = match word {
        "zero" => "0",
        "one" | "won" => "1",
        "two" | "to" | "too" => "2",
        "three" => "3",
        "four" | "for" | "fore" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" | "ate" => "8",
        "nine" => "9",
        "ten" => "10",
        "eleven" => "11",
        "twelve" => "12",
        "thirteen" => "13",
        "fourteen" => "14",
        "fifteen" => "15",
        "sixteen" => "16",
        "seventeen" => "17",
        "eighteen" => "18",
        "nineteen" => "19",
        "twenty" => "20",
        _ => return None,
    };
    Some(digit.to_string())
}

fn should_send_auto_submit(auto_submit: bool, paste_method: PasteMethod) -> bool {
    auto_submit && paste_method != PasteMethod::None
}

/// Pastes `text` into the focused app. Call from a worker thread, never the main
/// thread: it sleeps between steps (input injection hops via `with_enigo`).
pub fn paste(text: String, app_handle: AppHandle) -> Result<(), String> {
    paste_with_mode(text, app_handle, PasteMode::Normal)
}

/// Like [`paste`], but lets the caller pick a [`PasteMode`].
pub fn paste_with_mode(text: String, app_handle: AppHandle, mode: PasteMode) -> Result<(), String> {
    let _paste = PASTE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let settings = get_settings(&app_handle);
    let paste_method = settings.paste_method;
    let paste_delay_ms = settings.paste_delay_ms;
    let paste_delay_after_ms = settings.paste_delay_after_ms;

    // Optional "game mode": press a key to open the target's input field
    // (e.g. a game's chat box) right before the text is inserted.
    let open_sequence = OpenSequence::from_settings(&settings, mode);

    let text = match mode {
        PasteMode::SlashCommand => {
            // When the field is cleared, the `/` that opened it is wiped too,
            // so it has to be part of the pasted text.
            let cleared = open_sequence.map(|o| o.clear).unwrap_or(true);
            slash_command_text(&text, cleared, settings.slash_command_digits)
        }
        PasteMode::Normal => {
            let text = apply_capitalization(&text, settings.capitalization);
            // In game mode, a spoken "slash yell, hi" (often transcribed as
            // "/yell, hi") is cleaned up like the slash-command shortcut.
            let text = if settings.open_input_field {
                fix_spoken_slash_command(&text, settings.slash_command_digits).unwrap_or(text)
            } else {
                text
            };
            // Append trailing space if setting is enabled
            if settings.append_trailing_space {
                format!("{} ", text)
            } else {
                text
            }
        }
    };

    info!(
        "Using paste method: {:?}, delay before: {}ms, delay after: {}ms, open sequence: {:?}",
        paste_method, paste_delay_ms, paste_delay_after_ms, open_sequence
    );

    let send_submit = should_send_auto_submit(settings.auto_submit, paste_method);
    let mut submitted = false;

    // Perform the paste operation
    match paste_method {
        PasteMethod::None => {
            info!("PasteMethod::None selected - skipping paste action");
        }
        PasteMethod::Direct => {
            if let Some(open_sequence) = open_sequence {
                if let Err(error) = with_enigo(&app_handle, move |enigo| open_sequence.send(enigo))
                {
                    log::warn!("Failed to open input field: {error}");
                }
            }
            paste_direct(
                &text,
                &app_handle,
                #[cfg(target_os = "linux")]
                settings.typing_tool,
            )?;
        }
        PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert => {
            // Debug-gated receipt-sequenced paste (#502): restore the clipboard
            // after the target actually reads the transcript, not on a timer.
            // On success it fully handles the paste (including auto-submit and
            // clipboard handling) asynchronously; on failure fall through to
            // the legacy path untouched. Game mode always uses the legacy path
            // so the open/clear/paste/submit keys go out back to back.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if settings.reliable_paste && open_sequence.is_none() {
                // Runs entirely on the main thread, as it requires.
                let (text, app) = (text.clone(), app_handle.clone());
                let (auto_submit, auto_submit_key, clipboard_handling) = (
                    settings.auto_submit,
                    settings.auto_submit_key,
                    settings.clipboard_handling,
                );
                let reliable_result = with_enigo(&app_handle, move |enigo| {
                    crate::paste_tx::try_reliable_paste(
                        &text,
                        &app,
                        &paste_method,
                        enigo,
                        auto_submit,
                        auto_submit_key,
                        clipboard_handling,
                    )
                });
                match reliable_result {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        log::warn!("Reliable paste unavailable ({e}); falling back to legacy paste")
                    }
                }
            }
            let submit_key = send_submit.then_some(settings.auto_submit_key);
            submitted = paste_via_clipboard(
                &text,
                &app_handle,
                &paste_method,
                paste_delay_ms,
                paste_delay_after_ms,
                open_sequence,
                submit_key,
            )?;
        }
        PasteMethod::ExternalScript => {
            let script_path = settings
                .external_script_path
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or("External script path is not configured")?;
            if let Some(open_sequence) = open_sequence {
                if let Err(error) = with_enigo(&app_handle, move |enigo| open_sequence.send(enigo))
                {
                    log::warn!("Failed to open input field: {error}");
                }
            }
            paste_via_external_script(&text, script_path)?;
        }
    }

    if send_submit && !submitted {
        // In game mode, submit immediately so stray keys can't be appended.
        if open_sequence.is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
        let auto_submit_key = settings.auto_submit_key;
        if let Err(error) = with_enigo(&app_handle, move |enigo| {
            send_return_key(enigo, auto_submit_key)
        }) {
            log::warn!("Paste succeeded, but auto-submit failed: {error}");
        }
    }

    // After pasting, optionally copy to clipboard based on settings
    if settings.clipboard_handling == ClipboardHandling::CopyToClipboard {
        write_text_to_clipboard(&app_handle, &text)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_text_strips_punctuation_and_case() {
        assert_eq!(slash_command_text(" Dance.", true, true), "/dance");
        assert_eq!(slash_command_text("LOL!", false, true), "lol");
        assert_eq!(slash_command_text("/wave", true, true), "/wave");
    }

    #[test]
    fn slash_command_numbers_become_digits() {
        assert_eq!(slash_command_text("One.", true, true), "/1");
        assert_eq!(
            slash_command_text("Two, LFG tank.", true, true),
            "/2 lfg tank"
        );
        assert_eq!(
            slash_command_text("To WTS one sword", true, true),
            "/2 wts one sword"
        );
        assert_eq!(slash_command_text("Twelve", false, true), "12");
        assert_eq!(slash_command_text("One.", true, false), "/one");
        assert_eq!(slash_command_text("Dance", true, true), "/dance");
    }

    #[test]
    fn slash_command_word_never_keeps_punctuation() {
        assert_eq!(
            slash_command_text("Yell, hello everyone!", true, true),
            "/yell hello everyone"
        );
        assert_eq!(
            slash_command_text("Yell. Watch out, adds incoming.", true, false),
            "/yell watch out, adds incoming"
        );
        assert_eq!(
            slash_command_text("Guild: anyone up?", true, true),
            "/guild anyone up"
        );
        assert_eq!(
            slash_command_text("Yell\u{2026} hi", true, true),
            "/yell hi"
        );
        assert_eq!(
            slash_command_text("Yell \u{2014} hi", true, true),
            "/yell hi"
        );
        assert_eq!(
            slash_command_text("Yell, \"hi\"", true, true),
            "/yell \"hi\""
        );
    }

    #[test]
    fn spoken_slash_commands_in_normal_mode() {
        assert_eq!(
            fix_spoken_slash_command("/Yell, Hello everyone.", true).as_deref(),
            Some("/yell Hello everyone.")
        );
        assert_eq!(
            fix_spoken_slash_command("Slash yell. Hi", true).as_deref(),
            Some("/yell Hi")
        );
        assert_eq!(
            fix_spoken_slash_command("Slash two, LFG tank", true).as_deref(),
            Some("/2 LFG tank")
        );
        assert_eq!(fix_spoken_slash_command("Hello there.", true), None);
        assert_eq!(fix_spoken_slash_command("Slashing through", true), None);
    }

    #[test]
    fn capitalization_modes() {
        let text = "Hello there. I saw Bob! Ok?";
        assert_eq!(
            apply_capitalization(text, CapitalizationMode::Default),
            text
        );
        assert_eq!(
            apply_capitalization(text, CapitalizationMode::NoSentenceCaps),
            "hello there. i saw Bob! ok?"
        );
        assert_eq!(
            apply_capitalization(text, CapitalizationMode::Lowercase),
            "hello there. i saw bob! ok?"
        );
    }
    use std::cell::Cell;

    #[cfg(target_os = "linux")]
    const YDOTOOL_0_1_8_HELP: &str = r#"
Usage: key [--delay <ms>] [--key-delay <ms>] [--repeat <times>] [--repeat-delay <ms>] <key sequence> ...
Each key sequence can be any number of modifiers and keys, separated by plus (+)
For example: alt+r Alt+F4 CTRL+alt+f3 aLT+1+2+3 ctrl+Backspace
"#;

    #[cfg(target_os = "linux")]
    const YDOTOOL_1_0_4_HELP: &str = r#"
Usage: key [OPTION]... [KEYCODES]...
Since there's no way to know how many keyboard layouts are there in the world,
we're using raw keycodes now.
Syntax: <keycode>:<pressed>
e.g. 28:1 28:0 means pressing on the Enter button on a standard US keyboard.
"#;

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_ydotool_0_1_8_symbolic_help() {
        assert_eq!(
            classify_ydotool_key_syntax(YDOTOOL_0_1_8_HELP),
            Some(YdotoolKeySyntax::Symbolic)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_ydotool_1_0_4_raw_keycode_help() {
        assert_eq!(
            classify_ydotool_key_syntax(YDOTOOL_1_0_4_HELP),
            Some(YdotoolKeySyntax::RawKeycodes)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_ydotool_help_falls_back_to_raw_keycodes() {
        let syntax = classify_ydotool_key_syntax("unrecognized help output")
            .unwrap_or(YDOTOOL_UNKNOWN_HELP_FALLBACK);

        assert_eq!(syntax, YdotoolKeySyntax::RawKeycodes);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generates_symbolic_ydotool_arguments_for_all_paste_methods() {
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlV, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "ctrl+v"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlShiftV, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "ctrl+shift+v"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::ShiftInsert, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "shift+insert"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generates_raw_ydotool_arguments_for_all_paste_methods() {
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlV, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "29:1", "47:1", "47:0", "29:0"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlShiftV, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::ShiftInsert, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "42:1", "110:1", "110:0", "42:0"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dotool_script_types_single_line_text_unchanged() {
        assert_eq!(dotool_type_script("hello world"), "type hello world\n");
        assert_eq!(dotool_type_script(" indented"), "type  indented\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dotool_script_does_not_run_lines_of_text_as_commands() {
        // dotool reads one command per line; text after a line break must still
        // be typed, not executed as `key`/`click` commands.
        assert_eq!(
            dotool_type_script("first line\nkey super+l\r\nclick left\n"),
            "type first line\nkey enter\ntype key super+l\nkey enter\ntype click left\nkey enter\n"
        );
    }

    #[test]
    fn auto_submit_requires_setting_enabled() {
        assert!(!should_send_auto_submit(false, PasteMethod::CtrlV));
        assert!(!should_send_auto_submit(false, PasteMethod::Direct));
    }

    #[test]
    fn auto_submit_skips_none_paste_method() {
        assert!(!should_send_auto_submit(true, PasteMethod::None));
    }

    #[test]
    fn auto_submit_runs_for_active_paste_methods() {
        assert!(should_send_auto_submit(true, PasteMethod::CtrlV));
        assert!(should_send_auto_submit(true, PasteMethod::Direct));
        assert!(should_send_auto_submit(true, PasteMethod::CtrlShiftV));
        assert!(should_send_auto_submit(true, PasteMethod::ShiftInsert));
    }

    #[test]
    fn clipboard_is_restored_before_key_injection_error_is_returned() {
        let restored = Cell::new(false);
        let result = finish_clipboard_paste::<()>(Err("input failed".into()), 0, || {
            restored.set(true);
        });

        assert_eq!(result.unwrap_err(), "input failed");
        assert!(restored.get());
    }

    #[cfg(unix)]
    #[test]
    fn external_script_does_not_wait_for_inherited_stdio() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::thread;

        let script_path = std::env::temp_dir().join(format!(
            "handy-external-script-{}-{}.sh",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        fs::write(&script_path, "#!/bin/sh\nsleep 3 &\nexit 0\n").expect("write external script");
        let mut permissions = fs::metadata(&script_path)
            .expect("read external script metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script_path, permissions).expect("make external script executable");

        let (sender, receiver) = mpsc::channel();
        let script_path_for_thread = script_path.clone();
        thread::spawn(move || {
            let result =
                paste_via_external_script("test", script_path_for_thread.to_str().unwrap());
            sender.send(result).expect("send script result");
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("external script should return without waiting for its child");
        fs::remove_file(script_path).expect("remove external script");
        assert!(result.is_ok());
    }
}
