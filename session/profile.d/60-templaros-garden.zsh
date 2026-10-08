# Optional shipped zsh integration; preserves user hook arrays and job control.
case ":${XDG_CURRENT_DESKTOP:-}:" in *:TemplarOS:*|*:templaros:*) ;; *) return 0 ;; esac
[[ -o interactive ]] || return 0
if (( ! $+functions[__templaros_garden_prompt] )); then
    if [[ -r "${${(%):-%N}:h}/59-templaros-journal.sh" ]]; then
        source "${${(%):-%N}:h}/59-templaros-journal.sh"
    else
        source /etc/profile.d/59-templaros-journal.sh
    fi
    __templaros_garden_prompt() {
        local last_status=$?
        if [[ "${__templaros_garden_pwd-}" != "$PWD" ]]; then
            if templar garden observe terminal-entered "$PWD" --role terminal >/dev/null 2>&1; then
                __templaros_garden_pwd="$PWD"
            fi
        fi
        return "$last_status"
    }
    autoload -Uz add-zsh-hook
    add-zsh-hook precmd __templaros_garden_prompt
fi
