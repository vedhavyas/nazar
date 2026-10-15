#!/bin/sh
# Stands in for the go2rtc binary in tests: records its argv beside the
# config it was handed (unique per test, so concurrent tests cannot read
# each other's), then idles until the test kills it. `exec` matters: the
# kill must reach the sleeping process itself, or it outlives the test as a
# leak.
printf '%s\n' "$@" > "$2.args"
exec sleep 60
