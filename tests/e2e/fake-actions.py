#!/usr/bin/env python3
"""A stand-in for GitHub Actions in the e2e: runs a repository's dispatched
workflow (workflow_dispatch -> one reusable workflow of this repo) on this
machine, step by step, the way a runner would.

  fake-actions.py check <bare-repo> <workflow> <inputs-json>      validate inputs (exit 1 + message)
  fake-actions.py run <bare-repo> <workflow> <inputs-json> <run-dir> <cache-dir> [<secrets-json-file>]

Emulated, not executed: actions/checkout (git clone), dtolnay/rust-toolchain
and actions/setup-node (local toolchains), Swatinem/rust-cache (target dir
kept per app in <cache-dir>), actions/upload-artifact (zip into <run-dir>),
and the "Install SpacetimeDB CLI" download (SPACETIME_BIN put on PATH).
`docker` is podman. `secrets: inherit` passes the repository's secrets, and,
as on GitHub, only to a reusable workflow of the same organization.
Every `run:` step runs as written.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
REUSABLE = re.compile(r"^([\w-]+)/traum-haft/\.github/workflows/([\w.-]+)@main$")
ORG = "isp-insoft-gmbh"


def caller_workflow(bare, workflow):
    text = subprocess.run(["git", "-C", bare, "show", f"main:.github/workflows/{workflow}"],
                          check=True, capture_output=True, text=True).stdout
    return yaml.safe_load(text)


def on(wf):
    # PyYAML reads the key `on` as True.
    return wf.get("on", wf.get(True))


def check(bare, workflow, inputs):
    declared = on(caller_workflow(bare, workflow))["workflow_dispatch"]["inputs"]
    unknown = set(inputs) - set(declared)
    if unknown:
        sys.exit(f"Unexpected inputs provided: {sorted(unknown)}")
    missing = [k for k, v in declared.items() if v.get("required") and k not in inputs]
    if missing:
        sys.exit(f"Required input '{missing[0]}' not provided")


def expr(text, ctx):
    def sub(m):
        name = m.group(1).strip()
        if name == "toJSON(secrets)" and "secrets" in ctx:
            return json.dumps(ctx["secrets"])
        scope, _, key = name.partition(".")
        if scope not in ctx or key not in ctx[scope]:
            raise SystemExit(f"unsupported expression ${{{{ {name} }}}}")
        return str(ctx[scope][key])
    return re.sub(r"\$\{\{\s*([^}]+?)\s*\}\}", sub, str(text))


def run(bare, workflow, dispatch_inputs, run_dir, cache_dir, secrets_file=None):
    run_dir = Path(run_dir)
    log = open(run_dir / "log.txt", "w")
    say = lambda s: (log.write(s + "\n"), log.flush())
    state = json.loads((run_dir / "run.json").read_text())

    def finish(conclusion):
        state.update(status="completed", conclusion=conclusion)
        (run_dir / "run.json").write_text(json.dumps(state))
        sys.exit(0)

    state["status"] = "in_progress"
    (run_dir / "run.json").write_text(json.dumps(state))
    caller = caller_workflow(bare, workflow)
    jobs = list(caller["jobs"].values())
    assert len(jobs) == 1, "one job expected"
    m = REUSABLE.match(jobs[0]["uses"])
    if not m:
        say(f"refusing workflow: {jobs[0]['uses']}")
        finish("failure")
    reusable = yaml.safe_load((ROOT / ".github/workflows" / m.group(2)).read_text())
    inputs = {k: expr(v, {"inputs": dispatch_inputs}) for k, v in jobs[0]["with"].items()}
    secrets = {"GITHUB_TOKEN": "ghs_fake_runner_token"}
    if jobs[0].get("secrets") == "inherit":
        if m.group(1) != ORG:
            # GitHub: inherit works only within one organization (or enterprise).
            say(f"secrets: inherit is not passed to {m.group(1)}/traum-haft (other organization)")
        elif secrets_file and Path(secrets_file).exists():
            secrets.update(json.loads(Path(secrets_file).read_text()))
    for k, spec in on(reusable)["workflow_call"]["inputs"].items():
        if spec.get("required") and k not in inputs:
            say(f"missing input {k}")
            finish("failure")
    ctx = {"inputs": inputs, "secrets": secrets}

    work = Path(tempfile.mkdtemp(prefix="runner-", dir=run_dir))
    temp = work / "_temp"
    temp.mkdir()
    ws = work / "ws"
    github_path = temp / "path"
    github_path.write_text("")
    env = dict(os.environ)
    env.update({k: expr(v, ctx) for k, v in (reusable.get("env") or {}).items()})
    env.update(RUNNER_TEMP=str(temp), GITHUB_PATH=str(github_path), CI="true")
    app_cache = Path(cache_dir) / inputs.get("app", Path(bare).stem).removesuffix("-preview")
    # `docker` on GitHub's runners; podman here.
    shim = temp / "bin"
    shim.mkdir()
    (shim / "docker").write_text('#!/bin/sh\nexec podman "$@"\n')
    (shim / "docker").chmod(0o755)
    github_path.write_text(f"{shim}\n")

    (job,) = reusable["jobs"].values()
    for step in job["steps"]:
        name = step.get("name") or step.get("uses")
        say(f"##[group]{name}")
        uses = step.get("uses", "")
        if uses.startswith("actions/checkout@"):
            subprocess.run(["git", "clone", "-q", "-b", "main", bare, str(ws)], check=True)
            ref = (step.get("with") or {}).get("ref")
            if ref:
                subprocess.run(["git", "-C", str(ws), "checkout", "-q", expr(ref, ctx)], check=True)
            say(f"HEAD {subprocess.run(['git', '-C', str(ws), 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip()}")
            continue
        if uses.startswith(("dtolnay/rust-toolchain@", "actions/setup-node@")):
            continue
        if uses.startswith("Swatinem/rust-cache@"):
            for w in step.get("with", {}).get("workspaces", ".").split():
                target = app_cache / w / "target"
                target.mkdir(parents=True, exist_ok=True)
                (ws / w / "target").symlink_to(target)
            continue
        if uses.startswith("actions/upload-artifact@"):
            w = step["with"]
            src = ws / w["path"]
            files = [p for p in src.rglob("*") if p.is_file()] if src.is_dir() else []
            if not files and w.get("if-no-files-found") == "error":
                say("No files were found with the provided path")
                finish("failure")
            with zipfile.ZipFile(run_dir / f"{w['name']}.zip", "w") as z:
                for p in files:
                    z.write(p, p.relative_to(src))
            say(f"uploaded {len(files)} files as {w['name']}")
            continue
        if uses:
            say(f"action not emulated: {uses}")
            finish("failure")
        script = step["run"]
        if name == "Install SpacetimeDB CLI":
            script = f'echo "{os.environ["SPACETIME_BIN"]}" >> "$GITHUB_PATH"'
        step_env = dict(env)
        step_env.update({k: expr(v, ctx) for k, v in (step.get("env") or {}).items()})
        extra = [p for p in github_path.read_text().splitlines() if p]
        step_env["PATH"] = os.pathsep.join(extra[::-1] + [env["PATH"]])
        cwd = ws / step.get("working-directory", ".")
        # GitHub's default shell for run steps.
        r = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", expr(script, ctx)],
                           cwd=cwd, env=step_env, stdout=log, stderr=subprocess.STDOUT)
        if r.returncode:
            say(f"##[error]Process completed with exit code {r.returncode}.")
            finish("failure")
    finish("success")


if __name__ == "__main__":
    cmd, bare, workflow, inputs = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
    if cmd == "check":
        check(bare, workflow, inputs)
    else:
        run(bare, workflow, inputs, sys.argv[5], sys.argv[6], sys.argv[7] if len(sys.argv) > 7 else None)
