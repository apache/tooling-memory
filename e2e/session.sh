#!/bin/sh
# What every session runs under the browser tests: a banner naming an entry the record
# holds, so the terminal has a mem- id to link, then whatever is typed, echoed back.
# With the root as its second argument it also writes the status file the launcher
# would, under state/web/ by the id the web launcher gave it, so the strip has a status.
echo "e2e session: ready"
echo "read $(head -n 1 "$1")"
if [ -n "${2:-}" ] && [ -n "${AMEM_WEB_SESSION:-}" ]; then
    dir="$2/state/web"
    mkdir -p "$dir"
    printf '%s' '{"memory_session":"e2e","harness":"claude","auth":"gateway","destination":"llmao","ceiling_of":"llmao","ceiling":"internal","metered":true,"model":"model-the-launcher-reports","label":"internal","usage":{"calls":12,"input":41234,"output":3120},"written":3,"handoffs":1,"running":true,"updated":"2026-10-08T12:00:00Z"}' \
        >"$dir/$AMEM_WEB_SESSION.json.tmp"
    mv "$dir/$AMEM_WEB_SESSION.json.tmp" "$dir/$AMEM_WEB_SESSION.json"
fi
exec cat
