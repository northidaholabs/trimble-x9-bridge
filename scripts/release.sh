#!/bin/sh
# Open the pull request for a release. Merging it releases: on the push to
# main, the release workflow sees a version with no tag yet, tags the merge
# commit and builds and attaches every platform's archive.
#
#   scripts/release.sh                 next patch version
#   scripts/release.sh minor           next minor version
#   scripts/release.sh 0.22.0          that version
#   scripts/release.sh patch --merge   also wait for the checks and merge
#
# Needs a clean checkout of main that is up to date, git, gh and perl.
# COMMIT_TRAILER, if set, is added to the commit message after a blank line.
set -eu
cd "$(dirname "$0")/.."

bump=patch merge=0
for a in "$@"; do
  case $a in
    --merge) merge=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) bump=$a ;;
  esac
done

[ -z "$(git status --porcelain)" ] || { echo "the checkout has changes; commit or stash them first" >&2; exit 1; }
git fetch -q origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "check out origin/main first (git switch main && git pull)" >&2; exit 1; }

scripts/version.sh --check >/dev/null
scripts/version.sh "$bump"
v=$(scripts/version.sh)
tag=v$v
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
  git checkout -q -- .
  echo "$tag is already tagged" >&2
  exit 1
fi

branch=release/$tag
git switch -q -c "$branch"
msg=$tag
[ -z "${COMMIT_TRAILER:-}" ] || msg=$(printf '%s\n\n%s' "$tag" "$COMMIT_TRAILER")
git commit -q -am "$msg"
git push -q -u origin "$branch"
body="Version $v. Merging this tags the merge commit $tag and builds the release."
[ -z "${PR_FOOTER:-}" ] || body=$(printf '%s\n\n%s' "$body" "$PR_FOOTER")
url=$(gh pr create --base main --head "$branch" --title "$tag" --body "$body")
echo "$url"

[ "$merge" = 1 ] || exit 0
n=${url##*/}
# The checks take a moment to be listed after the pull request opens.
i=0
while gh pr checks "$n" 2>&1 | grep -q 'no checks reported' && [ $i -lt 30 ]; do sleep 5; i=$((i + 1)); done
gh pr checks "$n" --watch --fail-fast
gh pr merge "$n" --merge --delete-branch
git switch -q main 2>/dev/null || git switch -q --detach origin/main
echo "merged; the release workflow tags $tag and builds it"
