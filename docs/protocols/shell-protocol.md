# Shell ↔ compositor

Listing: upstream `ext-foreign-toplevel-list-v1` / `cosmic-toplevel-info`. Commands and sanitized identity:
private `lege_shell_v1` (`interfaces/wayland/lege-shell-v1.xml`), bound only by the compositor-launched shell.
Typed layers (`lege_window::Layer`): SecureLock, Recovery, SystemModal, ShellOverlay, Notification, AppOnTop,
Normal, HomeSurface. Recovery surfaces are compositor-owned (`lege_recovery_v1`) so they work when the shell is dead.
