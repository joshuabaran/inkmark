#!/usr/bin/env bash
# Local end-to-end checks for the real Wayland window.
#
# They open a small floating window. On Hyprland its app id is inkmark-e2e:
# it floats, starts unfocused, and focus is given back to whatever was
# active. idle, shutdown, and big do not send keys. soak does real work
# about once a minute: temp files, a private theme folder, and a paste.
#
#   scripts/e2e.sh idle [seconds]       stay open this long (default 300)
#   scripts/e2e.sh shutdown [seconds]   close itself after this long (default 20)
#   scripts/e2e.sh big [seconds]        idle on the 5 MB Tolstoy file (default 180)
#   scripts/e2e.sh soak [seconds]       work every E2E_ACT_EVERY seconds (default 1800, every 90)
#
# Soak never touches the desktop theme. The process gets its own
# XDG_STATE_HOME, and the theme folder under that tree is replaced the way
# `omarchy theme set` replaces `current/theme`. A paste focuses inkmark-e2e
# for a moment and then returns focus. Text already on the clipboard is
# copied back afterwards. Temp files stay inside the script's temp dir.
#
# A failure leaves target/e2e-$mode.log (and the full Wayland debug log when
# it is small enough to keep). Idle and soak are stopped with SIGKILL so a
# known crash while the clipboard thread shuts down does not fire a desktop
# notification.
set -euo pipefail
cd "$(dirname "$0")/.."

mode=${1:-idle}
case "$mode" in
  idle) seconds=${2:-300} ;;
  shutdown) seconds=${2:-20} ;;
  big) seconds=${2:-180} ;;
  soak) seconds=${2:-1800} ;;
  *)
    echo "usage: scripts/e2e.sh idle [seconds] | shutdown [seconds] | big [seconds] | soak [seconds]" >&2
    exit 2
    ;;
esac

tag=$mode
if [ "$mode" = soak ]; then
  tag="soak-$seconds"
fi

cargo build --release -p inkmark
bin=$PWD/target/release/inkmark
tmp=$(mktemp -d)
log=$tmp/session.log
wayland_log=$tmp/wayland.log
pid=""
focus_before=""
work=""
state=""
soak_n=0
act_every=${E2E_ACT_EVERY:-90}
case "$act_every" in
  ''|*[!0-9]*) echo "E2E_ACT_EVERY must be a positive number of seconds" >&2; exit 2 ;;
esac
if [ "$act_every" -lt 1 ]; then
  echo "E2E_ACT_EVERY must be a positive number of seconds" >&2
  exit 2
fi
next_act=$act_every
theme_i=0
palettes=()
clipboard_dirty=0
clipboard_had_text=0
clipboard_saved=""

hypr_rule_on() {
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl repl 'if inkmark_e2e_rule then inkmark_e2e_rule:set_enabled(false) end
inkmark_e2e_rule = hl.window_rule({
  name = "inkmark-e2e",
  match = { class = "^inkmark-e2e$" },
  float = true,
  no_initial_focus = true,
  focus_on_activate = false,
  size = {480, 320},
  move = {"monitor_w-window_w-16", "monitor_h-window_h-16"},
})' >/dev/null 2>&1 || true
}

hypr_rule_off() {
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl repl 'if inkmark_e2e_rule then inkmark_e2e_rule:set_enabled(false) end' >/dev/null 2>&1 || true
}

window_here() {
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl clients -j 2>/dev/null | python3 -c '
import json, sys
cs = json.load(sys.stdin)
ok = any(
    (c.get("class") or "") == "inkmark-e2e" or (c.get("initialClass") or "") == "inkmark-e2e"
    for c in cs
)
sys.exit(0 if ok else 1)
'
}

restore_focus() {
  [ -n "$focus_before" ] || return 0
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl repl "hl.dispatch(hl.dsp.focus({ window = \"address:${focus_before}\" }))" >/dev/null 2>&1 || true
}

repl() {
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl repl "$1" >/dev/null 2>&1 || echo "hyprctl repl failed: $1"
}

