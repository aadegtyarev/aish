# aish completion sidecar: a non-interactive-looking bash that answers
# completion requests from aish. Requests arrive on stdin as NUL-terminated
# fields, replies go to fd 3 (stdout/stderr are discarded so that nothing in the
# user's rc files can corrupt the protocol).
exec 3>&1 1>/dev/null 2>&1
set +eu +o history +H 2>/dev/null
unset HISTFILE
export AISH_SIDECAR=1
shopt -s progcomp extglob 2>/dev/null

# The user's rc must not replace or end this shell.
exec() { return 0; }
exit() { return 0; }
logout() { return 0; }
if [[ -r /usr/share/bash-completion/bash_completion ]]; then
  . /usr/share/bash-completion/bash_completion </dev/null
fi
if [[ -r ${HOME-}/.bashrc ]]; then
  . "$HOME/.bashrc" </dev/null
fi
unset -f exec exit logout
set +eu +o history +H 2>/dev/null
trap - DEBUG ERR EXIT 2>/dev/null
unset PROMPT_COMMAND

__lc_opts=
# Completion functions call compopt, which only works inside real completion;
# record the options instead.
compopt() {
  while (($#)); do
    case $1 in
      -o) __lc_opts+=" $2 "; shift ;;
      +o) __lc_opts=${__lc_opts// $2 /} ; shift ;;
    esac
    shift
  done
  return 0
}

__lc_files() {
  local cur=$1
  mapfile -t COMPREPLY < <(compgen -f -- "$cur")
  __lc_opts+=" filenames "
}

__lc_gen() {
  local cmd=${COMP_WORDS[0]} cur=${COMP_WORDS[COMP_CWORD]} prev=
  (( COMP_CWORD > 0 )) && prev=${COMP_WORDS[COMP_CWORD-1]}
  [[ $cur == [\"\']* ]] && cur=${cur:1}
  COMPREPLY=()
  __lc_opts=
  if (( COMP_CWORD == 0 )); then
    if [[ $cur == */* || $cur == \~* ]]; then
      __lc_files "$cur"
    else
      mapfile -t COMPREPLY < <(compgen -c -- "$cur")
    fi
    return
  fi
  if [[ $cur == \$* && $cur != *[!\$A-Za-z0-9_]* ]]; then
    mapfile -t COMPREPLY < <(compgen -v -P '$' -- "${cur#\$}")
    return
  fi
  local spec tries=0
  while :; do
    spec=$(complete -p -- "$cmd" 2>/dev/null)
    if [[ -z $spec ]] && (( tries++ == 0 )) && declare -F _completion_loader >/dev/null; then
      _completion_loader "$cmd" >/dev/null 2>&1 </dev/null
      continue
    fi
    break
  done
  if [[ -z $spec ]]; then
    __lc_files "$cur"
    return
  fi
  local -a args=() copts=()
  eval "args=(${spec#complete })" 2>/dev/null
  local fn= i=0 a
  while (( i < ${#args[@]} )); do
    a=${args[i]}
    case $a in
      -F) fn=${args[++i]} ;;
      -C) ((i++)) ;;
      -o) __lc_opts+=" ${args[++i]} " ;;
      -A|-G|-W|-X|-P|-S) copts+=("$a" "${args[++i]}") ;;
      -[abcdefgjksuv]) copts+=("$a") ;;
    esac
    ((i++))
  done
  local -a out=()
  if (( ${#copts[@]} )); then
    mapfile -t out < <(compgen "${copts[@]}" -- "$cur")
  fi
  if [[ -n $fn ]]; then
    COMPREPLY=()
    "$fn" "$cmd" "$cur" "$prev" >/dev/null 2>&1
    out+=("${COMPREPLY[@]}")
  fi
  COMPREPLY=("${out[@]}")
  if (( ${#COMPREPLY[@]} == 0 )); then
    if [[ $__lc_opts == *" default "* || $__lc_opts == *" bashdefault "* ]]; then
      __lc_files "$cur"
    elif [[ $__lc_opts == *" dirnames "* ]]; then
      mapfile -t COMPREPLY < <(compgen -d -- "$cur")
      __lc_opts+=" filenames "
    fi
  fi
}

__lc_loop() {
  local id kind cwd cword n i line
  local -a words
  while IFS= read -r -d '' id && IFS= read -r -d '' kind; do
    case $kind in
      L)
        local -a names
        mapfile -t names < <(compgen -a -b -k -A function)
        printf '%s\0' "$id" "-" "$PATH" >&3
        (( ${#names[@]} )) && printf '%s\0' "${names[@]}" >&3
        printf '\0' >&3
        ;;
      C)
        IFS= read -r -d '' cwd
        IFS= read -r -d '' cword
        IFS= read -r -d '' n
        words=()
        for ((i = 0; i < n; i++)); do IFS= read -r -d '' w; words+=("$w"); done
        IFS= read -r -d '' line
        cd -- "$cwd" 2>/dev/null
        COMP_WORDS=("${words[@]}")
        COMP_CWORD=$cword
        COMP_LINE=$line
        COMP_POINT=${#line}
        COMP_TYPE=9
        COMP_KEY=9
        COMPREPLY=()
        # stdin is the request pipe: completion code must never read from it
        __lc_gen </dev/null
        local flags=-
        [[ $__lc_opts == *" filenames "* ]] && flags+=f
        [[ $__lc_opts == *" nospace "* ]] && flags+=n
        printf '%s\0' "$id" "$flags" >&3
        (( ${#COMPREPLY[@]} )) && printf '%s\0' "${COMPREPLY[@]}" >&3
        printf '\0' >&3
        ;;
    esac
  done
}

printf '%s\0' R - >&3
printf '\0' >&3
__lc_loop
