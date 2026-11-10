# Tachyon shell integration for fish (3.1 and later).
#
# Marks each prompt and command with OSC 133, and reports the folder with OSC
# 7, so Tachyon can draw command bars, write receipts, name tabs and jump with
# Cmd-Up/Down.
#
# Tachyon loads it by itself: it puts this folder's parent first in
# XDG_DATA_DIRS, so fish reads it from vendor_conf.d before your config.fish.
#
# On a machine you ssh into, copy it to ~/.config/fish/conf.d/. It's a no-op
# outside Tachyon.

# Put XDG_DATA_DIRS back the way it was, so programs started from here don't
# see Tachyon's folder.
if set -q TACHYON_ORIG_XDG_DATA_DIRS
    set -gx XDG_DATA_DIRS $TACHYON_ORIG_XDG_DATA_DIRS
    set -e TACHYON_ORIG_XDG_DATA_DIRS
else if set -q TACHYON_SHELL_INTEGRATION
    set -e XDG_DATA_DIRS
end

status is-interactive; or exit
test "$TERM_PROGRAM" = Tachyon; or test "$LC_TERMINAL" = Tachyon; or set -q TMUX; or exit
set -q _tachyon_integrated; and exit
set -g _tachyon_integrated 1

# An OSC sequence for the terminal: inside tmux, which drops OSC 133, wrapped
# for its passthrough (`set -g allow-passthrough on`).
function _tachyon_osc
    if set -q TMUX
        printf '\ePtmux;\e\e]%s\a\e\\' $argv[1]
    else
        printf '\e]%s\a' $argv[1]
    end
end

# A prompt is about to be drawn. fish_prompt itself may be defined later (by
# config.fish, or starship), so it's wrapped here, the first time, to mark
# where typing starts.
function _tachyon_prompt_start --on-event fish_prompt
    _tachyon_osc '133;A'
    _tachyon_osc "7;file://$hostname"(string replace -a % %25 -- $PWD)
    if not functions -q _tachyon_user_prompt; and functions -q fish_prompt
        functions -c fish_prompt _tachyon_user_prompt
        function fish_prompt
            _tachyon_user_prompt
            # In tmux, erasing the blank cell at the cursor (ECH) makes it
            # send the prompt it holds back before this mark.
            set -q TMUX; and printf '\e[1X'
            _tachyon_osc '133;B'
        end
    end
end

# A command line is about to run: its text names the tab.
function _tachyon_preexec --on-event fish_preexec
    set -l cmd (string replace -ra '[[:cntrl:]]' ' ' -- $argv[1] | string join ' ')
    _tachyon_osc "133;C;cmdline="(string sub -l 256 -- "$cmd")
end

# It finished, with this status.
function _tachyon_postexec --on-event fish_postexec
    _tachyon_osc "133;D;$status"
end
