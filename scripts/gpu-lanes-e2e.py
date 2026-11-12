#!/usr/bin/env python3
"""Exercise every GPU lane through the conductor and report one honest verdict.

What this checks
---------------
For each lane the conductor exposes — Kaggle, molab, Saturn Cloud and Cerebrium — this asks the
lane's own status route, decides readiness from the fields that lane publishes (never from a guess
about the platform), lists the accounts the lane holds, and, where the lane has a probe route,
probes each account so the platform gets a chance to disagree. It also reads the GPU catalogue and
the deployment list, because a lane that is ready but has no deployment is not serving anything.

What it does not do
-------------------
It starts nothing and spends nothing. Kaggle is batch-only: proving it end to end means submitting a
kernel, which consumes the operator's weekly GPU allowance and posts a notebook to their account, so
that stays a deliberate manual step (see `docs/26-gpu-lanes.md`). A green Saturn or Cerebrium probe
means the platform answered the control-plane call; only a real completion proves inference.

Exit status is 0 only when every lane is ready and every probe that ran succeeded. Anything less is
a failure with the reason printed, because a test that always passes tests nothing.

Usage
-----
    scripts/gpu-lanes-e2e.py                 # human-readable table
    scripts/gpu-lanes-e2e.py --json          # machine-readable summary
    scripts/gpu-lanes-e2e.py --no-probe      # status only, no platform calls
    scripts/gpu-lanes-e2e.py --base-url http://127.0.0.1:20998   # a test conductor

`MANAROUTER_ADMIN` sets the base URL when `--base-url` is absent.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

LANES = ("kaggle", "molab", "saturn", "cerebrium")


def request(base: str, path: str, body: dict | None, timeout: float) -> tuple[int, object]:
    """One HTTP call, returning the status code and the decoded body (or the raw text)."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data)
    if data is not None:
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read().decode("utf-8", "replace")
            status = response.status
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8", "replace")
        status = error.code
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        return 0, {"unreachable": str(error)}
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, raw


def readiness(lane: str, status: dict) -> tuple[bool, str]:
    """Whether a lane can run, and the lane's own sentence about why not.

    Each platform publishes a different shape, so each is read on its own terms. Nothing is
    inferred: `ready` is used where the lane publishes it, and otherwise the fields it does publish.
    """
    if lane == "kaggle":
        ready = bool(status.get("kaggle_cli_present")) and bool(status.get("credential_configured"))
        if ready:
            return True, "the kaggle CLI is present and a credential is configured"
        missing = []
        if not status.get("kaggle_cli_present"):
            missing.append("the kaggle CLI is not on PATH")
        if not status.get("credential_configured"):
            missing.append("no Kaggle credential is configured")
        return False, "; ".join(missing)
    if lane == "molab":
        warm = int(status.get("warm_lanes") or 0)
        if warm:
            return True, f"{warm} warm lane(s)"
        return False, str(status.get("detail") or "no warm lane")
    if lane == "saturn":
        accounts = status.get("accounts") or []
        plan = status.get("plan") or {}
        if accounts and plan.get("usable"):
            return True, f"{len(accounts)} account(s), the plan is usable"
        reasons = [str(r.get("message")) for r in plan.get("refusals") or [] if r.get("message")]
        return False, "; ".join(reasons) or "no Saturn account is attached"
    if lane == "cerebrium":
        if status.get("ready"):
            return True, f"{len(status.get('accounts') or [])} project(s) attached"
        return False, str(status.get("detail") or "no Cerebrium project is attached")
    return False, f"unknown lane {lane}"


def accounts_of(status: dict) -> list[str]:
    """The account ids a lane is holding, from whichever key it uses."""
    names = []
    for entry in status.get("accounts") or []:
        if isinstance(entry, dict):
            name = entry.get("account") or entry.get("id") or entry.get("label")
            if name:
                names.append(str(name))
    return names


