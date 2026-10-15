#!/bin/sh
# Stands in for the go2rtc binary in tests: records its argv, then idles
# until the test kills it. `exec` matters: the kill must reach the sleeping
# process itself, or it outlives the test as a leak.
printf '%s\n' "$@" > /tmp/nazar-fake-go2rtc-args.txt
exec sleep 60
