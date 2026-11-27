# aish shell integration (generated; do not edit, overwritten on start)
# Written to survive `set -u` in user rc files: every expansion uses ${X-}.
# Session nonce (aish passes it in the environment): our OSC 133 marks carry it so
# marks of foreign shells (ssh, docker exec, nested integrations) are ignored.
__aish_n=${AISH_NONCE-}
unset AISH_NONCE
if [[ -n ${AISH_LOGIN-} ]]; then
  [[ -f /etc/profile ]] && source /etc/profile
  for __aish_f in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -f $__aish_f ]]; then source "$__aish_f"; break; fi
  done
  unset __aish_f AISH_LOGIN
else
  [[ -f /etc/bash.bashrc ]] && source /etc/bash.bashrc
  [[ -f ~/.bashrc ]] && source ~/.bashrc
fi

__aish_t0=
__aish_hn=
__aish_q=0
# 0: no integration (bash < 4.4, no PS0); 1: SECONDS timing; 2: EPOCHREALTIME (5.0+)
__aish_mode=0
if (( BASH_VERSINFO[0] >= 5 )); then __aish_mode=2
elif (( BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4 )); then __aish_mode=1; fi
__aish_funsub=0
if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 3) )); then
  __aish_funsub=1
fi

# Percent-encode $1 into $__aish_enc using bytes (C locale), capped at 8 KiB.
__aish_encode() {
  local LC_ALL=C s=${1:0:8192} ch i h
  __aish_enc=
  for (( i = 0; i < ${#s}; i++ )); do
    ch=${s:i:1}
    case $ch in
      [A-Za-z0-9_./=:,@+-]) __aish_enc+=$ch ;;
      *) printf -v h '%%%02X' "'$ch"; __aish_enc+=$h ;;
    esac
  done
}

# Runs first in PROMPT_COMMAND so that $1 is the user's command status.
__aish_pre() {
  local rc=${1:-0} h c n now ms dur txt
  if [[ -n ${__aish_t0-} ]]; then
    if (( __aish_mode == 2 )); then now=${EPOCHREALTIME//[.,]/}; else now=$(( SECONDS * 1000000 )); fi
    ms=$(( (now - __aish_t0) / 1000 ))
    __aish_t0=
    if (( __aish_funsub )); then
      eval 'h=${ HISTTIMEFORMAT= builtin history 1; }'
    else
      h=$(HISTTIMEFORMAT= builtin history 1)
    fi
    if [[ $h =~ ^[[:space:]]*([0-9]+)[[:space:]]+(.*)$ ]]; then
      n=${BASH_REMATCH[1]} c=${BASH_REMATCH[2]}
      if [[ $n != "${__aish_hn-}" ]]; then
        __aish_hn=$n
        __aish_encode "$c"
        printf '\e]1337;aish;p=%s\a' "$__aish_enc"
      fi
    fi
    printf '\e]133;D;%s;aish=%s\a' "$rc" "$__aish_n"
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
    printf '\e]133;D;%s;aish=%s\a' "$rc" "$__aish_n"
  fi
  return "$rc"
}

# Background jobs. `__aish_jt` is the live table last reported (private OSC
# `jobs=`: id, pid, state, command per row). From the prompt hook only the table
# is reported. aish pokes `__aish_jpoll` (a bind -x key) while idle at the prompt
# once a job's pid has died: by then bash has printed its own "[1]+ Done" line (aish
# swallows it) and forgotten the job, so the status comes from `wait`, and the
# row is reported as finished (`jobend=`). `jobs` is parsed in the C locale.
__aish_jt=
__aish_jobs() {
  local out line rest id pid state cmd tbl= ends= clear= row st
  if [[ -z $__aish_jt ]] && ! compgen -j >/dev/null 2>&1; then return 0; fi
  out=$(LC_ALL=C jobs -l 2>/dev/null)
  while IFS= read -r line; do
    [[ $line =~ ^\[([0-9]+)\][+-]?[[:space:]]+([0-9]+)[[:space:]]+(.*)$ ]] || continue
    id=${BASH_REMATCH[1]} pid=${BASH_REMATCH[2]} rest=${BASH_REMATCH[3]}
    state=${rest%%  *}; cmd=${rest#"$state"}; cmd=${cmd#"${cmd%%[! ]*}"}; cmd=${cmd% &}
    case $state in
      Running|Stopped*) tbl+="$id"$'\t'"$pid"$'\t'"${state%% *}"$'\t'"$cmd"$'\n' ;;
      *) clear=1
         case $state in Done) st=0 ;; "Exit "*) st=${state#Exit } ;; *) st="sig:$state" ;; esac
         ends+="$id"$'\t'"$pid"$'\t'"$st"$'\t'"$cmd"$'\n' ;;
    esac
  done <<<"$out"
  [[ -n $clear ]] && jobs >/dev/null 2>&1
  if [[ ${1-} == poll ]]; then
    while IFS= read -r row; do
      [[ -n $row ]] || continue
      IFS=$'\t' read -r id pid state cmd <<<"$row"
      [[ $'\n'$tbl == *$'\n'"$id"$'\t'"$pid"$'\t'* || $'\n'$ends == *$'\n'"$id"$'\t'"$pid"$'\t'* ]] && continue
      kill -0 "$pid" 2>/dev/null && continue
      wait "$pid" 2>/dev/null; st=$?
      (( st == 127 )) && st=?
      ends+="$id"$'\t'"$pid"$'\t'"$st"$'\t'"$cmd"$'\n'
    done <<<"$__aish_jt"
  fi
  if [[ $tbl != "$__aish_jt" ]]; then
    __aish_jt=$tbl; __aish_encode "$tbl"; printf '\e]1337;aish;jobs=%s\a' "$__aish_enc"
  fi
  if [[ -n $ends && ${1-} == poll ]]; then
    __aish_encode "$ends"; printf '\e]1337;aish;jobend=%s\a' "$__aish_enc"
  fi
  return 0
}
__aish_jpoll() { __aish_jobs poll; }

