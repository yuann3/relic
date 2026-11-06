# relic shell hook for zsh: eval "$(relic hook zsh)" in ~/.zshrc.
# Records what each command line was, when it started and ended, where it
# started and its exit status, one frame per command into a file of this
# shell's own. Never its output, input or environment. Builtins only: no
# process is started at a prompt. A command line starting with a space is
# not recorded, as HIST_IGNORE_SPACE would not.
if [[ -o interactive && -z ${_relic_hook_file-} ]]; then
  zmodload zsh/datetime 2>/dev/null
  typeset -g _relic_hook_file="${XDG_STATE_HOME:-$HOME/.local/state}/relic/hooks/zsh-${EPOCHSECONDS:-0}-$$-$RANDOM.frames"
  # In a subshell, so the umask that makes them 0700 and 0600 stays there.
  ( umask 077; mkdir -p -- "${_relic_hook_file:h}" && : >> "$_relic_hook_file" ) 2>/dev/null
  _relic_hook_preexec() {
    emulate -L zsh
    [[ $1 == ' '* ]] && return 0
    typeset -g _relic_hook_cmd=$1 _relic_hook_start=$EPOCHREALTIME _relic_hook_cwd=$PWD
  }
  _relic_hook_precmd() {
    # First: anything else run here replaces $?.
    local s=$?
    emulate -L zsh
    # Unset when the prompt was redrawn without a command running.
    if [[ -n ${_relic_hook_start-} ]]; then
      { printf 'rh1\0zsh\0%s\0%s\0%s\0%s\0%s\0\n' "$_relic_hook_start" "$EPOCHREALTIME" "$s" \
          "$_relic_hook_cwd" "$_relic_hook_cmd" >> "$_relic_hook_file"; } 2>/dev/null
      unset _relic_hook_cmd _relic_hook_start _relic_hook_cwd
    fi
    return $s
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook preexec _relic_hook_preexec
  add-zsh-hook precmd _relic_hook_precmd
fi
