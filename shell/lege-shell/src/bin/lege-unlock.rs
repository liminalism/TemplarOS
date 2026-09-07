//! `lege-unlock` — asks PAM whether a password is the person's, and says nothing else.
//!
//! The lock screen runs in `lege-shell`, and `lege-shell` is a long-lived process holding a
//! Wayland connection, a tokio runtime, an image decoder and a font stack. PAM is not something
//! to link into that: it loads arbitrary modules out of `/etc/pam.d` into the process that calls
//! it, and those modules keep credentials in memory. So authentication happens here instead, in
//! a program that exists for one question and exits.
//!
//! **The password arrives on stdin, never as an argument.** An argument would be in
//! `/proc/<pid>/cmdline`, which every process on the machine can read. One line, up to the first
//! newline; the trailing newline is not part of the password.
//!
//! The answer is the exit status and nothing more: `0` for authenticated, `1` for not. Nothing
//! is printed on success, and what is printed on failure goes to stderr for the journal, never
//! to stdout — a caller reading stdout for a verdict would be reading text this program does not
//! promise.
//!
//! This runs unprivileged, as the person whose session is locked, which is why it can be part of
//! the shell's own package rather than something setuid: `pam_unix` reaches the shadow file
//! through `pam_unix_auth`'s own helper, which is where that privilege belongs.

use std::io::Read as _;

/// The PAM service name, and the file that configures it: `/etc/pam.d/lege-unlock`, shipped by
/// the same package as this binary.
const SERVICE: &str = "lege-unlock";

fn main() -> std::process::ExitCode {
    let Some(user) = std::env::var("USER").ok().filter(|user| !user.is_empty()) else {
        eprintln!("lege-unlock: USER is not set; there is nobody to authenticate");
        return std::process::ExitCode::FAILURE;
    };

    let mut password = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut password) {
        eprintln!("lege-unlock: could not read the password from stdin: {error}");
        return std::process::ExitCode::FAILURE;
    }
    // One line. A password cannot contain a newline (nothing can type one into the field), so
    // everything from the first newline on is the caller's terminator, not the secret.
    let password = password.split('\n').next().unwrap_or_default().to_owned();
    if password.is_empty() {
        eprintln!("lege-unlock: no password on stdin");
        return std::process::ExitCode::FAILURE;
    }

    match authenticate(&user, &password) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // The reason goes to the journal, not to the caller: "wrong password" and "PAM is
            // misconfigured" look identical to whoever is at the lock screen, and that is
            // deliberate — telling them apart tells an attacker which one they hit.
            eprintln!("lege-unlock: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// One PAM conversation: `pam_authenticate`, then `pam_acct_mgmt`.
///
/// The account check matters as much as the password one. A correct password for an expired or
/// disabled account is still not a reason to unlock a screen, and `pam_authenticate` alone would
/// say yes to it.
fn authenticate(user: &str, password: &str) -> Result<(), String> {
    let mut context = pam_client::Context::new(
        SERVICE,
        Some(user),
        pam_client::conv_mock::Conversation::with_credentials(user, password),
    )
    .map_err(|error| format!("could not start a PAM context for {SERVICE}: {error}"))?;

    context
        .authenticate(pam_client::Flag::DISALLOW_NULL_AUTHTOK)
        .map_err(|error| format!("authentication failed: {error}"))?;
    context
        .acct_mgmt(pam_client::Flag::DISALLOW_NULL_AUTHTOK)
        .map_err(|error| format!("the account is not permitted to log in: {error}"))?;
    Ok(())
}
