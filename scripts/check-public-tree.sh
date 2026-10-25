#!/usr/bin/env bash
set -euo pipefail

failed=0
while IFS= read -r -d '' file; do
    case "$file" in
        .cache/*|captures/*|design/*|target/*|.env|.env.*|*.pem|*.key|*.log|config.json|energy.json|energy.lock|battery-history.json|battery-history.lock|protection-state.json)
            printf 'Private or generated file is tracked: %s\n' "$file" >&2
            failed=1
            ;;
    esac
    if grep -nE -- '/(home|Users)/[^/[:space:]]+|[A-Za-z]:\\Users\\' "$file"; then
        printf 'Machine-specific path in %s\n' "$file" >&2
        failed=1
    fi
done < <(git ls-files -z)

exit "$failed"
