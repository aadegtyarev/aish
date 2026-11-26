# llmsh shell integration (generated; do not edit, overwritten on start)
# Written to survive `set -u` in user rc files: every expansion uses ${X-}.
# Session nonce (llmsh passes it in the environment): our OSC 133 marks carry it so
# marks of foreign shells (ssh, docker exec, nested integrations) are ignored.
__llmsh_n=${LLMSH_NONCE-}
unset LLMSH_NONCE
if [[ -n ${LLMSH_LOGIN-} ]]; then
  [[ -f /etc/profile ]] && source /etc/profile
  for __llmsh_f in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -f $__llmsh_f ]]; then source "$__llmsh_f"; break; fi
  done
  unset __llmsh_f LLMSH_LOGIN
else
  [[ -f /etc/bash.bashrc ]] && source /etc/bash.bashrc
  [[ -f ~/.bashrc ]] && source ~/.bashrc
fi

__llmsh_t0=
__llmsh_hn=
__llmsh_q=0
# 0: no integration (bash < 4.4, no PS0); 1: SECONDS timing; 2: EPOCHREALTIME (5.0+)
__llmsh_mode=0
if (( BASH_VERSINFO[0] >= 5 )); then __llmsh_mode=2
elif (( BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4 )); then __llmsh_mode=1; fi
__llmsh_funsub=0
if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 3) )); then
  __llmsh_funsub=1
fi

# Percent-encode $1 into $__llmsh_enc using bytes (C locale), capped at 8 KiB.
__llmsh_encode() {
  local LC_ALL=C s=${1:0:8192} ch i h
  __llmsh_enc=
  for (( i = 0; i < ${#s}; i++ )); do
    ch=${s:i:1}
    case $ch in
      [A-Za-z0-9_./=:,@+-]) __llmsh_enc+=$ch ;;
      *) printf -v h '%%%02X' "'$ch"; __llmsh_enc+=$h ;;
    esac
  done
}

# Runs first in PROMPT_COMMAND so that $1 is the user's command status.
__llmsh_pre() {
  local rc=${1:-0} h c n now ms dur txt
  if [[ -n ${__llmsh_t0-} ]]; then
    if (( __llmsh_mode == 2 )); then now=${EPOCHREALTIME//[.,]/}; else now=$(( SECONDS * 1000000 )); fi
    ms=$(( (now - __llmsh_t0) / 1000 ))
    __llmsh_t0=
    if (( __llmsh_funsub )); then
      eval 'h=${ HISTTIMEFORMAT= builtin history 1; }'
    else
      h=$(HISTTIMEFORMAT= builtin history 1)
    fi
    if [[ $h =~ ^[[:space:]]*([0-9]+)[[:space:]]+(.*)$ ]]; then
      n=${BASH_REMATCH[1]} c=${BASH_REMATCH[2]}
      if [[ $n != "${__llmsh_hn-}" ]]; then
        __llmsh_hn=$n
        __llmsh_encode "$c"
        printf '\e]1337;llmsh;p=%s\a' "$__llmsh_enc"
      fi
    fi
    printf '\e]133;D;%s;llmsh=%s\a' "$rc" "$__llmsh_n"
    if (( rc != 0 || ms > 5000 )); then
      if (( ms >= 60000 )); then dur="$((ms / 60000))m$((ms % 60000 / 1000))s"
      else dur="$((ms / 1000)).$((ms % 1000 / 100))s"; fi
      if (( rc != 0 )); then
        txt=$rc; (( ms > 5000 )) && txt+=" · $dur"
        if [[ -n ${NO_COLOR-} ]]; then printf ' ✗ %s\n' "$txt"
        else printf ' \e[31m✗\e[0m \e[2m%s\e[0m\n' "$txt"; fi
      else
        if [[ -n ${NO_COLOR-} ]]; then printf ' ✓ %s\n' "$dur"
        else printf ' \e[32m✓\e[0m \e[2m%s\e[0m\n' "$dur"; fi
      fi
    fi
  else
    printf '\e]133;D;%s;llmsh=%s\a' "$rc" "$__llmsh_n"
  fi
  return "$rc"
}

# Runs last: cwd + (re)wrap PS1 with A/B marks, even if PROMPT_COMMAND set PS1.
__llmsh_post() {
  local rc=$?
  printf '\e]7;file://%s%s\a' "${HOSTNAME-}" "${PWD// /%20}"
  if [[ $PATH != "${__llmsh_path-}" ]]; then
    __llmsh_path=$PATH
    __llmsh_encode "$PATH"
    printf '\e]1337;llmsh;path=%s\a' "$__llmsh_enc"
  fi
  [[ ${PS1-} == *'133;B;llmsh='* ]] || PS1='\[\e]133;A;llmsh='"$__llmsh_n"'\a\]'"${PS1-}"'\[\e]133;B;llmsh='"$__llmsh_n"'\a\]'
  return $rc
}

if (( __llmsh_mode > 0 )); then
  __llmsh_h1='__llmsh_q=$?; declare -F __llmsh_pre >/dev/null && __llmsh_pre $__llmsh_q'
  __llmsh_h2='declare -F __llmsh_post >/dev/null && __llmsh_post'
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    PROMPT_COMMAND=("$__llmsh_h1" "${PROMPT_COMMAND[@]}" "$__llmsh_h2")
  else
    PROMPT_COMMAND="$__llmsh_h1"$'\n'"${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}$__llmsh_h2"
  fi
  # C mark + start timestamp (assignment must live in the current shell; the
  # substring-offset form is the one bash 5.x evaluates there).
  if (( __llmsh_mode == 2 )); then
    PS0="${PS0-}"'\e]133;C;llmsh='"$__llmsh_n"'\a${BASH:$((__llmsh_t0=${EPOCHREALTIME//[.,]/})):0}'
  else
    PS0="${PS0-}"'\e]133;C;llmsh='"$__llmsh_n"'\a${BASH:$((__llmsh_t0=SECONDS*1000000)):0}'
  fi
  # Don't leak into nested shells / tmux, which lack our functions.
  export -n PROMPT_COMMAND PS0
fi
# llmsh asks for a prompt repaint after stray output with this private key.
bind '"\e[1337~": redraw-current-line' 2>/dev/null
# Our bracketed-paste submit must not flash the pasted text in reverse video.
bind 'set enable-active-region off' 2>/dev/null
unset LLMSH __llmsh_h1 __llmsh_h2
