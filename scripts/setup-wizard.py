#!/usr/bin/env python3
"""Interactive setup wizard for the yonk agent pipeline (maple + bob + abe + goose).

Walks a user through adding one or more OpenAI-compatible model services
(local server, OpenAI, Anthropic, other cloud), assigning them to pipeline
roles (bob's cheap/frontier builder tiers, abe's judge), and writing
~/.config/{goose,abe,bob}/config.yaml accordingly. Unlike install-pipeline.sh
(which never touches an existing config), this wizard OVERWRITES — that's the
whole point of reconfiguring — after one confirmation, backing up whatever
was there first.

Stdlib only. No pip deps.

Usage:
  python3 setup-wizard.py                          # interactive
  python3 setup-wizard.py --yes --endpoint URL      # non-interactive fast path
                                                     # (reproduces install-pipeline.sh:
                                                     #  one local service, auto-detected
                                                     #  model, all roles = that service)
"""
import argparse
import getpass
import json
import os
import re
import shutil
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

HOME = Path(os.environ.get("HOME", str(Path.home())))
CONFIG_DIR = HOME / ".config"
SECRETS_PATH = CONFIG_DIR / "yonk" / "secrets.env"

LOCAL_DEFAULT = "http://localhost:8000/v1"
MODELS_TIMEOUT = 10
CHAT_TIMEOUT = 120

WROTE_ANYTHING = False  # flips true once we start writing files; drives the Ctrl-C message

TOOL_TEST_MESSAGES = [{"role": "user", "content": "Create hello.txt containing hello. Use the write tool."}]
TOOL_TEST_TOOLS = [{
    "type": "function",
    "function": {
        "name": "write",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"],
        },
    },
}]


@dataclass
class Service:
    slug: str                    # config alias, e.g. "local", "openai", "groq"
    kind: str                    # "local" | "openai" | "anthropic" | "other"
    base_url: str
    model: str
    api_key_env: str | None = None   # env var name; None for local (no key)
    key_value: str | None = None     # held in memory only; never written to YAML
    structured: bool | None = None   # tool-call preflight result; None = inconclusive
    latency_ms: float | None = None


# ── small UX helpers ─────────────────────────────────────────────────────────

def say(msg):
    print(f"\n\033[1m== {msg} ==\033[0m")


def ask(prompt, default=""):
    suffix = f" [{default}]" if default else ""
    try:
        val = input(f"{prompt}{suffix}: ").strip()
    except EOFError:
        val = ""
    return val or default


def ask_yn(prompt, default_no=True):
    suffix = "y/N" if default_no else "Y/n"
    try:
        val = input(f"{prompt} ({suffix}): ").strip().lower()
    except EOFError:
        val = ""
    if not val:
        return not default_no
    return val.startswith("y")


# ── pure helpers (formatting / naming) ───────────────────────────────────────

_YAML_SPECIAL = set(":#{}[],&*!|>'\"%@`")


def yq(value):
    """Quote a YAML scalar only when it needs it (special chars, leading/
    trailing space, or a value that would otherwise parse as bool/null/number).
    Plain identifiers (model ids without ':', slugs, env var names) stay bare.
    """
    s = str(value)
    if s == "":
        return '""'
    needs_quote = (
        s != s.strip()
        or any(c in s for c in _YAML_SPECIAL)
        or s.lower() in ("true", "false", "null", "yes", "no", "~")
        or re.match(r"^[+-]?[0-9]", s) is not None
    )
    if needs_quote:
        return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'
    return s


def openai_host(url):
    """Strip a trailing /v1 (and slash) — matches bob's own openai_host():
    goose's `openai` provider appends OPENAI_BASE_PATH itself."""
    u = url.rstrip("/")
    if u.endswith("/v1"):
        u = u[: -len("/v1")]
    return u.rstrip("/")


def slugify(name):
    s = re.sub(r"[^a-zA-Z0-9]+", "-", name.strip()).strip("-").lower()
    return s or "svc"


def unique_slug(base, existing):
    if base not in existing:
        return base
    i = 2
    while f"{base}-{i}" in existing:
        i += 1
    return f"{base}-{i}"


def guess_name_from_url(url):
    m = re.search(r"://(?:api\.)?([^./]+)", url)
    return m.group(1) if m else "provider"


# ── network preflight (urllib only, no requests) ────────────────────────────

def _fmt_err(e):
    if isinstance(e, urllib.error.HTTPError):
        return f"HTTP {e.code} {e.reason}"
    if isinstance(e, urllib.error.URLError):
        return str(e.reason)
    return str(e)


