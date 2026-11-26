# llmsh shell integration (generated; do not edit, overwritten on start)
[[ -f /etc/bash.bashrc ]] && source /etc/bash.bashrc
[[ -f ~/.bashrc ]] && source ~/.bashrc

__llmsh_t0=
__llmsh_hn=

# Runs first in PROMPT_COMMAND so that $? is the user's command status.
__llmsh_pre() {
  local rc=$?
  if [[ -n $__llmsh_t0 ]]; then
    local now=${EPOCHREALTIME//[.,]/} h c n re='^[A-Za-z0-9_./ =:,@%+-]*$'
    local ms=$(( (now - __llmsh_t0) / 1000 ))
    __llmsh_t0=
    h=$(HISTTIMEFORMAT= builtin history 1)
    if [[ $h =~ ^[[:space:]]*([0-9]+)[[:space:]]+(.*)$ ]]; then
      n=${BASH_REMATCH[1]} c=${BASH_REMATCH[2]}
      if [[ $n != "$__llmsh_hn" ]]; then
        __llmsh_hn=$n
        if [[ $c =~ $re ]]; then
          printf '\e]1337;llmsh;t=%s\a' "$c"
        else
          printf '\e]1337;llmsh;b=%s\a' "$(printf %s "$c" | base64 -w0)"
        fi
      fi
    fi
    printf '\e]133;D;%s\a' "$rc"
    if (( rc != 0 || ms > 5000 )); then
      local dur txt
      if (( ms >= 60000 )); then dur="$((ms / 60000))m$((ms % 60000 / 1000))s"
      else dur="$((ms / 1000)).$((ms % 1000 / 100))s"; fi
      if (( rc != 0 )); then
        txt=$rc; (( ms > 5000 )) && txt+=" · $dur"
        if [[ -n $NO_COLOR ]]; then printf ' ✗ %s\n' "$txt"
        else printf ' \e[31m✗\e[0m \e[2m%s\e[0m\n' "$txt"; fi
      else
        if [[ -n $NO_COLOR ]]; then printf ' ✓ %s\n' "$dur"
        else printf ' \e[32m✓\e[0m \e[2m%s\e[0m\n' "$dur"; fi
      fi
    fi
  else
    printf '\e]133;D;%s\a' "$rc"
  fi
  return $rc
}

# Runs last: cwd + (re)wrap PS1 with A/B marks, even if PROMPT_COMMAND set PS1.
__llmsh_post() {
  local rc=$?
  printf '\e]7;file://%s%s\a' "$HOSTNAME" "${PWD// /%20}"
  [[ $PS1 == *'133;B'* ]] || PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
  return $rc
}

if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
  PROMPT_COMMAND=(__llmsh_pre "${PROMPT_COMMAND[@]}" __llmsh_post)
else
  PROMPT_COMMAND="__llmsh_pre"$'\n'"${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}__llmsh_post"
fi
# C mark + start timestamp (assignment happens in the current shell).
PS0="${PS0}"'\e]133;C\a${BASH:$((__llmsh_t0=${EPOCHREALTIME//[.,]/})):0}'
