# Resources (`lege-resourced`)
States: Normal → Elevated (trim background caches) → Severe (`memory.high` + proactive reclaim on the
highest-scoring victim) → Critical (checkpoint request; kill the cgroup after `emergency_kill_delay`).
Victim score weighs memory, growth, PSI contribution, foreground/visible, unsaved work, disposability —
never size alone. Protected units never qualify. Swap: real NVMe swap + bounded zswap
(`distro/policy/{sysctl.d,tmpfiles.d,kernel}`). Not covered (different recovery paths): storage stalls,
GPU reset, kernel lockup, hardware failure.