window_addr() {
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl clients -j 2>/dev/null | python3 -c '
import json, sys
cs = json.load(sys.stdin)
for c in cs:
    klass = c.get("class") or ""
    initial = c.get("initialClass") or ""
    if klass == "inkmark-e2e" or initial == "inkmark-e2e":
        addr = c.get("address") or ""
        if addr.startswith("address:"):
            addr = addr[len("address:"):]
        print(addr)
        break
'
}

active_addr() {
  command -v hyprctl >/dev/null 2>&1 || return 0
  hyprctl activewindow -j 2>/dev/null | python3 -c '
import json, sys
try:
    addr = json.load(sys.stdin).get("address") or ""
except Exception:
    addr = ""
if addr.startswith("address:"):
    addr = addr[len("address:"):]
print(addr)
' || true
}

# Put back whatever text was on the clipboard before a paste. A picture or
# other non-text offer is never overwritten, so there is nothing to restore.
restore_clipboard() {
  [ "$clipboard_dirty" = 1 ] || return 0
  if [ "$clipboard_had_text" = 1 ]; then
    printf '%s' "$clipboard_saved" | wl-copy --type text/plain
  else
    wl-copy --clear
  fi
  clipboard_dirty=0
}

paste_or_type() {
  local text=$1
  local addr back types
  command -v hyprctl >/dev/null 2>&1 || return 0
  command -v wl-copy >/dev/null 2>&1 || return 0
  addr=$(window_addr)
  [ -n "$addr" ] || { echo "paste skipped: no inkmark-e2e window"; return 0; }
  back=$(active_addr)
  types=$(wl-paste -l 2>/dev/null || true)
  clipboard_had_text=0
  clipboard_saved=""
  if printf '%s\n' "$types" | grep -qx 'text/plain'; then
    clipboard_saved=$(wl-paste --no-newline --type text/plain 2>/dev/null || true)
    clipboard_had_text=1
    # A huge clipboard stays put. Typing still exercises the editor.
    if [ "${#clipboard_saved}" -gt 1000000 ]; then
      echo "clipboard left alone (large); typing instead"
      type_chars "$text" "$addr" "$back"
      return 0
    fi
  elif [ -n "$types" ]; then
    echo "clipboard left alone (not text); typing instead"
    type_chars "$text" "$addr" "$back"
    return 0
  fi
  printf '%s' "$text" | wl-copy --type text/plain || {
    echo "wl-copy failed"
    return 0
  }
  clipboard_dirty=1
  # The seat only offers the clipboard to the focused window.
  repl "hl.dispatch(hl.dsp.focus({ window = \"address:${addr}\" }))"
  sleep 0.05
  repl "hl.dispatch(hl.dsp.send_key_state({ mods = \"CTRL\", key = \"V\", state = \"down\", window = \"address:${addr}\" }))"
  sleep 0.05
  repl "hl.dispatch(hl.dsp.send_key_state({ mods = \"CTRL\", key = \"V\", state = \"up\", window = \"address:${addr}\" }))"
  # Stay focused while the data transfer finishes, then go back.
  sleep 0.25
  if [ -n "$back" ] && [ "$back" != "$addr" ]; then
    repl "hl.dispatch(hl.dsp.focus({ window = \"address:${back}\" }))"
  fi
  restore_clipboard
}

