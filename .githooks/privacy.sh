# Shared by the hooks in this directory. Nothing personal is stored here: the
# blocked text and the allowed email come from this clone's own git config,
# which is never committed:
#
#   git config core.hooksPath .githooks
#   git config privacy.email you@example.com         # the only email commits may carry
#   git config --add privacy.block "your-employer"   # case-insensitive fixed text, repeatable
#
# With neither set, the hooks check nothing.

privacy_patterns=$(mktemp)
privacy_scratch=$(mktemp)
trap 'rm -f "$privacy_patterns" "$privacy_scratch"' EXIT
# Lowercased here and in check_file: Git for Windows' grep 3.0 aborts on -i -F
# with several patterns.
git config --get-all privacy.block | grep -v '^$' | tr '[:upper:]' '[:lower:]' > "$privacy_patterns"
privacy_email=$(git config privacy.email)
fail=0

privacy_block() {
    echo "privacy: $1" >&2
    fail=1
}

# check_email EMAIL LABEL
check_email() {
    if [ -n "$privacy_email" ] && [ "$1" != "$privacy_email" ]; then
        privacy_block "$2 email is $1, not $privacy_email (privacy.email)"
    fi
    if [ -s "$privacy_patterns" ] &&
        printf '%s\n' "$1" | tr '[:upper:]' '[:lower:]' | grep -q -F -f "$privacy_patterns"; then
        privacy_block "$2 email $1 contains blocked text (privacy.block)"
    fi
}

# check_file FILE LABEL: refuse when FILE contains any blocked text.
check_file() {
    [ -s "$privacy_patterns" ] || return 0
    hits=$(tr '[:upper:]' '[:lower:]' < "$1" | grep -a -n -F -f "$privacy_patterns" | cut -c1-200 | head -5)
    if [ -n "$hits" ]; then
        privacy_block "$2 contains blocked text (privacy.block):"
        printf '%s\n' "$hits" >&2
    fi
}
