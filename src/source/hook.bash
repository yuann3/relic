# relic shell hook for bash: eval "$(relic hook bash)" at the end of
# ~/.bashrc, so that it runs before anything else in PROMPT_COMMAND and
# records the command's exit status, not another hook's.
# Records, at each prompt, the newest history line, when the prompt came,
# the directory the command left and its exit status, into a file of this
# shell's own. Never its output, input or environment. Builtins only: no
# process is started at a prompt, unless the file was deleted. Bash has no
# hook before a command that leaves PROMPT_COMMAND and the DEBUG trap to
# their owners, so there is no start time, duration or starting directory;
# relic reads the history number to tell a new command from a prompt
# redrawn without one. A command history leaves out (HISTCONTROL,
# HISTIGNORE) is not recorded, and with `history -c; history -r` in
# PROMPT_COMMAND a prompt without a command can record the line another
# shell last wrote as this one's.
if [[ $- == *i* && -z ${_relic_hook_file-} ]]; then
  # Bash before 5.0 has no EPOCHSECONDS; SECONDS counts from here instead.
  _relic_hook_t0=$(( ${EPOCHSECONDS:-$(date +%s)} - SECONDS ))
  _relic_hook_file="${XDG_STATE_HOME:-$HOME/.local/state}/relic/hooks/bash-$(( _relic_hook_t0 + SECONDS ))-$$-$RANDOM.frames"
  # In a subshell, so the umask that makes them 0700 and 0600 stays there.
  ( umask 077; mkdir -p -- "${_relic_hook_file%/*}" && : >> "$_relic_hook_file" ) 2>/dev/null
  _relic_hook_prompt() {
    # First: anything else run here replaces $?.
    local s=$?
    # `>>` alone would make a deleted file again under the user's umask.
    [[ -e $_relic_hook_file || ! -d ${_relic_hook_file%/*} ]] || ( umask 077; : >> "$_relic_hook_file" ) 2>/dev/null
    { printf 'rh1\0bash\0\0%s\0%s\0%s\0' "${EPOCHSECONDS:-$(( _relic_hook_t0 + SECONDS ))}" "$s" "$PWD"
      HISTTIMEFORMAT= builtin history 1
      printf '\0\n'; } 2>/dev/null >> "$_relic_hook_file"
    # Hooks after this one in PROMPT_COMMAND see the command's status.
    return $s
  }
  # First, so the status it records is the command's. 5.1 runs each
  # element of an array PROMPT_COMMAND; before it, only the first. In an &&
  # list, a failing return does not run the ERR trap a second time.
  if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
    PROMPT_COMMAND=('_relic_hook_prompt && :' "${PROMPT_COMMAND[@]}")
  else
    PROMPT_COMMAND="_relic_hook_prompt && :${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  fi
fi
