# Terminal job completion for TemplarOS notifications (zsh): preexec notes when a
# foreground command starts; precmd reports how it ended. Wired with add-zsh-hook, which
# chains with existing hooks instead of replacing them: no resident process, no timer.
#
# Same contract as 61-templaros-completion.sh: $? is captured first and returned last,
# job control is only read (to tell a background launch from a completion), and only a
# bare command name crosses into the notification — never the raw command line.
#
# Delivery is packaging (a zsh rc drop-in); this file is the hook's source of truth.
case ":${XDG_CURRENT_DESKTOP:-}:" in
    *:TemplarOS:*|*:templaros:*) ;;
    *) return 0 ;;
esac

[[ -o interactive ]] || return 0

if (( ! $+functions[__templaros_completion_precmd] )); then
    # Millisecond clock for job timing; without it the hook falls back to SECONDS.
    zmodload -F zsh/datetime p:EPOCHREALTIME 2>/dev/null || true
    if [[ -z "${TEMPLAR_NOTIFY_MIN_SECONDS:-}" ]]; then
        __templaros_completion_policy="$(templar notify policy-sh 2>/dev/null)" || __templaros_completion_policy=""
        case "$__templaros_completion_policy" in
            TEMPLAR_NOTIFY_MIN_SECONDS=*)
                __templaros_completion_policy_value="${__templaros_completion_policy#*=}"
                case "$__templaros_completion_policy_value" in
                    ''|*[^0-9]*) ;;
                    *) TEMPLAR_NOTIFY_MIN_SECONDS="$__templaros_completion_policy_value" ;;
                esac
                unset __templaros_completion_policy_value
                ;;
        esac
        unset __templaros_completion_policy
    fi
    case "${TEMPLAR_NOTIFY_MIN_SECONDS:-}" in
        ''|*[^0-9]*) TEMPLAR_NOTIFY_MIN_SECONDS=10 ;;
    esac

    __templaros_completion_now_ms() {
        if (( $+EPOCHREALTIME )); then
            local sec="${EPOCHREALTIME%.*}" frac="${EPOCHREALTIME#*.}000"
            __templaros_completion_now_ms_result="$((10#$sec * 1000 + 10#${frac:0:3}))"
        else
            __templaros_completion_now_ms_result="$((SECONDS * 1000))"
        fi
    }

    # Same label rule as the Bash hook: skip env assignments and known wrappers, then
    # the basename of the first remaining word. Empty for a bare assignment or for
    # `templar run --notify`, which sends its own notification from the owned exit.
    __templaros_completion_label() {
        local -a words
        words=(${=1})
        __templaros_completion_label_result=""
        if (( ${#words} >= 2 )); then
            local first="${words[1]:t}"
            if [[ "$first" == "templar" && "${words[2]}" == "run" ]]; then
                local item seen_dash=0
                for item in "${words[@]:2}"; do
                    if (( seen_dash )); then
                        __templaros_completion_label_result="${item:t}"
                        return
                    fi
                    [[ "$item" == "--" ]] && seen_dash=1
                    case "$item" in
                        --notify*) return ;;
                    esac
                done
                # No `--` workload: fall through to the plain first word below.
            fi
        fi
        local word in_quote=""
        for word in "${words[@]}"; do
            if [[ -n "$in_quote" ]]; then
                # Still inside an assignment value's quotes: consume through the
                # closing quote rather than mistaking the value for a command.
                [[ "$word" == *"$in_quote"* ]] && in_quote=""
                continue
            fi
            case "$word" in
                *=*)
                    local val="${word#*=}" dq='"' sq="'"
                    local nodq="${val//$dq/}" nosq="${val//$sq/}"
                    if (( (${#val} - ${#nodq}) % 2 )); then
                        in_quote="$dq"
                    elif (( (${#val} - ${#nosq}) % 2 )); then
                        in_quote="$sq"
                    fi
                    continue
                    ;;
                sudo|command|time|nice|nohup|env) continue ;;
            esac
            word="${word:t}"
            word="${word%\"}"; word="${word#\"}"
            word="${word%\'}"; word="${word#\'}"
            if [[ -n "$word" ]]; then
                __templaros_completion_label_result="$word"
                return
            fi
        done
    }

    __templaros_completion_preexec() {
        __templaros_completion_now_ms
        __templaros_completion_start="$__templaros_completion_now_ms_result"
        __templaros_completion_command="$1"
        __templaros_completion_bg_pid="$!"
    }

    __templaros_completion_precmd() {
        local last_status=$? pipeline="${pipestatus[*]}"
        local start="${__templaros_completion_start:-}"
        local raw_command="${__templaros_completion_command:-}"
        local bg_pid="${__templaros_completion_bg_pid:-}"
        unset __templaros_completion_start __templaros_completion_command __templaros_completion_bg_pid
        if [[ -n "$start" && -n "$raw_command" ]]; then
            __templaros_completion_now_ms
            local elapsed_ms="$((__templaros_completion_now_ms_result - start))"
            (( elapsed_ms < 0 )) && elapsed_ms=0
            # A background launch is not a completion: when this cycle started jobs
            # that are still running, the foreground returned instantly.
            if [[ "$!" != "$bg_pid" && -n "$(jobs -r 2>/dev/null)" ]]; then
                return "$last_status"
            fi
            if (( $+functions[__templaros_journal_write] )); then
                __templaros_journal_write "$raw_command" "$last_status" "$elapsed_ms" "$pipeline"
            fi
            __templaros_completion_label "$raw_command"
            if [[ -z "$__templaros_completion_label_result" ]]; then
                return "$last_status"
            fi
            local min_seconds="${TEMPLAR_NOTIFY_MIN_SECONDS:-10}"
            case "$min_seconds" in
                ''|*[^0-9]*) min_seconds=10 ;;
            esac
            if (( elapsed_ms >= min_seconds * 1000 )); then
                templar notify terminal-completed --elapsed-ms "$elapsed_ms" \
                    --status "$last_status" --label "$__templaros_completion_label_result" \
                    >/dev/null 2>&1 || true
            fi
        fi
        return "$last_status"
    }

    autoload -Uz add-zsh-hook
    add-zsh-hook preexec __templaros_completion_preexec
    # Run before existing precmd hooks can overwrite the command's exit status.
    precmd_functions=(__templaros_completion_precmd ${precmd_functions:#__templaros_completion_precmd})
fi
