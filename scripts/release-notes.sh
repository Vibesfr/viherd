#!/usr/bin/env bash
# Notes de version à partir des messages de commit depuis le tag précédent.
#
#   scripts/release-notes.sh v0.2.2 > notes.md
#
# Chaque commit devient une section : son titre, puis le corps du message (listes à puces).
# Les commits « Version x.y.z » et les lignes Co-Authored-By sont ignorés.
set -euo pipefail

tag="${1:?usage : release-notes.sh <tag>}"
repo="${GITHUB_REPOSITORY:-Vibesfr/viherd}"
prev="$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)"
range="${prev:+$prev..}$tag"

echo "## Nouveautés"
git log --reverse --no-merges --format='%H' "$range" | while read -r sha; do
  subject="$(git log -1 --format=%s "$sha")"
  [[ "$subject" =~ ^Version\ [0-9] ]] && continue
  body="$(git log -1 --format=%b "$sha" | grep -v -i '^co-authored-by:' | sed -e '/./,$!d' || true)"
  echo
  echo "### $subject"
  if [ -n "$(echo "$body" | tr -d '[:space:]')" ]; then
    echo
    # lignes de suite d'une puce (indentées) recollées à la puce
    echo "$body" | sed -e ':a' -e 'N' -e '$!ba' -e 's/\n  */ /g'
  fi
done

echo
echo "---"
echo
echo "**Installer** : télécharge \`viherd-setup.exe\` ci-dessous. Depuis la v0.2.0, Viherd propose lui-même la mise à jour."
if [ -n "$prev" ]; then
  echo
  echo "Toutes les modifications : https://github.com/$repo/compare/$prev...$tag"
fi
