# Tachyon shell integration for bash (3.2, as macOS ships it, and later).
#
# Marks each prompt and command with OSC 133, and reports the folder with OSC
# 7, so Tachyon can draw command bars, write receipts, name tabs and jump with
# Cmd-Up/Down.
#
# Tachyon loads it by itself: it starts bash with --rcfile pointing here, in
# place of ~/.bashrc. This file then reads the startup files bash would have
# read (the login ones for a login shell), and adds its hooks after them.
#
# On a machine you ssh into, source it from ~/.bashrc instead:
#   [[ $LC_TERMINAL == Tachyon ]] && source ~/.tachyon-integration.bash
# Safe to source more than once, and a no-op outside Tachyon.

if [ -n "${_TACHYON_BASH_INJECT-}" ]; then
  builtin unset _TACHYON_BASH_INJECT
  if [ -n "${_TACHYON_BASH_LOGIN-}" ]; then
    builtin unset _TACHYON_BASH_LOGIN
    if [ -r /etc/profile ]; then builtin source /etc/profile; fi
    for _tachyon_f in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
      if [ -r "$_tachyon_f" ]; then builtin source "$_tachyon_f"; break; fi
    done
    builtin unset _tachyon_f
  elif [ -r "$HOME/.bashrc" ]; then
    builtin source "$HOME/.bashrc"
  fi
fi

if [[ $- == *i* && ( ${TERM_PROGRAM-} == Tachyon || ${LC_TERMINAL-} == Tachyon ) && -z ${_TACHYON_INTEGRATED-} ]]; then
  _TACHYON_INTEGRATED=1
  _tachyon_running=
  _tachyon_armed=
  _tachyon_hist=
  _tachyon_prev_debug=

  # First in PROMPT_COMMAND: close the command that just ran and start a
  # prompt. Returns the command's status, so the rest of PROMPT_COMMAND (a
  # starship or git prompt) still sees it in $?.
  _tachyon_precmd() {
    local ret=$?
    if [[ -n $_tachyon_running ]]; then
      builtin printf '\033]133;D;%s\007' "$ret"
      _tachyon_running=
    fi
    builtin printf '\033]133;A\007\033]7;file://%s%s\007' "${HOSTNAME-}" "${PWD//\%/%25}"
    return $ret
  }

  # Last in PROMPT_COMMAND: mark where typing starts, keep PROMPT_COMMAND
  # around ours if something replaced it, and arm the DEBUG trap for the next
  # line.
  _tachyon_ready() {
    local ret=$?
    if [[ $PS1 != *'133;B'* ]]; then
      PS1=$PS1'\[\033]133;B\007\]'
    fi
    _tachyon_install
    _tachyon_hist=$(HISTTIMEFORMAT= builtin history 1)
    _tachyon_armed=1
    return $ret
  }

  # The DEBUG trap runs before every simple command at the prompt. The first
  # one after the prompt is the command line starting.
  _tachyon_debug() {
    # Only commands typed at the prompt, not ones inside shell functions
    # (with functrace on, the trap runs there too).
    if [[ -n $_tachyon_armed && ${#FUNCNAME[@]} -le 1 ]]; then
      _tachyon_armed=
      # An empty line runs nothing; the next prompt's own hook comes first.
      if [[ $BASH_COMMAND != _tachyon_precmd* ]]; then
        local hist cmd
        hist=$(HISTTIMEFORMAT= builtin history 1)
        if [[ -n $hist && $hist != "$_tachyon_hist" ]]; then
          # " 123  make test" -> "make test"
          cmd=${hist#*[0-9] }
          cmd=${cmd#"${cmd%%[![:space:]]*}"}
        else
          # Not in history (ignorespace, ignoredups, history off).
          cmd=$BASH_COMMAND
        fi
        cmd=${cmd//[[:cntrl:]]/ }
        _tachyon_running=1
        builtin printf '\033]133;C;cmdline=%s\007' "${cmd:0:256}"
      fi
    fi
    # A DEBUG trap that was there first (starship times commands with one)
    # still runs, with $_ as it would have seen it.
    if [[ -n $_tachyon_prev_debug ]]; then
      : "$1"
      builtin eval "$_tachyon_prev_debug"
    fi
  }

  _tachyon_install() {
    # PROMPT_COMMAND: ours first and last, around whatever is there, put
    # back if a prompt framework replaced it. Bash 5.1 also allows an array.
    if [[ "$(builtin declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
      if [[ " ${PROMPT_COMMAND[*]} " != *" _tachyon_precmd "* ]]; then
        PROMPT_COMMAND=(_tachyon_precmd "${PROMPT_COMMAND[@]}" _tachyon_ready)
      fi
    elif [[ ${PROMPT_COMMAND-} != *_tachyon_precmd* ]]; then
      local pc=${PROMPT_COMMAND-}
      while [[ $pc == *[\;[:space:]] ]]; do pc=${pc%?}; done
      PROMPT_COMMAND="_tachyon_precmd${pc:+; $pc}; _tachyon_ready"
    fi
  }

  _tachyon_install

  # The DEBUG trap: ours, calling any trap that was already set. Set here at
  # the top level: bash 3.2 puts a trap set inside a function back when the
  # function returns, and prints nothing for $(trap -p) there, hence the file.
  _tachyon_tmp=${TMPDIR:-/tmp}/tachyon-trap.$$
  builtin trap -p DEBUG >"$_tachyon_tmp" 2>/dev/null
  _tachyon_prev_debug=$(<"$_tachyon_tmp")
  command rm -f "$_tachyon_tmp"
  builtin unset _tachyon_tmp
  if [[ $_tachyon_prev_debug == *_tachyon_debug* ]]; then
    _tachyon_prev_debug=
  elif [[ -n $_tachyon_prev_debug ]]; then
    # "trap -- 'cmd' DEBUG" -> cmd
    _tachyon_prev_debug=${_tachyon_prev_debug#trap -- \'}
    _tachyon_prev_debug=${_tachyon_prev_debug%\' DEBUG}
    _tachyon_prev_debug=${_tachyon_prev_debug//\'\\\'\'/\'}
  fi
  builtin trap '_tachyon_debug "$_"' DEBUG
fi
