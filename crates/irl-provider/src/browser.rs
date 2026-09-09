//! Opening a URL in the user's browser, without a dependency.

use std::process::{Command, Stdio};

/// Launch the system browser. Errors come back to the caller so they reach the
/// plugin's log through its own logger.
pub(crate) fn open(url: &str) -> std::io::Result<()> {
    // The URL is built from provider-supplied endpoints. Nothing legitimate in
    // one contains a quote, whitespace or a control character, and each of
    // them is a way to break out of an argument on some platform.
    if url
        .chars()
        .any(|c| c == '"' || c == '\'' || c.is_whitespace() || c.is_control())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to open a URL containing a quote, whitespace or a control character",
        ));
    }
    let mut command = platform_command(url);
    // OBS's stdio belongs to OBS: a browser that inherits it can scribble over
    // the log.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command.spawn()?;
    // The launchers below all exit at once; reap on a throwaway thread so the
    // launcher never becomes a zombie and the caller never blocks.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_command(url: &str) -> Command {
    let mut c = Command::new("open");
    c.arg(url);
    c
}

#[cfg(windows)]
fn platform_command(url: &str) -> Command {
    use std::os::windows::process::CommandExt;
    /// CREATE_NO_WINDOW: otherwise a console flashes over a fullscreen OBS.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Not `cmd /C start`. cmd re-parses its command line, and an OAuth URL is
    // full of `&` (a command separator) and `%XX` (variable expansion).
    // rundll32 hands the argument to ShellExecute untouched.
    let mut c = Command::new("rundll32");
    c.args(["url.dll,FileProtocolHandler", url])
        .creation_flags(CREATE_NO_WINDOW);
    c
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_command(url: &str) -> Command {
    // Inside the OBS Flatpak this is the portal-aware xdg-open shim, which is
    // what makes the handoff work in the sandbox at all.
    let mut c = Command::new("xdg-open");
    c.arg(url);
    c
}
