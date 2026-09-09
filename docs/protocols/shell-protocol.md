# Shell ↔ compositor

Listing: upstream `ext-foreign-toplevel-list-v1` / `cosmic-toplevel-info`. Commands and sanitized identity:
private `templar_shell_v1` (`interfaces/wayland/templar-shell-v1.xml`), bound only by the compositor-launched shell.
Typed layers (`templar_window::Layer`): SecureLock, Recovery, SystemModal, ShellOverlay, Notification, AppOnTop,
Normal, HomeSurface. Recovery surfaces are compositor-owned (`templar_recovery_v1`) so they work when the shell is dead.
