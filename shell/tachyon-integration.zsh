# Tachyon shell integration for zsh.
#
# Marks each prompt and command with OSC 133 so Tachyon knows where commands
# start and end: that drives the gutter marks, run receipts and Cmd-Up/Down.
# Loaded automatically (see zsh/.zshenv). On a machine you ssh into, source it
# from ~/.zshrc; ssh passes LC_TERMINAL, which tells it it's in Tachyon. Safe
# to source more than once, and a no-op outside Tachyon.

[[ -n $_TACHYON_INTEGRATED ]] && return
[[ $TERM_PROGRAM == Tachyon || $LC_TERMINAL == Tachyon ]] || return
typeset -g _TACHYON_INTEGRATED=1

_tachyon_precmd() {
  # Must run first among precmd hooks so $? is still the command's status.
  local ret=$?
  if [[ -n $_tachyon_running ]]; then
    builtin print -n "\e]133;D;$ret\a"
    unset _tachyon_running
  fi
  builtin print -n "\e]133;A\a"
  # The working directory, which names the tab while it sits at a prompt.
  builtin print -rn -- $'\e]7;file://'"${HOST}${PWD//\%/%25}"$'\a'
  # Mark where typing starts: the end of the prompt. Re-added whenever a theme
  # (starship, powerlevel10k) has rebuilt the prompt without it.
  if [[ $PS1 != *$'\e]133;B\a'* ]]; then
    PS1="$PS1%{"$'\e]133;B\a'"%}"
  fi
}

_tachyon_preexec() {
  typeset -g _tachyon_running=1
  # The command line (control characters blanked, at most 256 characters)
  # names the tab while it runs.
  local cmd=${1//[[:cntrl:]]/ }
  builtin print -rn -- $'\e]133;C;cmdline='"${cmd[1,256]}"$'\a'
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _tachyon_precmd
add-zsh-hook preexec _tachyon_preexec
