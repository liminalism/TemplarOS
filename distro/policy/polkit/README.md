`org.templaros.host.policy` — the **one** polkit action set for the one privileged daemon (ADR-0003).
Rules (`*.rules`) may relax `auth_admin_keep` for the `sudo` group on single-user machines; none shipped by default.
