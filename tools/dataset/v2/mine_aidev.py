"""Pick merged agent PRs from hao-li/AIDev (parquet in CACHE) and clone them. Data stays under ~/aidev.

Run with the clef venv: mine_aidev.py [seed]. Needs gh (read-only API calls) and git.
"""
import json, os, random, re, subprocess, sys
import pandas as pd

HOME = os.path.expanduser("~/aidev")
CACHE = f"{HOME}/cache"
PERMISSIVE = {"MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause"}
LANGS = ["Python", "TypeScript", "JavaScript", "Rust"]
AGENTS = ["Claude_Code", "OpenAI_Codex", "Copilot", "Cursor", "Devin"]
SRC = re.compile(r"\.(?:py|ts|tsx|js|rs)$")
CMT = re.compile(r"^\+\s*(#(?!!)|//|/\*|\*(?!/)\s)")  # shortcut: line-based, misses trailing comments
# per-agent language quotas: cycle languages in this order, weighted toward Python/TS
LANG_CYCLE = ["Python", "TypeScript", "Python", "TypeScript", "JavaScript", "Rust"]


def sh(*a, cwd=None):
    r = subprocess.run(a, cwd=cwd, capture_output=True, text=True)
    return r.returncode, r.stdout.strip(), r.stderr.strip()


def candidates():
    p = pd.read_parquet(f"{CACHE}/pull_request.parquet")
    r = pd.read_parquet(f"{CACHE}/repository.parquet")
    m = p[p.merged_at.notna() & p.agent.isin(AGENTS)].merge(r, left_on="repo_id", right_on="id", suffixes=("", "_r"))
    m = m[m.language.isin(LANGS) & m.license.isin(PERMISSIVE) & (m.stars > 50) & ~m.is_forked]
    d = pd.read_parquet(f"{CACHE}/pr_commit_details.parquet", columns=["pr_id", "filename", "patch"])
    d = d[d.pr_id.isin(m.id) & d.filename.str.contains(SRC, na=False) & d.patch.notna()]
    d["n"] = d.patch.map(lambda s: sum(bool(CMT.match(l)) for l in s.split("\n")))
    n = d.groupby("pr_id").n.sum()
    m = m.assign(cmt=m.id.map(n).fillna(0))
    return m[m.cmt >= 3]


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    m = candidates()
    pools = {a: m[m.agent == a].sample(frac=1, random_state=seed) for a in AGENTS}
    used, rows = set(), []
    os.makedirs(f"{HOME}/repos", exist_ok=True)
    for k in range(200):  # round-robin agents, cycling languages
        if len(rows) >= 30:
            break
        for a in AGENTS:
            if len(rows) >= 30:
                break
            lang = LANG_CYCLE[(len(rows) // len(AGENTS) + AGENTS.index(a)) % len(LANG_CYCLE)]
            pool = pools[a]
            for want in (lang, None):
                c = pool[(pool.language == want) if want else pool.index.notna()]
                c = c[~c.repo_id.isin(used)]
                while len(c):
                    row = c.iloc[0]; c = c.iloc[1:]
                    used.add(row.repo_id)
                    rec = clone(row, a)
                    if rec:
                        rows.append(rec); print(len(rows), rec["repo"], a, rec["language"], flush=True); break
                else:
                    continue
                break
    with open(f"{HOME}/manifest.jsonl", "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    print("wrote", len(rows))


def clone(row, agent):
    repo = row.full_name; n = int(row.number)
    code, out, _ = sh("gh", "api", f"repos/{repo}/pulls/{n}")
    if code:
        return None
    pr = json.loads(out)
    path = f"{HOME}/repos/{repo.replace('/', '__')}"
    if not os.path.isdir(path):
        if sh("git", "clone", "-q", "--filter=blob:none", f"https://github.com/{repo}.git", path)[0]:
            return None
    if sh("git", "fetch", "-q", "origin", f"refs/pull/{n}/head:refs/aidev/pr{n}", cwd=path)[0]:
        return None
    sh("git", "fetch", "-q", "origin", pr["base"]["sha"], cwd=path)
    head = pr["head"]["sha"]
    c, base, _ = sh("git", "merge-base", pr["base"]["sha"], head, cwd=path)
    if c or sh("git", "rev-parse", "--verify", head + "^{commit}", cwd=path)[0]:
        return None
    if sh("git", "diff", "--quiet", base, head, "--", "*.py", "*.ts", "*.tsx", "*.js", "*.rs", cwd=path)[0] == 0:
        return None  # final head has no source-file change
    return dict(repo=repo, pr_number=n, agent=agent, language=row.language, license=row.license,
                stars=int(row.stars), base=base, head=head, local_path=path, url=row.html_url)


if __name__ == "__main__":
    main()