def lane_words(lane: str, status: dict) -> list[str]:
    """The lane's own explanatory sentences, so the report quotes rather than paraphrases."""
    words = []
    for key in ("detail", "note", "notes"):
        value = status.get(key)
        if isinstance(value, str) and value.strip():
            words.append(value.strip())
    notes = status.get("notes")
    if isinstance(notes, list):
        for item in notes:
            if isinstance(item, str) and item.strip() and item not in words:
                words.append(item.strip())
    return words


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base-url", default=os.environ.get("MANAROUTER_ADMIN", "http://127.0.0.1:20901"))
    parser.add_argument("--json", action="store_true", help="print a machine-readable summary")
    parser.add_argument("--no-probe", action="store_true", help="do not call the platforms")
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()
    base = args.base_url.rstrip("/")

    summary: dict[str, object] = {"base_url": base, "lanes": {}, "catalog": {}, "rentals": {}, "ok": False}
    failures = 0

    for lane in LANES:
        code, status = request(base, f"/api/gpu/lanes/{lane}/status", None, args.timeout)
        entry: dict[str, object] = {"http_status": code}
        if code != 200 or not isinstance(status, dict):
            failures += 1
            entry["ready"] = False
            entry["detail"] = f"the status route answered {code}: {status}"
            summary["lanes"][lane] = entry
            continue
        ready, why = readiness(lane, status)
        entry.update({"ready": ready, "detail": why, "accounts": accounts_of(status), "words": lane_words(lane, status)})
        probes = []
        if ready and not args.no_probe and lane in ("saturn", "cerebrium"):
            for account in accounts_of(status):
                probe_code, probe_body = request(
                    base, f"/api/gpu/lanes/{lane}/probe", {"account": account}, args.timeout
                )
                ok = probe_code == 200 and not (isinstance(probe_body, dict) and probe_body.get("error"))
                probes.append({"account": account, "http_status": probe_code, "ok": ok, "body": probe_body})
                if not ok:
                    failures += 1
        entry["probes"] = probes
        if not ready:
            failures += 1
        summary["lanes"][lane] = entry

    code, catalog = request(base, "/api/gpu/catalog", None, args.timeout)
    if code == 200 and isinstance(catalog, dict):
        platforms = catalog.get("platforms") or []
        gpus = sum(len(p.get("gpus") or []) for p in platforms if isinstance(p, dict))
        summary["catalog"] = {"http_status": code, "platforms": len(platforms), "gpus": gpus}
    else:
        summary["catalog"] = {"http_status": code, "error": catalog}
        failures += 1

    code, rentals = request(base, "/api/gpu/rentals", None, args.timeout)
    if code == 200 and isinstance(rentals, dict):
        summary["rentals"] = {
            "http_status": code,
            "accounts": len(rentals.get("accounts") or []),
            "deployments": len(rentals.get("deployments") or []),
            "gpu_seconds": (rentals.get("usage") or {}).get("gpu_seconds"),
            "cost_usd": (rentals.get("usage") or {}).get("cost_usd"),
        }
    else:
        summary["rentals"] = {"http_status": code, "error": rentals}
        failures += 1

    summary["ok"] = failures == 0
    summary["failures"] = failures

    if args.json:
        print(json.dumps(summary, indent=2, sort_keys=True))
    else:
        print(f"GPU lanes e2e against {base}")
        for lane in LANES:
            entry = summary["lanes"][lane]
            mark = "ok  " if entry.get("ready") else "FAIL"
            print(f"  [{mark}] {lane:10} {entry.get('detail')}")
            for word in entry.get("words") or []:
                if word != entry.get("detail"):
                    print(f"           {word}")
            for probe in entry.get("probes") or []:
                state = "ok  " if probe["ok"] else "FAIL"
                print(f"           [{state}] probe {probe['account']}: http {probe['http_status']}")
        catalog = summary["catalog"]
        rentals = summary["rentals"]
        print(
            f"  catalog: http {catalog.get('http_status')}, {catalog.get('platforms')} platform(s), "
            f"{catalog.get('gpus')} GPU id(s)"
        )
        print(
            f"  rentals: http {rentals.get('http_status')}, {rentals.get('accounts')} rental account(s), "
            f"{rentals.get('deployments')} deployment(s), {rentals.get('gpu_seconds')} GPU-second(s), "
            f"${rentals.get('cost_usd')}"
        )
        print("verdict:", "every lane is ready" if summary["ok"] else f"{failures} failure(s)")

    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
