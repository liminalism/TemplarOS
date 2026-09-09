# Associations (`templar-associationd`)
Order: project MIME default → explicit extension override → global MIME default → generic-type extension
fallback → ask. Detected type beats extension. Choice UI distinguishes open once / always for MIME / always
for extension / always in this project / ask when extension and content disagree. `mimeapps.list` is
*generated* by the broker (import once, then own it). Stale handlers are pruned when apps are uninstalled.
