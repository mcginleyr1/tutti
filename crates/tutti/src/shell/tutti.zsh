# tutti shell integration for zsh. Inside a tutti pane it reports the working
# directory (OSC 7) and marks each prompt and command (OSC 133), so a split
# starts where you are and `tutti pane read --last-output` finds the last
# command's output. Load it from ~/.zshrc: eval "$(tutti shell-init zsh)"
if [[ -n ${TUTTI_PANE-} && -z ${__tutti_shell-} ]]; then
  __tutti_shell=1
  __tutti_precmd() {
    local ret=$?
    emulate -L zsh
    local LC_ALL=C c dir=
    for c in ${(s::)PWD}; do
      [[ $c == [A-Za-z0-9/._~-] ]] || printf -v c '%%%02X' "'$c"
      dir+=$c
    done
    printf '\e]133;D;%s\a\e]7;file://%s%s\a\e]133;A\a' "$ret" "$HOST" "$dir"
    [[ $PS1 == *'133;B'* ]] || PS1+=$'%{\e]133;B\a%}'
  }
  __tutti_preexec() { printf '\e]133;C\a'; }
  # PROMPT_SP draws its mark before precmd runs, right where output ended:
  # a D there keeps the mark out of the output (precmd's D is the fallback).
  PROMPT_EOL_MARK=$'%{\e]133;D;%?\a%}'${PROMPT_EOL_MARK-'%B%S%#%s%b'}
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __tutti_precmd
  add-zsh-hook preexec __tutti_preexec
fi
