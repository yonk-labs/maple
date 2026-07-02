#!/usr/bin/env python3
"""
S0 validation spike for `maple` (code-symbol-graph).  THROWAWAY CODE.

Answers PLAN.md S0.1: on a real repo, classify every call-site by name-based
resolution and report the rates — the number that decides whether byte-sized
bundles are worth building a Rust core for.

    exact      : exactly one in-repo definition with that name  -> clean bundle edge
    ambiguous  : >1 in-repo definitions (name collision)        -> universal tier over-includes (safe, flagged)
    external   : 0 in-repo definitions (stdlib / third-party /  -> not a gap; you don't bundle these
                 aliased import / dynamic dispatch)

The gate: if `external` swamps real in-repo calls, or `ambiguous` is so high that
bundles bloat, that's the signal to rethink before writing the core.

# ponytail: deliberately-dumb baseline — no import-alias expansion, no scope/type
#           awareness. That's exactly what the real universal tier ADDS (D3). This
#           script measures the floor those features build up from. Upgrade path:
#           add alias expansion here and watch `external` drop -> quantifies D3's value.

Requires:  pip install "tree-sitter>=0.23" tree-sitter-python
           (add tree-sitter-javascript etc. to widen LANGS)
Usage:     python3 s0_resolution_rates.py /path/to/repo [--top 25]
           python3 s0_resolution_rates.py --selfcheck
"""
from __future__ import annotations
import argparse, sys
from collections import Counter
from pathlib import Path

# ---- tree-sitter, with defensive shims for cross-version API churn -------------
# imported lazily so --selfcheck runs with zero deps
def _make_query(lang, src):
    from tree_sitter import Query
    try:
        return Query(lang, src)              # 0.23+
    except Exception:
        return lang.query(src)               # older

def _run_query(query, node):
    """Return list[(capture_name, node)] regardless of version."""
    try:
        from tree_sitter import QueryCursor   # 0.24+
        caps = QueryCursor(query).captures(node)
    except Exception:
        caps = query.captures(node)          # 0.23 and older
    if isinstance(caps, dict):               # {name: [nodes]}
        return [(name, n) for name, nodes in caps.items() for n in nodes]
    return [(name, n) for n, name in caps]   # [(node, name)]

# ---- language registry: extension -> (language, def-query, call-query) ---------
def _load_python():
    import tree_sitter_python as tsp
    from tree_sitter import Language
    lang = Language(tsp.language())
    defs = _make_query(lang, """
        (function_definition name: (identifier) @def)
        (class_definition name: (identifier) @def)
    """)
    calls = _make_query(lang, """
        (call function: (identifier) @call.func)
        (call function: (attribute attribute: (identifier) @call.method))
    """)
    return lang, defs, calls

# add more here as needed, e.g. ".js": _load_javascript  (same shape)
LANG_LOADERS = {".py": _load_python}

SKIP_DIRS = {".git", ".maple", "node_modules", "venv", ".venv", "__pycache__",
             "dist", "build", "target", ".mypy_cache", "vendor"}
MAX_BYTES = 2_000_000  # skip pathological files for a spike

def _iter_files(root: Path):
    for p in root.rglob("*"):
        if p.is_dir() or any(part in SKIP_DIRS for part in p.parts):
            continue
        if p.suffix in LANG_LOADERS and p.stat().st_size <= MAX_BYTES:
            yield p

def _texts(nodes, want=None):
    return [n.text.decode("utf8", "replace") for name, n in nodes
            if want is None or name == want]

def analyze(root: Path):
    from tree_sitter import Parser
    parsers, queries = {}, {}
    for ext, loader in LANG_LOADERS.items():
        lang, dq, cq = loader()
        parsers[ext] = Parser(lang)
        queries[ext] = (dq, cq)

    def_names: Counter[str] = Counter()
    func_calls: Counter[str] = Counter()   # foo()      — tractable by name
    method_calls: Counter[str] = Counter() # x.foo()    — needs receiver type
    files = 0
    for path in _iter_files(root):
        try:
            src = path.read_bytes()
            tree = parsers[path.suffix].parse(src)
        except Exception as e:                 # a bad parse must not silently vanish
            print(f"  ! parse failed {path}: {e}", file=sys.stderr)
            continue
        dq, cq = queries[path.suffix]
        def_names.update(_texts(_run_query(dq, tree.root_node)))
        caps = _run_query(cq, tree.root_node)
        func_calls.update(_texts(caps, "call.func"))
        method_calls.update(_texts(caps, "call.method"))
        files += 1
    return files, def_names, func_calls, method_calls

