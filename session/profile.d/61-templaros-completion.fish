# Terminal job completion for TemplarOS notifications (fish): a fish_postexec event
# handler reports how each foreground command ended, with fish's own CMD_DURATION as the
# elapsed clock. Event handlers stack with existing configuration instead of replacing
# it: no resident process, no timer.
#
# Same contract as 61-templaros-completion.sh: $status is captured first, job control is
# only read (to tell a background launch from a completion), and only a bare command
# name crosses into the notification — never the raw command line.
#
# Delivery is packaging (a fish conf.d drop-in); this file is the hook's source of truth.
# No early `exit`: this file is sourced into the interactive shell, where exiting
# would close the terminal. Everything lives inside nested conditions instead.
if status is-interactive
    if string match -q '*:TemplarOS:*' ":$XDG_CURRENT_DESKTOP:"; or string match -q '*:templaros:*' ":$XDG_CURRENT_DESKTOP:"
        if not functions -q __templaros_completion_postexec
            if not set -q TEMPLAR_NOTIFY_MIN_SECONDS
                set -l __templaros_completion_policy (templar notify policy-sh 2>/dev/null)
                if string match -q 'TEMPLAR_NOTIFY_MIN_SECONDS=*' -- "$__templaros_completion_policy"
                    set -l __templaros_completion_value (string replace 'TEMPLAR_NOTIFY_MIN_SECONDS=' '' -- "$__templaros_completion_policy")
                    if string match -q -r '^[0-9]+$' -- "$__templaros_completion_value"
                        set -g TEMPLAR_NOTIFY_MIN_SECONDS "$__templaros_completion_value"
                    end
                end
            end
            if not set -q TEMPLAR_NOTIFY_MIN_SECONDS; or not string match -q -r '^[0-9]+$' -- "$TEMPLAR_NOTIFY_MIN_SECONDS"
                set -g TEMPLAR_NOTIFY_MIN_SECONDS 10
            end

            # Same label rule as the other hooks: skip env assignments and known wrappers, then
            # the basename of the first remaining word. Prints nothing for a bare assignment or
            # for `templar run --notify`, which notifies from its own owned exit.
            function __templaros_completion_label
                set -l words (string split -n ' ' -- $argv[1])
                if test (count $words) -ge 2
                    set -l first (string replace -r '.*/' '' -- $words[1])
                    if test "$first" = templar; and test "$words[2]" = run
                        set -l seen_dash 0
                        for item in $words[3..-1]
                            if test "$seen_dash" = 1
                                string replace -r '.*/' '' -- "$item"
                                return
                            end
                            if test "$item" = --
                                set seen_dash 1
                            end
                            switch "$item"
                                case '--notify*'
                                    return
                            end
                        end
                    end
                end
                set -l in_quote ""
                for word in $words
                    if test -n "$in_quote"
                        # Still inside an assignment value's quotes: consume through the
                        # closing quote rather than mistaking the value for a command.
                        if string match -q "*$in_quote*" -- "$word"
                            set in_quote ""
                        end
                        continue
                    end
                    switch "$word"
                        case '*=*'
                            # An env assignment; an unbalanced quote means the following
                            # words are still the value, not the command.
                            set -l val (string replace -r '^[^=]*=' '' -- "$word")
                            set -l dq '"'
                            set -l sq "'"
                            set -l nodq (string replace -a -- "$dq" '' "$val")
                            set -l nosq (string replace -a -- "$sq" '' "$val")
                            set -l ndq (math (string length -- "$val") - (string length -- "$nodq"))
                            set -l nsq (math (string length -- "$val") - (string length -- "$nosq"))
                            if test (math "$ndq % 2") = 1
                                set in_quote "$dq"
                            else if test (math "$nsq % 2") = 1
                                set in_quote "$sq"
                            end
                            continue
                        case sudo command time nice nohup env
                            continue
                    end
                    set -l base (string replace -r '.*/' '' -- "$word")
                    set base (string trim -c "'\"" -- "$base")
                    if test -n "$base"
                        echo "$base"
                        return
                    end
                end
            end

            function __templaros_completion_postexec --on-event fish_postexec
                set -l result $status $pipestatus
                set -l last_status $result[1]
                set -l pipeline $result[2..-1]
                set -l raw_command $argv[1]
                set -l elapsed_ms "$CMD_DURATION"
                if not string match -q -r '^[0-9]+$' -- "$elapsed_ms"
                    set elapsed_ms 0
                end
                # A background launch is not a completion: when this cycle started jobs that
                # are still running, the foreground returned instantly.
                if test "$last_pid" != "$__templaros_completion_bg_pid"
                    if test -n (jobs)
                        set -g __templaros_completion_bg_pid "$last_pid"
                        return $last_status
                    end
                end
                set -g __templaros_completion_bg_pid "$last_pid"
                if functions -q __templaros_journal_write
                    __templaros_journal_write "$raw_command" "$last_status" "$elapsed_ms" (string join ' ' -- $pipeline)
                end
                set -l label (__templaros_completion_label "$raw_command")
                if test -z "$label"
                    return $last_status
                end
                set -l min_seconds "$TEMPLAR_NOTIFY_MIN_SECONDS"
                if not string match -q -r '^[0-9]+$' -- "$min_seconds"
                    set min_seconds 10
                end
                if test "$elapsed_ms" -ge (math "$min_seconds * 1000")
                    templar notify terminal-completed --elapsed-ms "$elapsed_ms" \
                        --status "$last_status" --label "$label" \
                        >/dev/null 2>&1 || true
                end
                return $last_status
            end
        end
    end
end
