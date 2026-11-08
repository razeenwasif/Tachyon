# Tachyon starts zsh with ZDOTDIR pointing here, so this file runs first.
# Restore the user's ZDOTDIR, run their .zshenv, and load the integration;
# zsh then reads their .zprofile and .zshrc from the restored ZDOTDIR as usual.

if [[ -n $TACHYON_ZDOTDIR_WAS_SET ]]; then
  ZDOTDIR=$TACHYON_ORIG_ZDOTDIR
else
  unset ZDOTDIR
fi
unset TACHYON_ORIG_ZDOTDIR TACHYON_ZDOTDIR_WAS_SET

if [[ -f ${ZDOTDIR:-$HOME}/.zshenv ]]; then
  source ${ZDOTDIR:-$HOME}/.zshenv
fi

if [[ -o interactive && -n $TACHYON_SHELL_INTEGRATION ]]; then
  source "$TACHYON_SHELL_INTEGRATION/tachyon-integration.zsh"
fi
