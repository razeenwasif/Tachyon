# Tachyon shell integration for zsh.
#
# Marks each prompt and command with OSC 133 so Tachyon knows where commands
# start and end: that drives the gutter marks, run receipts and Cmd-Up/Down.
# Loaded automatically (see zsh/.zshenv). On a machine you ssh into, source it
# from ~/.zshrc; ssh passes LC_TERMINAL, which tells it it's in Tachyon. Safe
# to source more than once, and a no-op outside Tachyon.
#
# Inside tmux, whose shells Tachyon doesn't start, source it from ~/.zshrc:
#   [[ -n $TMUX ]] && source /Applications/Tachyon.app/Contents/Resources/shell/tachyon-integration.zsh
# tmux drops OSC 133, so there the marks go through its passthrough (`set -g
# allow-passthrough on`); a terminal outside that doesn't read them ignores
# them.

[[ -n $_TACHYON_INTEGRATED ]] && return
[[ $TERM_PROGRAM == Tachyon || $LC_TERMINAL == Tachyon || -n $TMUX ]] || return
typeset -g _TACHYON_INTEGRATED=1

# An OSC sequence for the terminal: wrapped for tmux's passthrough inside it.
if [[ -n $TMUX ]]; then
  typeset -g _tachyon_pre=$'\ePtmux;\e\e]' _tachyon_post=$'\a\e\\'
  # tmux holds back text it's drawing but passes a mark on at once, so the
  # mark where typing starts would come before the prompt. Erasing the blank
  # cell at the cursor (ECH) makes tmux send the prompt first.
  typeset -g _tachyon_input=$'\e[1X'${_tachyon_pre}133';B'${_tachyon_post}
else
  typeset -g _tachyon_pre=$'\e]' _tachyon_post=$'\a'
  typeset -g _tachyon_input=${_tachyon_pre}133';B'${_tachyon_post}
fi
_tachyon_osc() { builtin print -rn -- "$_tachyon_pre$1$_tachyon_post" }

_tachyon_precmd() {
  # Must run first among precmd hooks so $? is still the command's status.
  local ret=$?
  if [[ -n $_tachyon_running ]]; then
    _tachyon_osc "133;D;$ret"
    unset _tachyon_running
  fi
  _tachyon_osc "133;A"
  # The working directory, which names the tab while it sits at a prompt.
  _tachyon_osc "7;file://${HOST}${PWD//\%/%25}"
  # Mark where typing starts: the end of the prompt. Re-added whenever a theme
  # (starship, powerlevel10k) has rebuilt the prompt without it.
  if [[ $PS1 != *"$_tachyon_input"* ]]; then
    PS1="$PS1%{$_tachyon_input%}"
  fi
}

_tachyon_preexec() {
  typeset -g _tachyon_running=1
  # The command line (control characters blanked, at most 256 characters)
  # names the tab while it runs.
  local cmd=${1//[[:cntrl:]]/ }
  _tachyon_osc "133;C;cmdline=${cmd[1,256]}"
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _tachyon_precmd
add-zsh-hook preexec _tachyon_preexec

# A tab Tachyon opened to run something (following a cluster job): run it as
# if typed at the first prompt, so it's in history and gets a receipt. The
# line editor isn't set up yet while this file loads, so the hook that types
# it is added at the first prompt.
if [[ -n $TACHYON_RUN ]]; then
  typeset -g _tachyon_run=$TACHYON_RUN
  unset TACHYON_RUN
  _tachyon_run_once() {
    [[ -n $_tachyon_run ]] || return
    BUFFER=$_tachyon_run
    _tachyon_run=
    add-zle-hook-widget -d line-init _tachyon_run_once
    zle .accept-line
  }
  _tachyon_run_setup() {
    add-zsh-hook -d precmd _tachyon_run_setup
    autoload -Uz add-zle-hook-widget
    add-zle-hook-widget line-init _tachyon_run_once
  }
  add-zsh-hook precmd _tachyon_run_setup
fi
