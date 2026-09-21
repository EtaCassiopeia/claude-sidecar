#!/bin/bash
#
# <xbar.title>Claude Sidecar</xbar.title>
# <xbar.version>v1.0</xbar.version>
# <xbar.author>dzm226</xbar.author>
# <xbar.desc>Menu-bar readout of claude-sidecar job activity.</xbar.desc>
# <xbar.dependencies>bash,curl,jq</xbar.dependencies>
#
# Install: cp to "~/Library/Application Support/xbar/plugins/sidecar.5s.sh"
# The "5s" in the filename is xbar's refresh interval — rename to change it.
#
# Read-only: polls GET /health and GET /jobs. Nothing here mutates sidecar state.

export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

PORT="${SIDECAR_PORT:-8765}"
BASE="http://localhost:${PORT}"

# Gruvbox, to match the Ghostty theme.
GREEN="#b8bb26"
RED="#cc241d"
GRAY="#928374"
YELLOW="#fabd2f"

# Keep the timeout well under the refresh interval — a hung request must never
# stall the menu bar.
health=$(curl -s --max-time 2 "${BASE}/health" 2>/dev/null)

if [[ -z "$health" ]]; then
  echo "○ ✗ | color=${RED}"
  echo "---"
  echo "sidecar not running on :${PORT} | color=${GRAY}"
  echo "Start it with: claude-sidecar & | color=${GRAY}"
  exit 0
fi

version=$(printf '%s' "$health" | jq -r '.version // "?"')
jobs=$(curl -s --max-time 2 "${BASE}/jobs" 2>/dev/null)
[[ -z "$jobs" ]] && jobs="[]"

running=$(printf '%s' "$jobs" | jq '[.[] | select(.running)] | length')

if [[ "$running" -gt 0 ]]; then
  echo "● ${running} | color=${GREEN}"
else
  echo "○ | color=${GRAY}"
fi

echo "---"
echo "sidecar :${PORT}  v${version} | color=${GRAY}"
echo "---"

printf '%s' "$jobs" | jq -r --arg green "$GREEN" --arg red "$RED" \
  --arg gray "$GRAY" --arg yellow "$YELLOW" '
  def dur:
    (. / 1000 | floor) as $s
    | if   $s < 60   then "\($s)s"
      elif $s < 3600 then "\($s / 60 | floor)m\($s % 60)s"
      else                "\($s / 3600 | floor)h\(($s % 3600) / 60 | floor)m"
      end;

  # Not "label" — that is a reserved word in jq.
  def title:
    ([.cmd] + .args | join(" ")) as $full
    | if ($full | length) > 44 then ($full[:43] + "…") else $full end;

  # `outcome` is an object — {"outcome":"completed","exit_code":0} — because the
  # server serializes the enum with an internal tag. The name is the inner field,
  # not the object itself.
  def outcome_name: .outcome.outcome // "completed";

  # A running job that has been silent for minutes is the wedged-process
  # signature the sidecar tracks idle_ms for — surface it rather than letting it
  # look like a slow build. Only meaningful while running: idle_ms keeps growing
  # after a job finishes, so a finished job would always look "idle".
  def marks:
    if .running then
      (if .idle_ms > 120000 then " ⚠ idle \(.idle_ms | dur)" else "" end)
    else
      # A non-completed outcome already explains itself; "exit ?" alongside it is
      # noise, since a signalled process has no exit code to report.
      (outcome_name
       | if . != "completed" then " \(.)"
         else "" end) as $why
      | (if $why != "" then $why
         elif .exit_code == 0 then ""
         else " exit \(.exit_code)" end)
      + (if .outcome.escalated == true then " (killed)" else "" end)
    end;

  def color:
    if .running               then $green
    elif outcome_name != "completed" then $yellow
    elif .exit_code == 0      then $gray
    else                           $red
    end;

  if length == 0 then
    "no jobs | color=\($gray)"
  else
    # Running first, then most-recently-started.
    sort_by([(if .running then 0 else 1 end), .elapsed_ms])[]
    | "\(if .running then "▶" else (if .exit_code == 0 then "✓" else "✗" end) end) " +
      "\(title)  \(.elapsed_ms | dur)  \(.line_count) ln\(marks)" +
      " | color=\(color) font=Menlo size=12"
  end
'
