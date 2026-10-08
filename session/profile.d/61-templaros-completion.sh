# Terminal job completion for TemplarOS notifications. The DEBUG trap notes when a
# foreground command starts; the prompt hook reports how it ended. Event-driven by Bash
# itself: no resident process, no timer, no polling.
#
# What this never does: change $? (captured first, returned last), disturb prompt hooks
# (chained, ours first), touch job control (only read, to tell a background launch from a
# completion), alter Homegarden CWD reporting (that is 60-templaros-garden.sh),
# or persist raw command lines (only a bare command name crosses into the notification).
#
# Known blind spot, measured on bash 5.3: the DEBUG trap never fires for a `( ... )`
# subshell group, so one stays untimed and unreported. A Ctrl-C interrupting an ordinary
# command is unaffected — the trap fired when the command started, and the prompt hook
# still sees status 130.
case ":${XDG_CURRENT_DESKTOP:-}:" in
    *:TemplarOS:*|*:templaros:*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

case $- in
    *i*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

# Bash only: the DEBUG trap below is Bash syntax, and /etc/profile.d is also read by dash.
if [ -z "${BASH_VERSION:-}" ]; then
    return 0 2>/dev/null || exit 0
fi

if ! declare -F __templaros_completion_precmd >/dev/null 2>&1; then
    # Elapsed prefilter, in whole seconds: the hook reports only jobs at least this long,
    # so an everyday `ls` never spawns the delivery route. Read once from policy at shell
    # startup; an explicit export wins and applies immediately. Never eval'd: the value
    # must be a bare number or it is ignored.
    if [ -z "${TEMPLAR_NOTIFY_MIN_SECONDS:-}" ]; then
        __templaros_completion_policy="$(templar notify policy-sh 2>/dev/null)" || __templaros_completion_policy=""
        case "$__templaros_completion_policy" in
            TEMPLAR_NOTIFY_MIN_SECONDS=*)
                __templaros_completion_policy_value="${__templaros_completion_policy#*=}"
                case "$__templaros_completion_policy_value" in
                    ''|*[!0-9]*) ;;
                    *) TEMPLAR_NOTIFY_MIN_SECONDS="$__templaros_completion_policy_value" ;;
                esac
                unset __templaros_completion_policy_value
                ;;
        esac
        unset __templaros_completion_policy
    fi
    case "${TEMPLAR_NOTIFY_MIN_SECONDS:-}" in
        ''|*[!0-9]*) TEMPLAR_NOTIFY_MIN_SECONDS=10 ;;
    esac

    __templaros_completion_now_ms() {
        if [ -n "${EPOCHREALTIME:-}" ]; then
            # seconds.fraction -> milliseconds, without forking date(1) per command.
            # The 10# prefixes keep values like 08 from parsing as octal.
            local sec="${EPOCHREALTIME%.*}" frac="${EPOCHREALTIME#*.}000"
            __templaros_completion_now_ms_result="$((10#$sec * 1000 + 10#${frac:0:3}))"
        else
            __templaros_completion_now_ms_result="$((SECONDS * 1000))"
        fi
    }


    # Reduce a command line to a safe label: skip env assignments and known wrappers,
    # then the basename of the first remaining word. Empty when nothing runnable
    # remains (a bare assignment) or when the line notifies itself.
    __templaros_completion_label() {
        local word rest self_notify=0
        rest="$1"
        # `templar run --notify` sends its own notification from the owned child's exit;
        # the hook stays silent so the job is not reported twice. Only `--notify` before
        # any `--` counts: arguments after it belong to the workload, not to run.
        local words=()
        local IFS=' '
        read -r -a words <<< "$rest" || true
        if [ "${#words[@]}" -ge 2 ]; then
            local first="${words[0]##*/}"
            if [ "$first" = "templar" ] && [ "${words[1]}" = "run" ]; then
                local item
                for item in "${words[@]:2}"; do
                    case "$item" in
                        --) break ;;
                        --notify*) self_notify=1 ;;
                    esac
                done
            fi
        fi
        if [ "$self_notify" = "1" ]; then
            __templaros_completion_label_result=""
            __templaros_completion_self_notified=1
            return
        fi
        __templaros_completion_self_notified=0
        # A plain `templar run` is reported by the hook (only --notify self-reports),
        # labeled with its workload rather than with the launcher.
        if [ "${#words[@]}" -ge 2 ]; then
            local first="${words[0]##*/}"
            if [ "$first" = "templar" ] && [ "${words[1]}" = "run" ]; then
                local item seen_dash=0
                for item in "${words[@]:2}"; do
                    if [ "$seen_dash" = "1" ]; then
                        words=("$item")
                        break
                    fi
                    case "$item" in
                        --) seen_dash=1 ;;
                    esac
                done
            fi
        fi
        local in_quote=""
        for word in "${words[@]}"; do
            if [ -n "$in_quote" ]; then
                # Still inside an assignment value's quotes: consume through the
                # closing quote rather than mistaking the value for a command.
                case "$word" in
                    *"$in_quote"*) in_quote="" ;;
                esac
                continue
            fi
            case "$word" in
                *=*)
                    # An env assignment. When its value opens a quote it never
                    # closes on this word (`CFLAGS="-O2 -g" make`), the following
                    # words are still the value. Quote counts come from length
                    # differences so no literal quote ever sits inside an
                    # expansion, where the parser would swallow the line.
                    local val="${word#*=}" dq='"' sq
                    printf -v sq '%c' 39
                    local nodq="${val//$dq/}" nosq="${val//$sq/}"
                    if [ "$(( (${#val} - ${#nodq}) % 2 ))" = "1" ]; then
                        in_quote="$dq"
                    elif [ "$(( (${#val} - ${#nosq}) % 2 ))" = "1" ]; then
                        in_quote="$sq"
                    fi
                    continue
                    ;;
                sudo|command|time|nice|nohup|env) continue ;;
            esac
            word="${word##*/}"
            word="${word%\"}"; word="${word#\"}"
            word="${word%\'}"; word="${word#\'}"
            if [ -n "$word" ]; then
                __templaros_completion_label_result="$word"
                return
            fi
        done
        __templaros_completion_label_result=""
    }

    __templaros_completion_preexec() {
        # PS0 arms only after Bash reads a command, not while it runs PROMPT_COMMAND.
        # A prompt element can be an arbitrary compound list (Kitty uses `&&`), so
        # comparing BASH_COMMAND to the element's source cannot identify that phase.
        # One pipeline, one timing: the first element arms, the rest observe.
        if [ -n "${__templaros_completion_start:-}" ] ||
            [ -z "${__templaros_completion_ready:-}" ]; then
            return 0
        fi
        # PS0/PS1 substitutions and function bodies under functrace/extdebug are
        # integration machinery, not a command in the interactive parent.
        if [ "$BASHPID" != "$__templaros_completion_shell_pid" ] ||
            [ "${#FUNCNAME[@]}" -gt 2 ]; then
            return 0
        fi
        __templaros_completion_now_ms
        __templaros_completion_start="$__templaros_completion_now_ms_result"
        __templaros_completion_command="$BASH_COMMAND"
        __templaros_completion_bg_pid="$!"
        __templaros_journal_private=0
        # BASH_COMMAND has already lost leading whitespace. Honor history exclusions
        # without changing the person's HISTCONTROL or enabling disabled history.
        if [[ ":${SHELLOPTS}:" != *:history:* ]] ||
            [ "${HISTCMD:-0}" -lt "${__templaros_journal_histcmd:-0}" ]; then
            __templaros_journal_private=1
        else
            case ":${HISTCONTROL:-}:" in
                *:ignorespace:*|*:ignoreboth:*) ;;
                *)
                    # With ignorespace disabled, inspect the admitted raw line. Bash
                    # history's fixed two-space separator precedes the original text.
                    # No usable raw line means no journal entry, not guessed consent.
                    local history_line
                    history_line="$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)" || history_line=
                    if ! [[ "$history_line" =~ ^[[:space:]]*[0-9]+[[:blank:]]{2}[^[:blank:]] ]]; then
                        __templaros_journal_private=1
                    fi ;;
            esac
        fi
        unset __templaros_completion_ready
        return 0
    }

    __templaros_completion_precmd() {
        local status=$? pipeline="${PIPESTATUS[*]}"
        if [ -z "${__templaros_completion_debug_installed:-}" ]; then
            # source without functrace temporarily clears its caller's DEBUG trap.
            # Capture only after startup sources unwind, in this traced function.
            # trap -p's shell quoting must be decoded, including single quotes.
            local previous_trap
            previous_trap="$(trap -p DEBUG)" || previous_trap=
            case "$previous_trap" in
                ""|"trap -- '' DEBUG") __templaros_completion_prev_debug= ;;
                *)
                    previous_trap="${previous_trap#trap -- }"
                    eval "__templaros_completion_prev_debug=${previous_trap% DEBUG}"
                    ;;
            esac
            __templaros_completion_debug_installed=1
            trap '__templaros_completion_chain_debug' DEBUG
        fi
        local start="${__templaros_completion_start:-}"
        local raw_command="${__templaros_completion_command:-}"
        local bg_pid="${__templaros_completion_bg_pid:-}"
        unset __templaros_completion_start __templaros_completion_command __templaros_completion_bg_pid
        unset __templaros_completion_ready
        __templaros_journal_histcmd="${HISTCMD:-0}"
        if [ -n "$start" ] && [ -n "$raw_command" ]; then
            __templaros_completion_now_ms
            local elapsed_ms
            elapsed_ms="$((__templaros_completion_now_ms_result - start))"
            if [ "$elapsed_ms" -lt 0 ]; then
                elapsed_ms=0
            fi
            # A background launch is not a completion: when this cycle started jobs that
            # are still running, the foreground returned instantly and there is nothing
            # to report. `cmd & wait` reaps before the prompt and reports normally.
            if [ "$!" != "$bg_pid" ] && [ -n "$(jobs -r 2>/dev/null)" ]; then
                return "$status"
            fi
            if declare -F __templaros_journal_write >/dev/null &&
                [ "${__templaros_journal_private:-0}" = 0 ]; then
                __templaros_journal_write "$raw_command" "$status" "$elapsed_ms" "$pipeline"
            fi
            __templaros_completion_label "$raw_command"
            if [ -z "$__templaros_completion_label_result" ]; then
                return "$status"
            fi
            local min_seconds="${TEMPLAR_NOTIFY_MIN_SECONDS:-10}"
            case "$min_seconds" in
                ''|*[!0-9]*) min_seconds=10 ;;
            esac
            local min_ms
            min_ms="$((min_seconds * 1000))"
            if [ "$elapsed_ms" -ge "$min_ms" ]; then
                templar notify terminal-completed --elapsed-ms "$elapsed_ms" \
                    --status "$status" --label "$__templaros_completion_label_result" \
                    >/dev/null 2>&1 || true
            fi
        fi
        return "$status"
    }
    # Inherit the restored caller DEBUG handler for first-prompt installation.
    # This is a function-local trace attribute, not a change to shell functrace.
    declare -ft __templaros_completion_precmd

    __templaros_completion_chain_debug() {
        local incoming_status=$?
        __templaros_completion_preexec
        # The previous handler's status is its own: under extdebug a non-zero return
        # skips the command, and swallowing it would break that contract.
        if [ -n "${__templaros_completion_prev_debug:-}" ]; then
            # Restore the status seen by the previous trap, not preexec's bookkeeping.
            __templaros_completion_restore_status "$incoming_status"
            eval "$__templaros_completion_prev_debug"
            return "$?"
        fi
        return 0
    }

    __templaros_completion_restore_status() { return "$1"; }


    if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
        PROMPT_COMMAND=(__templaros_completion_precmd "${PROMPT_COMMAND[@]}")
    else
        PROMPT_COMMAND="__templaros_completion_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
    fi
    __templaros_completion_shell_pid=$BASHPID
    __templaros_completion_ps0=
    # A zero-length substring performs the arithmetic assignment in the parent
    # during PS0 expansion, without output, a command substitution or a fork.
    # Preserve the user's PS0 and let integrations such as Kitty decorate it.
    PS0='${__templaros_completion_ps0:__templaros_completion_ready=1:0}'"${PS0-}"
fi
