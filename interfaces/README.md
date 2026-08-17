# interfaces/ — the contracts

Everything two components agree on lives here, in one copy: D-Bus introspection XML (`dbus/`), private
Wayland protocols (`wayland/`), JSON schemas (`schemas/`). Code is checked against these by
`cargo xtask check-interfaces`. Changing a shipped signature means a new versioned interface, not an edit.
