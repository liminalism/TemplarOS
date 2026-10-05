# Homegarden learns an interactive terminal's working context from prompt transitions. This is
# event-driven by Bash itself: no process remains resident and no timer checks the directory.
case ":${XDG_CURRENT_DESKTOP:-}:" in
    *:TemplarOS:*|*:templaros:*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

case $- in
    *i*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

# Bash only: the prompt hook below is Bash syntax, and /etc/profile.d is also read by dash.
if [ -z "${BASH_VERSION:-}" ]; then
    return 0 2>/dev/null || exit 0
fi

if ! declare -F __templaros_garden_prompt >/dev/null 2>&1; then
    __templaros_garden_prompt() {
        if [ "${__templaros_garden_pwd-}" = "$PWD" ]; then
            return
        fi
        if command templar garden observe terminal-entered "$PWD" --role terminal >/dev/null 2>&1; then
            __templaros_garden_pwd=$PWD
        fi
    }

    if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
        PROMPT_COMMAND+=(__templaros_garden_prompt)
    else
        PROMPT_COMMAND="__templaros_garden_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
    fi
fi