type_chars() {
  local text=$1 addr=$2 back=$3
  local i ch
  repl "hl.dispatch(hl.dsp.focus({ window = \"address:${addr}\" }))"
  sleep 0.05
  for ((i = 0; i < ${#text}; i++)); do
    ch=${text:i:1}
    case "$ch" in
      [a-z0-9]) ;;
      *) continue ;;
    esac
    repl "hl.dispatch(hl.dsp.send_key_state({ key = \"${ch}\", state = \"down\", window = \"address:${addr}\" }))"
    repl "hl.dispatch(hl.dsp.send_key_state({ key = \"${ch}\", state = \"up\", window = \"address:${addr}\" }))"
  done
  if [ -n "$back" ] && [ "$back" != "$addr" ]; then
    repl "hl.dispatch(hl.dsp.focus({ window = \"address:${back}\" }))"
  fi
}

soak_setup() {
  work=$tmp/work
  state=$tmp/state
  mkdir -p "$work" "$state/omarchy/current/theme"
  printf '# soak\n\n' > "$work/note.md"
  note=$work/note.md
  local dark="" light="" palette
  for palette in /usr/share/omarchy/themes/*/colors.toml; do
    [ -f "$palette" ] || continue
    if [ -z "$dark" ] && grep -q 'mode = "dark"' "$palette"; then
      dark=$palette
    elif [ -z "$light" ] && grep -q 'mode = "light"' "$palette"; then
      light=$palette
    fi
    [ -n "$dark" ] && [ -n "$light" ] && break
  done
  if [ -z "$dark" ] || [ -z "$light" ]; then
    dark=$tmp/dark.toml
    light=$tmp/light.toml
    printf 'mode = "dark"\nbackground = "#101010"\nforeground = "#e8e8e8"\n' > "$dark"
    printf 'mode = "light"\nbackground = "#f7f7f5"\nforeground = "#1c1c1c"\n' > "$light"
  fi
  palettes=("$dark" "$light")
  cp "${palettes[0]}" "$state/omarchy/current/theme/colors.toml"
  theme_i=0
}

soak_act() {
  case "$work" in
    "$tmp"/*) ;;
    *) echo "refusing to touch $work"; return 0 ;;
  esac
  local theme_dir=$state/omarchy/current/theme
  case "$theme_dir" in
    "$tmp"/state/omarchy/current/theme) ;;
    *) echo "refusing theme path $theme_dir"; return 0 ;;
  esac
  # A failed file or paste step should not end the run. The run ends if
  # the window dies.
  set +e
  soak_n=$((soak_n + 1))
  local dir=$work/folder-$soak_n
  mkdir -p "$dir"
  printf '# note %s\n\nTemp file for the soak.\n' "$soak_n" > "$dir/child.md"
  mv "$dir/child.md" "$dir/renamed.md"
  if [ "$soak_n" -gt 1 ]; then
    rm -rf "$work/folder-$((soak_n - 1))"
  fi
  theme_i=$(( (theme_i + 1) % ${#palettes[@]} ))
  rm -rf "$theme_dir"
  mkdir -p "$theme_dir"
  cp "${palettes[$theme_i]}" "$theme_dir/colors.toml"
  paste_or_type "soak${soak_n}"
  local theme_name title=""
  theme_name=$(basename "$(dirname "${palettes[$theme_i]}")")
  if command -v hyprctl >/dev/null 2>&1; then
    title=$(hyprctl clients -j 2>/dev/null | python3 -c '
import json, sys
cs = json.load(sys.stdin)
for c in cs:
    klass = c.get("class") or ""
    initial = c.get("initialClass") or ""
    if klass == "inkmark-e2e" or initial == "inkmark-e2e":
        print(c.get("title") or "")
        break
' || true)
  fi
  echo "action $soak_n create/rename/delete + theme + paste at t=${i}s ($theme_name) title=${title}"
  set -e
  return 0
}

cleanup() {
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  restore_clipboard || true
  hypr_rule_off
  rm -rf "$tmp"
}
trap cleanup EXIT

note=$tmp/note.md
if [ "$mode" = big ]; then
  big=${E2E_BIG:-$HOME/Projects/inkmark/tolstoy.md}
  if [ ! -f "$big" ]; then
    echo "no 5 MB file at $big" >&2
    exit 2
  fi
  note=$big
elif [ "$mode" = soak ]; then
  soak_setup
else
  printf '# e2e %s\n\nidle\n' "$mode" > "$note"
fi

launch_env=(
  INKMARK_E2E=1
  INKMARK_LOG="$log"
  INKMARK_HEARTBEAT_SECS=5
  RUST_BACKTRACE=1
  WAYLAND_DEBUG=1
)
if [ "$mode" = shutdown ]; then
  launch_env+=(INKMARK_E2E_QUIT_AFTER="$seconds")
fi
if [ "$mode" = soak ]; then
  launch_env+=(XDG_STATE_HOME="$state")
fi

if command -v hyprctl >/dev/null 2>&1; then
  focus_before=$(hyprctl activewindow -j 2>/dev/null | python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("address") or "")
except Exception:
    print("")
' || true)
fi
hypr_rule_on

echo "e2e $mode for ${seconds}s"
echo "log $log"

env "${launch_env[@]}" "$bin" "$note" >"$wayland_log" 2>&1 &
pid=$!

keep_summary() {
  mkdir -p "$PWD/target"
  local kept=$PWD/target/e2e-$tag.log
  {
    echo "# e2e $mode"
    grep -E '^[0-9]{4}-' "$log" || true
    echo "# wayland lines matching error, protocol, closed, or fatal"
    if [ -f "$wayland_log" ]; then
      grep -Ei 'error|protocol|closed|fatal' "$wayland_log" | grep -v '^[0-9]\{4\}-' | tail -40 || true
    fi
  } > "$kept"
  echo "kept $kept"
}

fail() {
  local code=$1
  local dead=$2
  echo "FAILED $mode pid=$dead exit=$code"
  echo "log $log"
  if [ -f "$log" ]; then
    echo "--- session lines ---"
    grep -E '^[0-9]{4}-' "$log" | tail -40 || true
    echo "--- wayland errors ---"
    if [ -f "$wayland_log" ]; then
      grep -Ei 'error|protocol|closed|fatal' "$wayland_log" | grep -v '^[0-9]\{4\}-' | tail -20 || true
    fi
  fi
  coredumpctl info "$dead" --no-pager 2>&1 | head -30 || true
  keep_summary
  # Leave pid set so the trap SIGKILLs a process that is still up.
  if [ -f "$log" ]; then
    cp "$log" "$PWD/target/e2e-$tag-full.log" 2>/dev/null || true
    echo "kept $PWD/target/e2e-$tag-full.log"
  fi
  if [ -f "$wayland_log" ]; then
    local bytes
    bytes=$(wc -c < "$wayland_log" || echo 0)
    if [ "$bytes" -lt 20000000 ]; then
      cp "$wayland_log" "$PWD/target/e2e-$tag-wayland.log" 2>/dev/null || true
      echo "kept $PWD/target/e2e-$tag-wayland.log"
    fi
  fi
  exit 1
}

if [ "$mode" = shutdown ]; then
  limit=$((seconds + 30))
  restored=0
  for ((i = 0; i < limit; i++)); do
    if window_here && [ "$restored" -eq 0 ]; then
      restore_focus
      restored=1
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      set +e
      wait "$pid"
      code=$?
      set -e
      if [ "$code" -ne 0 ]; then
        fail "$code" "$pid"
      fi
      pid=""
      if ! grep -q 'event_loop returned ok' "$log"; then
        echo "shutdown exited 0 without an event_loop line"
        fail 0 0
      fi
      keep_summary
      echo "shutdown ok"
      exit 0
    fi
    sleep 1
  done
  echo "shutdown did not exit"
  fail 124 "$pid"
fi

seen=0
restored=0
for ((i = 0; i < seconds; i++)); do
  if ! kill -0 "$pid" 2>/dev/null; then
    set +e
    wait "$pid"
    code=$?
    set -e
    fail "$code" "$pid"
  fi
  if window_here; then
    seen=1
    if [ "$restored" -eq 0 ]; then
      restore_focus
      restored=1
    fi
  elif [ "$seen" -eq 1 ]; then
    echo "window disappeared while pid $pid was still running"
    fail 0 "$pid"
  elif [ "$i" -eq 20 ]; then
    echo "window never appeared"
    fail 0 "$pid"
  fi
  if [ "$mode" = soak ] && [ "$seen" -eq 1 ] && [ "$i" -ge "$next_act" ]; then
    soak_act || echo "action failed at t=${i}s"
    next_act=$((i + act_every))
  fi
  sleep 1
done

if [ "$mode" = soak ]; then
  echo "soak ok (${seconds}s, ${soak_n} actions, pid $pid still running)"
else
  echo "idle ok (${seconds}s, pid $pid still running)"
fi
# SIGKILL skips the clipboard-thread drop, which has segfaulted on purpose
# in a smaller repro and would post a crash notification.
kill -KILL "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
pid=""
keep_summary
grep -E '^[0-9]{4}-' "$log" | tail -5 || true
