use std::path::PathBuf;
use std::process::Command;

// `mailto:` URLs can't carry attachments, and there's no simpler public
// API for "compose a message with these files attached" — AppleScript
// driving Mail.app is the standard approach. Paths are passed as
// `osascript` positional arguments (`argv`) rather than interpolated into
// the script text, so there are no escaping/injection concerns.
const SCRIPT: &str = r#"
on run argv
    tell application "Mail"
        set newMessage to make new outgoing message with properties {visible:true}
        tell newMessage
            repeat with filePath in argv
                make new attachment with properties {file name:(filePath as text)} at after the last paragraph
            end repeat
        end tell
        activate
    end tell
end run
"#;

/// Opens a new Mail.app compose window with `paths` attached.
pub fn email_files(paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("Nothing to email".to_string());
    }
    let status =
        Command::new("/usr/bin/osascript").arg("-e").arg(SCRIPT).args(paths).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Couldn't open Mail".to_string())
    }
}
