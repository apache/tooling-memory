#!/bin/sh
# What every session runs under the browser tests: a banner naming an entry the record
# holds, so the terminal has a mem- id to link, then whatever is typed, echoed back.
echo "e2e session: ready"
echo "read $(head -n 1 "$1")"
exec cat