def _get_json(url, headers, timeout):
    req = urllib.request.Request(url, headers=headers, method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _post_json(url, headers, body, timeout):
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers={**headers, "Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def preflight_models(base_url, headers):
    """GET {base}/models. Returns (model_ids_or_None, error_or_None)."""
    try:
        data = _get_json(base_url.rstrip("/") + "/models", headers, MODELS_TIMEOUT)
        ids = [m.get("id") for m in data.get("data", []) if isinstance(m, dict) and m.get("id")]
        return ids, None
    except Exception as e:
        return None, _fmt_err(e)


def preflight_toolcall(base_url, headers, model):
    """POST {base}/chat/completions with a `write` tool to detect structured
    tool_calls. Returns (structured_or_None, latency_ms, error_or_None)."""
    body = {"model": model, "messages": TOOL_TEST_MESSAGES, "tools": TOOL_TEST_TOOLS}
    t0 = time.monotonic()
    try:
        data = _post_json(base_url.rstrip("/") + "/chat/completions", headers, body, CHAT_TIMEOUT)
        elapsed_ms = (time.monotonic() - t0) * 1000
        msg = (data.get("choices") or [{}])[0].get("message", {})
        return bool(msg.get("tool_calls")), elapsed_ms, None
    except Exception as e:
        elapsed_ms = (time.monotonic() - t0) * 1000
        return None, elapsed_ms, _fmt_err(e)


def choose_model(models):
    if not models:
        return ask("   model id (couldn't auto-detect)", "CHANGE-ME")
    if len(models) == 1:
        print(f"   model: {models[0]}")
        return models[0]
    print("   models available:")
    for i, m in enumerate(models, 1):
        print(f"     [{i}] {m}")
    choice = ask("   pick a model", "1")
    try:
        idx = int(choice) - 1
        if 0 <= idx < len(models):
            return models[idx]
    except ValueError:
        pass
    return choice  # treat free text as a manual model id


def report_toolcall(structured, latency_ms, err):
    if err:
        print(f"!! tool-call pre-flight failed: {err} (inconclusive; continuing — enable GOOSE_TOOLSHIM by hand later if a goose role makes no edits)")
        return None
    ms = f"{latency_ms:.0f} ms"
    if structured:
        print(f"   tool-calls: structured ✓  ({ms} round-trip)")
    else:
        print(f"   tool-calls: text-only — goose toolshim will be enabled ⚠  ({ms} round-trip)")
    return structured


# ── SERVICES ─────────────────────────────────────────────────────────────────

def add_service_flow(kind, existing_slugs):
    """Interactively add one service. Returns a Service, or None if cancelled."""
    if kind == "local":
        label, default_url, needs_key = "local server", LOCAL_DEFAULT, False
    elif kind == "openai":
        label, default_url, needs_key = "OpenAI", "https://api.openai.com/v1", True
    elif kind == "anthropic":
        label, default_url, needs_key = "Anthropic", "https://api.anthropic.com", True
    else:
        label, default_url, needs_key = "custom cloud provider", "", True

    while True:
        url = ask(f"Base URL for {label}", default_url)
        if kind == "other" and not url:
            print("A base URL is required for a custom cloud provider.")
            continue

        name = label
        slug_seed = kind
        if kind == "other":
            name = ask("Short name for this provider (used to build the env var name)", guess_name_from_url(url))
            slug_seed = name

        key_value, env_name = None, None
        if needs_key:
            key_value = getpass.getpass(f"API key for {name} (hidden): ")
            if kind == "openai":
                env_name = "OPENAI_API_KEY"
            elif kind == "anthropic":
                env_name = "ANTHROPIC_API_KEY"
            else:
                env_name = f"{slugify(name).upper().replace('-', '_')}_API_KEY"

        headers = {"Authorization": f"Bearer {key_value}"} if key_value else {}
        models, err = preflight_models(url, headers)
        if err:
            print(f"!! could not reach {url}/models: {err}")
            print("   [1] Keep anyway — enter model id manually  [2] Re-enter URL/key  [3] Cancel this service")
            choice = ask("   choice", "1")
            if choice == "2":
                continue
            if choice == "3":
                return None
            model = ask("   model id", "CHANGE-ME")
        else:
            model = choose_model(models)

        structured, latency_ms, tc_err = preflight_toolcall(url, headers, model)
        structured = report_toolcall(structured, latency_ms, tc_err)

        slug = unique_slug(slugify(slug_seed), existing_slugs)
        return Service(slug=slug, kind=kind, base_url=url, model=model,
                        api_key_env=env_name, key_value=key_value,
                        structured=structured, latency_ms=latency_ms)


def collect_services_interactive():
    services = []
    kinds = {"1": "local", "2": "openai", "3": "anthropic", "4": "other"}
    while True:
        say("Add a model service")
        print("  [1] Local server (vLLM/MLX/llama.cpp/ollama — OpenAI-compatible, no key)")
        print("  [2] OpenAI")
        print("  [3] Anthropic")
        print("  [4] Other cloud (custom base_url + key)")
        done_note = "" if services else "  (add at least one first)"
        print(f"  [5] Done adding{done_note}")
        default_choice = "5" if services else "1"
        choice = ask("Choice", default_choice)
        if choice == "5":
            if not services:
                print("Add at least one service before finishing.")
                continue
            break
        kind = kinds.get(choice)
        if not kind:
            print("Please enter 1-5.")
            continue
        svc = add_service_flow(kind, [s.slug for s in services])
        if svc:
            services.append(svc)
            print(f"   added: {svc.slug} ({svc.kind}) @ {svc.base_url} -> {svc.model}")
    return services


# ── ROLES ────────────────────────────────────────────────────────────────────

def pick_role(role_label, services, reuse=None, allow_skip=False, default_index=1):
    say(f"Assign role: {role_label}")
    options = []
    if allow_skip:
        options.append(("Skip — no frontier tier", None))
    for rlabel, rsvc in (reuse or []):
        options.append((f"Same as {rlabel} ({rsvc.slug} → {rsvc.model})", rsvc))
    for s in services:
        options.append((f"{s.slug}  ({s.kind} @ {s.base_url} → {s.model})", s))
    for i, (disp, _) in enumerate(options, 1):
        print(f"  [{i}] {disp}")
    choice = ask("Choice", str(default_index))
    try:
        idx = int(choice) - 1
    except ValueError:
        idx = default_index - 1
    idx = max(0, min(idx, len(options) - 1))
    return options[idx][1]


def print_summary(cheap, frontier, judge):
    say("Summary")
    print(f"  {'role':<18} {'model':<38} {'endpoint':<34} key")
    for role, svc in (("builder-cheap", cheap), ("builder-frontier", frontier), ("judge", judge)):
        if svc is None:
            print(f"  {role:<18} (skipped)")
            continue
        print(f"  {role:<18} {svc.model:<38} {svc.base_url:<34} {svc.api_key_env or 'none'}")


def warn_goose_provider(svc, role):
    if svc.kind == "anthropic":
        print(f"!! {role} is Anthropic, but goose is always configured with GOOSE_PROVIDER=openai "
              f"(OpenAI-compatible transport) in this pipeline. Anthropic's API isn't OpenAI-compatible, "
              f"so goose may fail against it — edit ~/.config/goose/config.yaml by hand if needed.")


# ── CONFIG GENERATION (plain string formatting, no yaml lib) ────────────────

def gen_goose_yaml(cheap):
    lines = [
        "# yonk pipeline — generated by setup-wizard.py",
        "GOOSE_PROVIDER: openai",
        f"GOOSE_MODEL: {yq(cheap.model)}",
        f"OPENAI_HOST: {yq(openai_host(cheap.base_url))}",
    ]
    if cheap.api_key_env:
        lines.append(
            "OPENAI_API_KEY: local  # placeholder — never a real key here; goose prefers the real "
            f"{cheap.api_key_env} env var when it's exported (see ~/.config/yonk/secrets.env)"
        )
    else:
        lines.append("OPENAI_API_KEY: local")
    lines.append("GOOSE_MODE: auto")
    if cheap.structured is False:
        lines.append("GOOSE_TOOLSHIM: true")
    return "\n".join(lines) + "\n"


def gen_abe_yaml(services, judge):
    kind_map = {"local": "openai-compatible", "other": "openai-compatible", "openai": "openai", "anthropic": "anthropic"}
    lines = [
        "# abe — generated by setup-wizard.py",
        "defaults:",
        "  timeout_secs: 300",
        "  max_tokens: 1024",
        "models:",
    ]
    for s in services:
        entry = f"  - {{ name: {s.slug}, kind: {kind_map[s.kind]}, model: {yq(s.model)}, base_url: {yq(s.base_url)}"
        if s.api_key_env:
            entry += f", api_key_env: {s.api_key_env}"
        entry += " }"
        lines.append(entry)
    lines += [
        "debate:",
        "  rounds: 0",
        "  protocol: synthesis",
        f"  chairman: {judge.slug}",
        "  anonymize: true",
        "validate:",
        f"  reviewers: [{judge.slug}]",
    ]
    return "\n".join(lines) + "\n"


def gen_bob_yaml(cheap, frontier):
    lines = [
        "# bob global defaults — generated by setup-wizard.py.",
        "# Per-repo ./bob.yaml overrides this. verify.cmds intentionally empty here:",
        "# ALWAYS set real verify gates in the repo's own bob.yaml.",
        "builder:",
        "  cmd: goose",
        "  timeout_secs: 900",
        f"  endpoint: {yq(cheap.base_url)}  # informational; the models: roster below is authoritative",
        "  models:",
    ]
    written = set()

    def model_entry(svc):
        if svc.slug in written:
            return
        written.add(svc.slug)
        entry = f"    {svc.slug}: {{ model: {yq(svc.model)}, base_url: {yq(svc.base_url)}"
        if svc.api_key_env:
            entry += f", api_key_env: {svc.api_key_env}"
        entry += " }"
        lines.append(entry)

    # ponytail: explicit models: roster (not a bare model id in tiers.cheap) so
    # bob resolves base_url/api_key_env directly instead of guessing from the
    # model-id prefix — that guess only knows minimax/zai/ollama/192.168.1.* and
    # otherwise falls back to a hardcoded default vLLM IP, which would silently
    # misroute any other local port or cloud service. See bob/src/engine.rs
    # resolve_endpoint()/extract_base_url().
    model_entry(cheap)
    if frontier:
        model_entry(frontier)
    lines.append("  tiers:")
    lines.append(f"    cheap: [{cheap.slug}]")
    lines.append("    cheap_builder: goose")
    if frontier:
        lines.append(f"    frontier: [{frontier.slug}]")
        lines.append("    frontier_builder: goose")
    lines += [
        "judge:",
        "  cmd: abe",
        "  policy: advisory",
        "loop:",
        "  max_iterations: 3",
        "  max_walltime_secs: 1500",
        "scope:",
        "  max_changed_files: 4",
        "  max_changed_lines: 300",
        "apply: false",
        "artifacts:",
        "  dir: .bob/runs",
    ]
    return "\n".join(lines) + "\n"


# ── WRITE ────────────────────────────────────────────────────────────────────

def backup_if_exists(path):
    if path.exists():
        ts = time.strftime("%Y%m%d-%H%M%S")
        backup = Path(str(path) + f".bak.{ts}")
        shutil.copy2(path, backup)
        print(f"   backed up {path} -> {backup}")


def write_configs(cheap, frontier, judge, services, interactive=True):
    global WROTE_ANYTHING
    warn_goose_provider(cheap, "builder-cheap")
    if frontier:
        warn_goose_provider(frontier, "builder-frontier")

    goose_path = CONFIG_DIR / "goose" / "config.yaml"
    abe_path = CONFIG_DIR / "abe" / "config.yaml"
    bob_path = CONFIG_DIR / "bob" / "config.yaml"

    print("\nThis OVERWRITES (with a timestamped backup of anything already there) —")
    print("reconfiguring is this wizard's whole point, unlike install-pipeline.sh which never touches an existing config:")
    for p in (goose_path, abe_path, bob_path):
        print(f"  - {p}")
    if interactive and not ask_yn("Proceed?", default_no=True):
        print("Nothing written.")
        return None

    WROTE_ANYTHING = True
    targets = (
        (goose_path, gen_goose_yaml(cheap)),
        (abe_path, gen_abe_yaml(services, judge)),
        (bob_path, gen_bob_yaml(cheap, frontier)),
    )
    for path, content in targets:
        path.parent.mkdir(parents=True, exist_ok=True)
        backup_if_exists(path)
        path.write_text(content)
        print(f"   wrote {path}")
    return {"goose": goose_path, "abe": abe_path, "bob": bob_path}


def write_secrets(services, interactive=True):
    global WROTE_ANYTHING
    keyed = [s for s in services if s.key_value]
    if not keyed:
        return None
    WROTE_ANYTHING = True

    secrets_dir = SECRETS_PATH.parent
    secrets_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(secrets_dir, 0o700)
    backup_if_exists(SECRETS_PATH)

    lines = ["# yonk pipeline API keys — generated by setup-wizard.py. chmod 600. Never commit this file."]
    for s in keyed:
        escaped = s.key_value.replace("'", "'\\''")
        lines.append(f"export {s.api_key_env}='{escaped}'")
    SECRETS_PATH.write_text("\n".join(lines) + "\n")
    os.chmod(SECRETS_PATH, 0o600)
    print(f"   wrote {SECRETS_PATH} (chmod 600)")

    print("\nAdd this to your shell profile so the keys are available:")
    print(f"  source {SECRETS_PATH}")
    if interactive and ask_yn("Append that line to ~/.zshrc now?", default_no=True):
        zshrc = HOME / ".zshrc"
        marker = f"source {SECRETS_PATH}"
        existing = zshrc.read_text() if zshrc.exists() else ""
        if marker in existing:
            print("   already present in ~/.zshrc")
        else:
            with zshrc.open("a") as f:
                f.write(f"\n# yonk pipeline secrets (added by setup-wizard.py)\n{marker}\n")
            print(f"   appended to {zshrc}")
    return SECRETS_PATH


def print_finish(cheap, frontier, judge):
    say("Done")
    print_summary(cheap, frontier, judge)
    print("\nSmoke test commands:")
    print("  abe models")
    print("  bob doctor")
    print("  maple index <repo>")


# ── entry points ─────────────────────────────────────────────────────────────

def fast_path(endpoint):
    """--yes [--endpoint URL]: reproduces install-pipeline.sh's behavior —
    one local/OpenAI-compatible service, auto-detected model, every role
    pointed at it. No prompts, no keys."""
    url = endpoint or LOCAL_DEFAULT
    say(f"Non-interactive fast path: {url}")
    models, err = preflight_models(url, {})
    if err:
        print(f"!! Could not list models at {url}: {err} (continuing; edit configs later)")
        model, structured = "CHANGE-ME", None
    else:
        model = models[0] if models else "CHANGE-ME"
        print(f"   endpoint model: {model}")
        structured, latency_ms, tc_err = preflight_toolcall(url, {}, model)
        structured = report_toolcall(structured, latency_ms, tc_err)

    svc = Service(slug="local", kind="local", base_url=url, model=model, structured=structured)
    services = [svc]
    wrote = write_configs(svc, None, svc, services, interactive=False)
    write_secrets(services, interactive=False)
    if wrote:
        print_finish(svc, None, svc)


def interactive_main():
    say("yonk pipeline setup wizard")
    print("Configures goose, abe, and bob for one or more model services.")

    services = collect_services_interactive()

    cheap = pick_role("builder-cheap (bob cheap tier / goose default)", services)
    frontier = pick_role(
        "builder-frontier (bob frontier tier, optional)", services,
        reuse=[("builder-cheap", cheap)], allow_skip=True,
    )
    judge_reuse = [("builder-cheap", cheap)]
    if frontier:
        judge_reuse.append(("builder-frontier", frontier))
    judge = pick_role("judge (abe chairman + reviewer)", services, reuse=judge_reuse)

    print_summary(cheap, frontier, judge)

    wrote = write_configs(cheap, frontier, judge, services, interactive=True)
    if wrote is None:
        return
    write_secrets(services, interactive=True)
    print_finish(cheap, frontier, judge)


def _selftest():
    assert yq("plain") == "plain"
    assert yq("has:colon") == '"has:colon"'
    assert yq("") == '""'
    assert yq("123abc") == '"123abc"'
    assert yq("true") == '"true"'
    assert openai_host("http://h:8000/v1") == "http://h:8000"
    assert openai_host("https://api.openai.com/v1") == "https://api.openai.com"
    assert openai_host("http://h:8000/v1/") == "http://h:8000"
    assert slugify("My Cool Provider!") == "my-cool-provider"
    assert unique_slug("openai", ["openai"]) == "openai-2"
    assert unique_slug("openai", []) == "openai"
    assert guess_name_from_url("https://api.groq.com/openai/v1") == "groq"
    print("selftest: OK")


def main():
    parser = argparse.ArgumentParser(description="Interactive setup wizard for the yonk agent pipeline (maple + bob + abe + goose).")
    parser.add_argument("--yes", action="store_true", help="Non-interactive fast path: one local service, auto-detected model, all roles assigned to it.")
    parser.add_argument("--endpoint", metavar="URL", help="Base URL for --yes mode (default http://localhost:8000/v1). Also pre-fills the default in interactive mode.")
    parser.add_argument("--selftest", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.selftest:
        _selftest()
        return

    if args.yes:
        fast_path(args.endpoint)
        return

    global LOCAL_DEFAULT
    if args.endpoint:
        LOCAL_DEFAULT = args.endpoint

    interactive_main()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        if WROTE_ANYTHING:
            print("\ninterrupted — some files may have been written; check ~/.config/{goose,abe,bob}/config.yaml")
        else:
            print("\nnothing written")
        sys.exit(130)