# Runs last: cwd + (re)wrap PS1 with A/B marks, even if PROMPT_COMMAND set PS1.
__aish_post() {
  local rc=$?
  printf '\e]7;file://%s%s\a' "${HOSTNAME-}" "${PWD// /%20}"
  if [[ $PATH != "${__aish_path-}" ]]; then
    __aish_path=$PATH
    __aish_encode "$PATH"
    printf '\e]1337;aish;path=%s\a' "$__aish_enc"
  fi
  __aish_jobs prompt
  [[ ${PS1-} == *'133;B;aish='* ]] || PS1='\[\e]133;A;aish='"$__aish_n"'\a\]'"${PS1-}"'\[\e]133;B;aish='"$__aish_n"'\a\]'
  return $rc
}

if (( __aish_mode > 0 )); then
  __aish_h1='__aish_q=$?; declare -F __aish_pre >/dev/null && __aish_pre $__aish_q'
  __aish_h2='declare -F __aish_post >/dev/null && __aish_post'
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    PROMPT_COMMAND=("$__aish_h1" "${PROMPT_COMMAND[@]}" "$__aish_h2")
  else
    PROMPT_COMMAND="$__aish_h1"$'\n'"${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}$__aish_h2"
  fi
  # C mark + start timestamp (assignment must live in the current shell; the
  # substring-offset form is the one bash 5.x evaluates there).
  if (( __aish_mode == 2 )); then
    PS0="${PS0-}"'\e]133;C;aish='"$__aish_n"'\a${BASH:$((__aish_t0=${EPOCHREALTIME//[.,]/})):0}'
  else
    PS0="${PS0-}"'\e]133;C;aish='"$__aish_n"'\a${BASH:$((__aish_t0=SECONDS*1000000)):0}'
  fi
  # Don't leak into nested shells / tmux, which lack our functions.
  export -n PROMPT_COMMAND PS0
fi
# aish asks for a prompt repaint after stray output with this private key.
bind '"\e[1337~": redraw-current-line' 2>/dev/null
bind -x '"\e[1338~": __aish_jpoll' 2>/dev/null
# Our bracketed-paste submit must not flash the pasted text in reverse video.
bind 'set enable-active-region off' 2>/dev/null
unset AISH __aish_h1 __aish_h2
