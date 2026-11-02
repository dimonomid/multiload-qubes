# Run inside a Linux qube. Shell built-ins handle each sample without child processes.
set -eu
domid=$(xenstore-read domid)

# EOF from the collector closes this loop. Removing the panel plugin alone
# does not stop the collector or its connections. There is no sampling timer here.
while IFS= read -r command; do
    [ "$command" = sample ] || break
    # Clear every field so a missing value cannot reuse the previous sample.
    mem_total= mem_available= swap_total= swap_free=
    while read -r key value unit; do
        case "$key" in
            MemTotal:) mem_total=$value ;;
            MemAvailable:) mem_available=$value ;;
            SwapTotal:) swap_total=$value ;;
            SwapFree:) swap_free=$value ;;
        esac
    done < /proc/meminfo
    [ -n "$mem_total" ] && [ -n "$mem_available" ] &&
        [ -n "$swap_total" ] && [ -n "$swap_free" ] || exit 1
    # Keep the same domain ID check and RAM/swap reply format used by the parser.
    printf 'M2 %s %s %s %s %s\n' "$domid" "$mem_total" "$mem_available" "$swap_total" "$swap_free"
done
