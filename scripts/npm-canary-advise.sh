#!/usr/bin/env bash
# The npm canary's advisory LLM summary (.github/workflows/npm-canary.yml):
#   npm-canary-advise.sh <llm-input.txt> <out.md>
# Sends the diffs of the tracked functions (npm's public source, nothing
# else) to Claude through the Claude Code CLI in print mode, and writes a
# comment marked as advisory to <out.md>. Never a gate: it always exits 0;
# <out.md>.status holds one line saying what happened (posted / skipped / failed).
#
# Credentials, one code path: ANTHROPIC_API_KEY (an Anthropic API key) or
# CLAUDE_CODE_OAUTH_TOKEN (a subscription token from `claude setup-token`);
# the CLI reads either. When both are set, the API key is used (the CLI's own
# order). Neither: skipped with a note.
#
# The CLI runs with no tools at all (--tools "", MCP tools denied, no MCP
# servers), one answer (--max-turns), a spend cap for API keys, no session
# saved, no non-essential traffic, in an empty directory, under a timeout.
# CANARY_CLAUDE names the CLI (default: claude on PATH).
set -u
in=${1:?usage: npm-canary-advise.sh <llm-input.txt> <out.md>}; out=${2:?}
status() { echo "$1" > "$out.status"; echo "advisory summary: $1"; }
rm -f "$out" "$out.status"
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  cred="an API key"; unset CLAUDE_CODE_OAUTH_TOKEN
elif [ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]; then
  cred="a subscription token"; unset ANTHROPIC_API_KEY
else
  status "skipped (neither ANTHROPIC_API_KEY nor CLAUDE_CODE_OAUTH_TOKEN is set as a repository secret)"; exit 0
fi
[ -s "$in" ] || { status "skipped (no diff)"; exit 0; }
size=$(wc -c < "$in" | tr -d ' ')
[ "$size" -le 60000 ] || { status "skipped (the diffs are $size bytes; the summary takes at most 60000, and the diffs are in the comment)"; exit 0; }
claude=${CANARY_CLAUDE:-claude}
model='claude-opus-5-5'
system='You read diffs of the npm CLI'"'"'s own source code for the maintainer of matchapack. matchapack replaces one method inside `npm ci`: pacote'"'"'s FetcherBase.prototype.extract(dest), which it answers by cloning a cached extraction of the same tarball into dest. That is correct only while (1) on that path the FetcherBase class does nothing beyond writing the tarball'"'"'s files into dest, (2) arborist calls `await pacote.extract(...)` as a bare statement and never reads what it resolves to, and (3) nothing new touches the package directory after extract returns. For the diffs on standard input, answer in at most 200 words, as a short list: which changes alter behaviour (not style or logging), naming each function, and whether any bears on (1), (2) or (3). If none does, say so in one line. Do not guess beyond the diff.'
dir=$(mktemp -d)
t0=$(date +%s)
( cd "$dir" && CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 timeout 300 "$claude" -p 'Summarize the diffs on standard input.' \
    --model "$model" --effort low --system-prompt "$system" \
    --tools "" --disallowedTools "mcp__*" --strict-mcp-config \
    --max-turns 2 --max-budget-usd 1 --no-session-persistence --output-format text ) < "$in" > "$dir/answer" 2> "$dir/err"
rc=$?
if [ "$rc" -ne 0 ] || [ ! -s "$dir/answer" ]; then
  status "failed (exit $rc after $(( $(date +%s) - t0 )) s; the job log has the CLI's last lines)"
  tail -3 "$dir/err" >&2
  rm -rf "${dir:?}"; exit 0
fi
{
  echo "### Advisory summary by an LLM"
  echo
  echo "> **Advisory only.** Written by \`$model\` (Claude Code CLI, $cred) from the function diffs in the comment above, and nothing else. No test checks it and it decides nothing: read the diffs."
  echo
  # Quoted, and no @mention can notify anyone (a zero-width space after @).
  sed -e 's/@/@​/g' -e 's/^/> /' "$dir/answer"
} > "$out"
status "posted ($(( $(date +%s) - t0 )) s)"
rm -rf "${dir:?}"
exit 0
