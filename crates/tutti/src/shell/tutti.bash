# tutti shell integration for bash. Inside a tutti pane it reports the working
# directory (OSC 7) and marks each prompt and command (OSC 133), so a split
# starts where you are and `tutti pane read --last-output` finds the last
# command's output. Load it from ~/.bashrc: eval "$(tutti shell-init bash)"
# Marking where a command's output starts takes PS0, bash 4.4 or newer: an
# older bash (macOS's /bin/bash is 3.2) reports the directory only.
if [[ -n ${TUTTI_PANE-} && -z ${__tutti_shell-} ]]; then
  __tutti_shell=1
  __tutti_prompt() {
    local ret=$? LC_ALL=C c dir= i n
    for ((i = 0; i < ${#PWD}; i++)); do
      c=${PWD:i:1}
      # bash 3.2 reads a byte past 0x7f as negative: mask it.
      [[ $c == [A-Za-z0-9/._~-] ]] || { printf -v n %d "'$c"; printf -v c %%%02X $((n & 255)); }
      dir+=$c
    done
    printf '\e]133;D;%s\a\e]7;file://%s%s\a\e]133;A\a' "$ret" "$HOSTNAME" "$dir"
    [[ $PS1 == *'133;B'* ]] || PS1+='\[\e]133;B\a\]'
    return $ret
  }
  PROMPT_COMMAND="__tutti_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  PS0+='\e]133;C\a'
fi
