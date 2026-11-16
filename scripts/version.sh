#!/bin/sh
# The version lives in five places. Keep them in step.
#
#   scripts/version.sh               print the version (from Cargo.toml)
#   scripts/version.sh --check       fail if any place disagrees with Cargo.toml
#   scripts/version.sh 0.22.0        set every place to 0.22.0
#   scripts/version.sh patch|minor   set every place to the next patch or minor
#
# The places: Cargo.toml's workspace version, the workspace crates in
# Cargo.lock, README's "Version X:" line, and the `tag=vX` install lines in
# docs/guide.md and ops/README.md.
set -eu
cd "$(dirname "$0")/.."

cur=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$cur" ] || { echo "no workspace version in Cargo.toml" >&2; exit 1; }

# Each place: file, a perl pattern where @V@ stands for the version, and how
# many times the pattern must match.
PLACES='Cargo.toml	^version = "@V@"$	1
Cargo.lock	^name = "x9-(?:bridge|mock|cli|client)"\nversion = "@V@"$	4
README.md	^Version @V@:	1
docs/guide.md	tag=v@V@;	1
ops/README.md	tag=v@V@;	1'

# Prints each place that does not match its expected count, for version $1.
mismatches() {
  printf '%s\n' "$PLACES" | while IFS='	' read -r f pat want; do
    n=$(PAT=$pat VER=$1 perl -0777 -ne '
      my ($pat, $ver) = ($ENV{PAT}, $ENV{VER});
      my $q = quotemeta $ver; $pat =~ s/\@V\@/$q/g;
      my $n = () = /$pat/mg; print $n' "$f")
    [ "$n" = "$want" ] || echo "$f: $n of $want for $1"
  done
}

case ${1:-} in
  "") echo "$cur" ;;
  -h|--help) sed -n '2,11p' "$0" ;;
  --check)
    bad=$(mismatches "$cur")
    if [ -n "$bad" ]; then
      printf '%s\n' "$bad" >&2
      echo "some places disagree with Cargo.toml ($cur)" >&2
      exit 1
    fi
    echo "$cur everywhere" ;;
  *)
    case $1 in
      patch) new=$(echo "$cur" | awk -F. '{print $1"."$2"."$3+1}') ;;
      minor) new=$(echo "$cur" | awk -F. '{print $1"."$2+1".0"}') ;;
      [0-9]*.[0-9]*.[0-9]*) new=$1 ;;
      *) echo "not a version: $1" >&2; exit 2 ;;
    esac
    [ "$new" != "$cur" ] || { echo "already $cur" >&2; exit 1; }
    bad=$(mismatches "$cur")
    [ -z "$bad" ] || { printf '%s\n' "$bad" >&2; echo "fix these first: they disagree with Cargo.toml ($cur)" >&2; exit 1; }
    printf '%s\n' "$PLACES" | while IFS='	' read -r f pat want; do
      PAT=$pat VER=$cur NEW=$new perl -0777 -pi -e '
        my ($pat, $ver) = ($ENV{PAT}, $ENV{VER});
        my $q = quotemeta $ver; $pat =~ s/\@V\@/$q/g;
        s/$pat/my $m = $&; $m =~ s{\Q$ver\E}{$ENV{NEW}}; $m/mge' "$f"
    done
    bad=$(mismatches "$new")
    [ -z "$bad" ] || { printf '%s\n' "$bad" >&2; echo "the bump missed a place" >&2; exit 1; }
    echo "$cur -> $new" ;;
esac
