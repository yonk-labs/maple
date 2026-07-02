#!/usr/bin/env bash
# yonk pipeline installer — maple + bob + abe + hector + goose, configured and on PATH.
# Usage:  bash install-pipeline.sh [OPENAI_COMPAT_ENDPOINT]
#   e.g.  bash install-pipeline.sh http://192.168.1.193:8000/v1
# Idempotent: re-running updates everything in place. Nothing here needs sudo.
set -euo pipefail

BIN="$HOME/.local/bin"
SRC="${YONK_SRC:-$HOME/yonk-tools}"
ENDPOINT="${1:-}"

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

# 0) Rust toolchain (skipped if present)
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  say "Installing Rust (one-time, ~1 min)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env" 2>/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$BIN" "$SRC"

# 1) Ask for the model endpoint once (any OpenAI-compatible server: vLLM, MLX, llama.cpp)
if [ -z "$ENDPOINT" ]; then
  printf 'OpenAI-compatible endpoint URL [http://localhost:8000/v1]: '
  read -r ENDPOINT || true
  ENDPOINT="${ENDPOINT:-http://localhost:8000/v1}"
fi
HOSTONLY="${ENDPOINT%/v1}"

# 2) Discover the first model the endpoint serves (so nobody has to type a model id)
MODEL="$(curl -sf --max-time 10 "$ENDPOINT/models" | python3 -c 'import json,sys;print(json.load(sys.stdin)["data"][0]["id"])' 2>/dev/null || true)"
if [ -z "$MODEL" ]; then
  echo "!! Could not list models at $ENDPOINT — is the server running? (continuing; edit configs later)"
  MODEL="CHANGE-ME"
else
  echo "   endpoint model: $MODEL"
  # pre-flight: agents need STRUCTURED tool calls from the server
  TC="$(curl -sf --max-time 120 "$ENDPOINT/chat/completions" -H 'Content-Type: application/json' -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Create hello.txt containing hello. Use the write tool.\"}],\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"write\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"}},\"required\":[\"path\",\"content\"]}}}]}" \
        | python3 -c 'import json,sys;print("ok" if json.load(sys.stdin)["choices"][0]["message"].get("tool_calls") else "none")' 2>/dev/null || echo err)"
  case "$TC" in
    ok)   echo "   tool-calls: structured ✓" ;;
    none) echo "!! Server returns tool calls as text, not structured. Agents will need GOOSE_TOOLSHIM=true (configured below)." ;;
    *)    echo "!! Tool-call pre-flight inconclusive; continuing." ;;
  esac
fi

# 3) Clone/update + build + install the four tools
for R in maple bob abe hector; do
  say "Installing $R"
  if [ -d "$SRC/$R/.git" ]; then git -C "$SRC/$R" pull -q --ff-only || true
  else git clone -q "https://github.com/yonk-labs/$R.git" "$SRC/$R"; fi
  (cd "$SRC/$R" && cargo build --release -q)
  cp "$SRC/$R/target/release/$R" "$BIN/$R"
done

# 4) goose (block/goose builder CLI) — official binary
if ! command -v goose >/dev/null 2>&1; then
  say "Installing goose"
  ARCH="$(uname -m)"; case "$ARCH" in arm64|aarch64) T=aarch64-apple-darwin;; *) T=x86_64-apple-darwin;; esac
  [ "$(uname -s)" = "Linux" ] && case "$ARCH" in aarch64) T=aarch64-unknown-linux-gnu;; *) T=x86_64-unknown-linux-gnu;; esac
  curl -fsSL "https://github.com/block/goose/releases/download/stable/goose-$T.tar.bz2" | tar xj -C "$BIN" goose
  chmod +x "$BIN/goose"
fi

# 5) Configs (only written if absent — your edits are never clobbered)
say "Writing configs (skipping any that already exist)"
mkdir -p "$HOME/.config/goose" "$HOME/.config/abe" "$HOME/.config/bob"
[ -f "$HOME/.config/goose/config.yaml" ] || cat > "$HOME/.config/goose/config.yaml" <<EOF
GOOSE_PROVIDER: openai
GOOSE_MODEL: $MODEL
OPENAI_HOST: $HOSTONLY
OPENAI_API_KEY: local
GOOSE_MODE: auto
$( [ "${TC:-}" = "none" ] && echo "GOOSE_TOOLSHIM: true" )
EOF
[ -f "$HOME/.config/abe/config.yaml" ] || cat > "$HOME/.config/abe/config.yaml" <<EOF
defaults: { timeout_secs: 300, max_tokens: 1024 }
models:
  - { name: local, kind: openai-compatible, model: "$MODEL", base_url: "$ENDPOINT" }
debate: { rounds: 0, protocol: synthesis, chairman: local, anonymize: true }
validate: { reviewers: [local] }
EOF
[ -f "$HOME/.config/bob/config.yaml" ] || cat > "$HOME/.config/bob/config.yaml" <<EOF
# Global defaults; put a bob.yaml with REAL verify gates in each repo.
builder:
  cmd: goose
  timeout_secs: 900
  endpoint: $ENDPOINT
  tiers:
    cheap: ["$MODEL"]
    cheap_builder: goose
judge: { cmd: abe, policy: advisory }
loop: { max_iterations: 3, max_walltime_secs: 1500 }
scope: { max_changed_files: 4, max_changed_lines: 300 }
apply: false
artifacts: { dir: .bob/runs }
EOF

# 6) PATH check + verify
case ":$PATH:" in *":$BIN:"*) ;; *) echo "!! Add to your shell profile:  export PATH=\"\$HOME/.local/bin:\$PATH\"";; esac
say "Verify"
for b in maple bob abe hector goose; do printf '  %-8s %s\n' "$b" "$("$BIN/$b" --version 2>/dev/null | head -1 || echo FAILED)"; done
abe models 2>/dev/null | head -3 || true
cat <<'EOF'

Done. Try it:
  maple index /path/to/a/python/repo        # build the code graph (once)
  maple bundle /path/to/repo --symbol some_function --format prompt
  cd /path/to/repo && bob build "fix the failing test X"   # add a bob.yaml with verify gates first
EOF
