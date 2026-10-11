// Shared by the library and binary hook tests through include!: the linked library is built
// without cfg(test) for binary tests, so this fixture stays entirely outside production code.

pub(super) fn command(
    directory: &std::path::Path,
    rcfile: &std::path::Path,
    desktop: &str,
) -> std::process::Command {
    use std::os::unix::fs::PermissionsExt as _;

    // Debian Bash reads /etc/bash.bashrc even with --rcfile. Keep its installed TemplarOS
    // hooks inactive, then restore the fixture desktop before reading the checkout hook.
    // This happens during startup, before the first prompt or interactive command.
    let startup = directory.join("bash-fixture-rc");
    std::fs::write(
        &startup,
        "export XDG_CURRENT_DESKTOP=\"$TEMPLAR_TEST_DESKTOP\"\nsource \"$TEMPLAR_TEST_RCFILE\"\n",
    )
    .unwrap();

    // Delivery-only tests need no policy or journal response. Keep startup probes away from
    // the installed CLI and the person's session bus. Recording stubs remain untouched.
    let stub = directory.join("templar");
    if !stub.exists() {
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut command = std::process::Command::new("bash");
    command
        .args(["--noprofile", "--rcfile"])
        .arg(startup)
        .env("XDG_CURRENT_DESKTOP", "")
        .env("TEMPLAR_TEST_DESKTOP", desktop)
        .env("TEMPLAR_TEST_RCFILE", rcfile)
        .env("HISTFILE", directory.join("bash-history"))
        .env("PATH", format!("{}:{}", directory.display(), std::env::var("PATH").unwrap()));
    for inherited in ["BASH_ENV", "ENV", "PROMPT_COMMAND", "PS0", "PS1", "SHELLOPTS", "BASHOPTS", "TEMPLAR_NOTIFY_MIN_SECONDS"] {
        command.env_remove(inherited);
    }
    command
}
