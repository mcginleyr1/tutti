# tutti shell integration for fish. Inside a tutti pane it reports the working
# directory (OSC 7) and marks each prompt and command (OSC 133), so a split
# starts where you are and `tutti pane read --last-output` finds the last
# command's output. Load it from ~/.config/fish/config.fish:
# tutti shell-init fish | source
if set -q TUTTI_PANE; and not set -q __tutti_shell
    set -g __tutti_shell 1
    function __tutti_postexec --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
    end
    function __tutti_prompt --on-event fish_prompt
        printf '\e]7;file://%s%s\a\e]133;A\a' $hostname (string escape --style=url -- $PWD)
    end
    function __tutti_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end
end
