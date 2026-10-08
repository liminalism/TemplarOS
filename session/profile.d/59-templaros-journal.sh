# Shared Bash/zsh journal helpers, explicitly sourced by the interactive garden hook.
# Startup may fork once for private directory setup; command/prompt writes never fork.
case ":${XDG_CURRENT_DESKTOP:-}:" in *:TemplarOS:*|*:templaros:*) ;; *) return 0 ;; esac
case $- in *i*) ;; *) return 0 ;; esac
if [ -z "${BASH_VERSION:-}" ] && [ -z "${ZSH_VERSION:-}" ]; then return 0; fi
if [ -n "${__templaros_journal_loaded:-}" ]; then return 0; fi
__templaros_journal_loaded=1
__templaros_journal_prefix=$(templar garden journal-init "$$" 2>/dev/null) || __templaros_journal_prefix=
__templaros_journal_seq=0
if [ -n "$__templaros_journal_prefix" ]; then
    __templaros_journal_start_umask=$(umask)
    umask 077
    : > "$__templaros_journal_prefix.umask"
    umask "$__templaros_journal_start_umask"
    unset __templaros_journal_start_umask
fi

__templaros_journal_label() {
    local text="$1" program verb rest
    __templaros_journal_label_result=
    # Privacy independent of shell history settings: never persist a leading-space line.
    case "$text" in ' '*|$'\t'*) return 0 ;; esac
    while [ -n "$text" ]; do
        read -r program verb rest <<< "$text"
        case "$program" in *=*|env|command|sudo|time|nice|nohup) text="$verb${rest:+ $rest}"; continue ;; esac
        break
    done
    program="${program##*/}"
    case "$program" in
        cargo|git|npm|pnpm|yarn|claude|codex|gemini|bash|zsh|fish|sh|make|cmake|ninja|python|python3|node|go|rustc|nvim|vim|code|ls|cd|true|false|sleep|echo|printf) ;;
        *) program=command ;;
    esac
    case "$program:$verb" in
        cargo:build|cargo:test|cargo:check|cargo:run|cargo:clippy|cargo:fmt|cargo:bench|cargo:doc|cargo:clean|git:status|git:diff|git:log|git:commit|git:rebase|git:merge|git:fetch|git:pull|git:push|git:checkout|git:switch|git:restore|git:add|npm:run|npm:test|npm:build|npm:install|pnpm:run|pnpm:test|pnpm:build|pnpm:install|yarn:run|yarn:test|yarn:build|yarn:install)
            __templaros_journal_label_result="$program $verb" ;;
        *) __templaros_journal_label_result="$program" ;;
    esac
}
__templaros_journal_encode() {
    __templaros_journal_encoded="$1"
    __templaros_journal_encoded="${__templaros_journal_encoded//%/%25}"
    __templaros_journal_encoded="${__templaros_journal_encoded//$'\t'/%09}"
    __templaros_journal_encoded="${__templaros_journal_encoded//$'\n'/%0A}"
    __templaros_journal_encoded="${__templaros_journal_encoded//$'\r'/%0D}"
}
__templaros_journal_write() {
    # status and pipeline vector were captured before any other prompt hook ran.
    local raw="$1" exit_status="$2" elapsed="$3" pipeline="$4"
    [ -n "$__templaros_journal_prefix" ] || return 0
    __templaros_journal_label "$raw"
    [ -n "$__templaros_journal_label_result" ] || return 0
    local uptime ignored seconds fraction timestamp directory full=
    read -r uptime ignored < /proc/uptime || return 0
    seconds="${uptime%.*}"; fraction="${uptime#*.}000"; fraction="${fraction:0:3}"
    timestamp=$((seconds * 1000 + 10#$fraction))
    __templaros_journal_encode "$PWD"; directory="$__templaros_journal_encoded"
    if [ "${TEMPLAR_GARDEN_FULL_COMMANDS:-0}" = 1 ]; then
        __templaros_journal_encode "${raw:0:2048}"; full="$__templaros_journal_encoded"
    fi
    __templaros_journal_seq=$((__templaros_journal_seq + 1))
    # A subshell would fork. Temporarily narrow and restore umask in this shell instead.
    local old_umask
    umask > "$__templaros_journal_prefix.umask"
    read -r old_umask < "$__templaros_journal_prefix.umask"
    umask 077
    printf '1\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$__templaros_journal_seq" "$timestamp" "$elapsed" "$exit_status" "$pipeline" \
        "$directory" "$__templaros_journal_label_result" "$full" \
        > "$__templaros_journal_prefix.$(((__templaros_journal_seq - 1) % 32))"
    umask "$old_umask"
    return 0
}
