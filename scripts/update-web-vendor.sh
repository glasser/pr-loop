#!/usr/bin/env bash
# Re-fetches the browser JS/CSS dependencies vendored into src/web/vendor/,
# served locally by `pr-loop hub` instead of pulled from a CDN at runtime by
# the browser. Bump a version below, then run this script and review the
# diff like any other dependency bump.
#
# preact.mjs and hooks.mjs are fetched un-bundled and deliberately kept as
# two separate files in the same directory: hooks.mjs imports preact.mjs by
# relative path, and hooks only works correctly if that import resolves to
# the exact same module instance used by the rest of the app (it patches
# preact's internal options object). Bundling them independently — each
# inlining its own private copy of preact's internals — silently breaks
# hooks. The other packages don't share state with anything else, so
# fetching each as one self-contained bundle is safe.
set -euo pipefail
cd "$(dirname "$0")/.."

VENDOR_DIR=src/web/vendor
mkdir -p "$VENDOR_DIR"

PREACT_VERSION=10.22.0
HTM_VERSION=3.1.1
MARKDOWN_IT_VERSION=14.1.0
MARKDOWN_IT_EMOJI_VERSION=3.0.0
HIGHLIGHTJS_VERSION=11.10.0
GITHUB_MARKDOWN_CSS_VERSION=5.9.0

# esm.sh serves a package specifier as a one-line stub re-exporting from a
# concrete versioned .mjs file (e.g. "highlight.js@11.10.0?bundle" redirects
# to ".../highlight.bundle.mjs" — not a predictable name, so we resolve it
# rather than guessing). The stub can also contain a plain `import "...";`
# line for a *different* file (e.g. preact/hooks importing preact itself) —
# grep specifically for the `... from "target"` line, not just any .mjs
# string, and take the last such line in case there's more than one (htm's
# stub re-exports the same target twice, via `export *` and `export {
# default }`).
fetch_esm() {
  local spec="$1" out="$2"
  local stub target
  stub="$(curl -sSL --fail "https://esm.sh/${spec}")"
  target="$(printf '%s' "$stub" | grep -oE 'from "[^"]+\.mjs"' | tail -1 | grep -oE '"/[^"]+\.mjs"' | tr -d '"')"
  if [ -z "$target" ]; then
    echo "error: couldn't resolve a target .mjs for esm.sh/${spec}" >&2
    echo "$stub" >&2
    exit 1
  fi
  curl -sSL --fail "https://esm.sh${target}" -o "$out"
  echo "  ${spec} -> ${target}"
}

echo "Fetching preact + preact/hooks..."
fetch_esm "preact@${PREACT_VERSION}" "$VENDOR_DIR/preact.mjs"
fetch_esm "preact@${PREACT_VERSION}/hooks" "$VENDOR_DIR/hooks.mjs"

echo "Fetching self-contained bundles..."
fetch_esm "htm@${HTM_VERSION}?bundle" "$VENDOR_DIR/htm.mjs"
fetch_esm "markdown-it@${MARKDOWN_IT_VERSION}?bundle" "$VENDOR_DIR/markdown-it.mjs"
fetch_esm "markdown-it-emoji@${MARKDOWN_IT_EMOJI_VERSION}?bundle" "$VENDOR_DIR/markdown-it-emoji.mjs"
fetch_esm "highlight.js@${HIGHLIGHTJS_VERSION}?bundle" "$VENDOR_DIR/highlightjs.mjs"

echo "Fetching CSS..."
curl -sSL --fail "https://cdn.jsdelivr.net/npm/github-markdown-css@${GITHUB_MARKDOWN_CSS_VERSION}/github-markdown.min.css" -o "$VENDOR_DIR/github-markdown.min.css"
curl -sSL --fail "https://cdn.jsdelivr.net/npm/highlight.js@${HIGHLIGHTJS_VERSION}/styles/github.min.css" -o "$VENDOR_DIR/highlightjs-github.min.css"

echo "Done. Review the diff, then run \`cargo build\` and exercise the web UI before committing."
