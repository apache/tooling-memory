# How the demo shows itself: each act and each step opens with a rule, a title and a
# short description of what is about to be shown, so a transcript reads as a script.
# Sourced by scripts/demo.sh (act 1) and scripts/demo/lib.sh (the others).
#
#   act <n> "<title>" "<what the act shows>"
#   say "<title>" "<what the step shows>"        numbered <act>.<step>
#
# Bold on a terminal only, so a captured transcript is plain text.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then _b=$(printf '\033[1m'); _r=$(printf '\033[0m'); else _b=; _r=; fi
_rule() { printf '%78s\n' '' | tr ' ' "$1"; }
_wrap() { [ -n "$2" ] && printf '%s\n' "$2" | fold -s -w $((78 - $1)) | sed "s/^/$(printf '%*s' "$1" '')/;s/ *$//"; return 0; }
DEMO_ACT=${DEMO_ACT:-0}
DEMO_STEP=0
act() {
  DEMO_ACT=$1; DEMO_STEP=0
  printf '\n'; _rule =
  printf '%sAct %s · %s%s\n' "$_b" "$1" "$2" "$_r"
  _wrap 0 "${3:-}"
  _rule =
}
say() {
  DEMO_STEP=$((DEMO_STEP + 1))
  printf '\n'; _rule -
  printf '%s%s.%s  %s%s\n' "$_b" "$DEMO_ACT" "$DEMO_STEP" "$1" "$_r"
  _wrap 5 "${2:-}"
  _rule -
}
