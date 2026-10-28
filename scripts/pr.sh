#!/usr/bin/env bash
# Opens a PR for the current branch and links its issue, without GitHub Actions.
# The issue number comes from --issue N or the branch name (e.g. fix/14-suspended-login).
# usage: scripts/pr.sh "Title" body.md [--issue N] [--base main]
set -euo pipefail
title=$1 body_file=$2; shift 2
issue="" base=main
while [ $# -gt 0 ]; do
    case $1 in
        --issue) issue=$2; shift 2 ;;
        --base) base=$2; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
branch=$(git branch --show-current)
[ -n "$issue" ] || issue=$(sed -nE 's#^([^/]+/)?([0-9]+)-.*#\2#p' <<<"$branch")
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
body=$(cat "$body_file")
# "Closes #N" makes GitHub link the issue and close it when the PR merges.
if [ -n "$issue" ] && ! grep -qiE "(close[sd]?|fix(e[sd])?|resolve[sd]?) #$issue\b" <<<"$body"; then
    body+=$'\n\n'"Closes #$issue"
fi
git push -q -u origin "$branch"
gh api "repos/$repo/pulls" -f title="$title" -f head="$branch" -f base="$base" -f body="$body" --jq .html_url
