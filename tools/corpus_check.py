#!/usr/bin/env python3
"""Run --delete over a corpus and prove nothing but comments/docstrings changed.

    tools/corpus_check.py [--bin PATH] [--target comments|docstrings|both] DIR...

Each DIR (a git repo, or any tree such as /usr/lib/python3.12) is copied to a temp git repo,
the binary runs there, and every changed file is checked:

- Python, comments: the AST must be identical before and after.
- Python, docstrings: the AST must be identical once docstrings are removed from both sides.
- YAML: `yaml.safe_load_all` must yield equal documents (files PyYAML cannot parse before the
  run are skipped and counted).

Exit status 1 on any mismatch, skipped file (parse error / internal error) or compile failure.
Needs PyYAML for YAML corpora. tools/ is not part of the crate; this is a development check.
"""

import argparse
import ast
import os
import shutil
import subprocess
import sys
import tempfile

SUFFIXES = {".py", ".pyi", ".js", ".jsx", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".tsx", ".yml", ".yaml"}


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=True).stdout


def stage(dirs):
    repo = tempfile.mkdtemp(prefix="corpus_check_")
    n = 0
    for i, d in enumerate(dirs):
        for root, _, files in os.walk(d):
            if "/.git" in root or "__pycache__" in root or "node_modules" in root:
                continue
            for f in files:
                if os.path.splitext(f)[1] in SUFFIXES:
                    src = os.path.join(root, f)
                    dst = os.path.join(repo, str(i), os.path.relpath(src, d))
                    os.makedirs(os.path.dirname(dst), exist_ok=True)
                    shutil.copyfile(src, dst)
                    n += 1
    git(repo, "init", "-q")
    git(repo, "add", "-A")
    git(repo, "-c", "user.name=c", "-c", "user.email=c@c", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "corpus")
    return repo, n


def strip_docstrings(tree):
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
            first = node.body[0]
            if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant) and isinstance(first.value.value, str):
                del node.body[0]
                if not node.body:
                    node.body = [ast.Pass()]
    return tree


def check_python(old, new, target):
    a, b = ast.parse(old), ast.parse(new)
    if target == "docstrings":
        a, b = strip_docstrings(a), strip_docstrings(b)
    return ast.dump(a) == ast.dump(b)


def run_target(binary, repo, target):
    proc = subprocess.run([binary, target, repo, "--delete"], capture_output=True, text=True)
    skipped = [l for l in proc.stderr.splitlines() if l.startswith("warning: skipping")]
    summary = proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() else ""
    changed = git(repo, "diff", "--name-only").split()
    mismatches, checked, unparsable = [], 0, 0
    for rel in changed:
        path = os.path.join(repo, rel)
        with open(path, "rb") as fh:
            new = fh.read()
        old = subprocess.run(["git", "-C", repo, "show", f"HEAD:{rel}"], capture_output=True, check=True).stdout
        ext = os.path.splitext(rel)[1]
        try:
            if ext in (".py", ".pyi"):
                ok = check_python(old.decode("utf-8", "surrogateescape"), new.decode("utf-8", "surrogateescape"), target)
            elif ext in (".yml", ".yaml"):
                import yaml

                try:
                    before = list(yaml.safe_load_all(old))
                except yaml.YAMLError:
                    unparsable += 1
                    continue
                ok = list(yaml.safe_load_all(new)) == before
            else:
                continue  # JS/TS: no parser at hand; the tree-sitter reparse in the tool is the guard
        except SyntaxError as e:
            ok = False
            mismatches.append(f"{rel}: syntax error after edit: {e}")
            continue
        checked += 1
        if not ok:
            mismatches.append(rel)
    git(repo, "checkout", "-q", ".")
    return summary, skipped, changed, checked, unparsable, mismatches


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "commentreducr"))
    ap.add_argument("--target", choices=["comments", "docstrings", "both"], default="both")
    ap.add_argument("dirs", nargs="+")
    args = ap.parse_args()
    repo, n = stage(args.dirs)
    print(f"staged {n} files in {repo}")
    failed = False
    for target in ["comments", "docstrings"] if args.target == "both" else [args.target]:
        summary, skipped, changed, checked, unparsable, mismatches = run_target(args.bin, repo, target)
        print(f"\n== {target}: {summary}")
        print(f"changed {len(changed)}, checked {checked}, yaml unparsable before {unparsable}, mismatches {len(mismatches)}, skipped {len(skipped)}")
        for line in skipped + mismatches:
            print("  " + line.replace(repo + "/", ""))
        failed |= bool(mismatches) or bool(skipped)
    shutil.rmtree(repo)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
