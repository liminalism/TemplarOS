# Event-driven optional fish integration. No timer, no replacement of user event handlers.
if status is-interactive
    if string match -q '*:TemplarOS:*' ":$XDG_CURRENT_DESKTOP:"; or string match -q '*:templaros:*' ":$XDG_CURRENT_DESKTOP:"
        if not functions -q __templaros_garden_prompt
            set -g __templaros_journal_prefix (templar garden journal-init $fish_pid 2>/dev/null)
            set -g __templaros_journal_seq 0
            function __templaros_garden_prompt --on-event fish_prompt
                set -l last_status $status
                if test "$__templaros_garden_pwd" != "$PWD"
                    if templar garden observe terminal-entered "$PWD" --role terminal >/dev/null 2>&1
                        set -g __templaros_garden_pwd "$PWD"
                    end
                end
                return $last_status
            end
            function __templaros_journal_encode
                set -l encoded (string replace -a '%' '%25' -- "$argv[1]")
                set encoded (string replace -a \t '%09' -- "$encoded")
                set encoded (string replace -a \n '%0A' -- "$encoded")
                string replace -a \r '%0D' -- "$encoded"
            end
            function __templaros_journal_write
                test -n "$__templaros_journal_prefix"; or return 0
                set -l raw "$argv[1]"
                # fish already excludes leading-space history; apply the same rule ourselves.
                string match -q -r '^\\s' -- "$raw"; and return 0
                set -l words (string split -n ' ' -- "$raw")
                while test (count $words) -gt 0
                    if string match -q '*=*' -- "$words[1]"; or contains -- "$words[1]" env command sudo time nice nohup
                        set words $words[2..-1]
                    else
                        break
                    end
                end
                set -l program command
                set -l verb ''
                if test (count $words) -gt 0
                    set program (string replace -r '.*/' '' -- "$words[1]")
                end
                if test (count $words) -gt 1
                    set verb "$words[2]"
                end
                contains -- "$program" cargo git npm pnpm yarn claude codex gemini bash zsh fish sh make cmake ninja python python3 node go rustc nvim vim code ls cd true false sleep echo printf; or set program command
                set -l label "$program"
                switch "$program:$verb"
                    case cargo:build cargo:test cargo:check cargo:run cargo:clippy cargo:fmt cargo:bench cargo:doc cargo:clean git:status git:diff git:log git:commit git:rebase git:merge git:fetch git:pull git:push git:checkout git:switch git:restore git:add npm:run npm:test npm:build npm:install pnpm:run pnpm:test pnpm:build pnpm:install yarn:run yarn:test yarn:build yarn:install
                        set label "$program $verb"
                end
                read -l uptime ignored < /proc/uptime; or return 0
                set -l timestamp (math --scale=0 "$uptime * 1000")
                set -l directory (__templaros_journal_encode "$PWD")
                set -l full ''
                if test "$TEMPLAR_GARDEN_FULL_COMMANDS" = 1
                    set full (__templaros_journal_encode (string sub -l 2048 -- "$raw"))
                end
                set -g __templaros_journal_seq (math "$__templaros_journal_seq + 1")
                set -l slot (math "($__templaros_journal_seq - 1) % 32")
                set -l old_umask (umask)
                umask 077
                printf '1\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$__templaros_journal_seq" "$timestamp" "$argv[3]" "$argv[2]" "$argv[4]" "$directory" "$label" "$full" > "$__templaros_journal_prefix.$slot"
                umask "$old_umask"
                return 0
            end
        end
    end
end