def classify(def_names: Counter, call_names: Counter):
    exact = ambiguous = external = 0
    ext_counter: Counter[str] = Counter()
    for name, n in call_names.items():
        d = def_names.get(name, 0)
        if d == 1:   exact += n
        elif d > 1:  ambiguous += n
        else:        external += n; ext_counter[name] += n
    return exact, ambiguous, external, ext_counter

def _bucket_line(label, exact, ambiguous, external):
    total = exact + ambiguous + external
    if not total:
        print(f"  {label}: (none)"); return
    p = lambda x: f"{100*x/total:5.1f}%"
    in_repo = exact + ambiguous
    ir = f"  |  in-repo: {100*exact/in_repo:.0f}% exact / {100*ambiguous/in_repo:.0f}% ambiguous" if in_repo else ""
    print(f"  {label} (n={total}): exact {p(exact)}  ambiguous {p(ambiguous)}  external {p(external)}{ir}")

def report(root, top):
    files, def_names, func_calls, method_calls = analyze(root)
    total = sum(func_calls.values()) + sum(method_calls.values())
    fe, fa, fx, _ = classify(def_names, func_calls)
    me, ma, mx, _ = classify(def_names, method_calls)
    _, _, _, ext_counter = classify(def_names, func_calls + method_calls)
    print(f"\n=== maple S0 resolution-rate spike: {root} ===")
    print(f"files parsed       : {files}")
    print(f"defs indexed (fn+class): {len(def_names)}")
    print(f"total call-sites   : {total}   (func {sum(func_calls.values())}, method {sum(method_calls.values())})")
    if not total:
        print("no calls found — point at a repo with source in a supported language."); return
    print("\nby call kind:")
    _bucket_line("function calls  foo()  ", fe, fa, fx)   # the tractable case
    _bucket_line("method calls    x.foo()", me, ma, mx)   # needs receiver type (worst case for name-based)
    _bucket_line("ALL             combined", fe+me, fa+ma, fx+mx)
    print(f"\ntop {top} external/unresolved names (eyeball: stdlib/3rd-party vs real in-repo gap):")
    for name, n in ext_counter.most_common(top):
        print(f"  {n:5d}  {name}")
    print("\nGATE (PLAN S0.1): FUNCTION-call resolution is the number that matters for maple's v1 "
          "universal tier. Runaway 'ambiguous' there = exactness (F4) is load-bearing, not optional.")

def selfcheck():
    # minimal in-memory check that the classifier logic is sound
    defs = Counter({"foo": 1, "bar": 2})          # foo unique, bar collides
    calls = Counter({"foo": 3, "bar": 1, "print": 5})  # print is external
    exact, ambiguous, external, _ = classify(defs, calls)
    assert (exact, ambiguous, external) == (3, 1, 5), (exact, ambiguous, external)
    print("selfcheck OK")

def main():
    ap = argparse.ArgumentParser(description="maple S0 resolution-rate spike (throwaway)")
    ap.add_argument("repo", nargs="?", help="path to a real repo to measure")
    ap.add_argument("--top", type=int, default=25, help="show N most common external names")
    ap.add_argument("--selfcheck", action="store_true", help="run the classifier self-check and exit")
    args = ap.parse_args()
    if args.selfcheck:
        selfcheck(); return
    if not args.repo:
        ap.error("give a repo path, or --selfcheck")
    root = Path(args.repo).resolve()
    if not root.is_dir():
        ap.error(f"not a directory: {root}")
    report(root, args.top)

if __name__ == "__main__":
    main()
