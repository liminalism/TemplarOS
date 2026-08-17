Greeter/display-manager choice (ADR-0010): `greetd` + the `cosmic-greeter` fork (it reuses the compositor,
so no second toolkit/session stack). Lock screen is compositor-core `ext-session-lock-v1` — the
`lock-screen` provider role has one provider by construction.
