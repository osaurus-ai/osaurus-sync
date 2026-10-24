#!/usr/bin/env bash
# Prints the UDID of an iPhone simulator on the newest available iOS runtime, creating one
# named "osaurus-sync-ci" if none exists.
set -euo pipefail

xcrun simctl list -j devices available runtimes | python3 -c '
import json, subprocess, sys

data = json.load(sys.stdin)
runtimes = [r for r in data["runtimes"] if r["platform"] == "iOS" and r["isAvailable"]]
if not runtimes:
    sys.exit("no iOS simulator runtime is installed")
runtime = max(runtimes, key=lambda r: tuple(int(x) for x in r["version"].split(".")))
phones = [d for d in data["devices"].get(runtime["identifier"], []) if d["name"].startswith("iPhone") or d["name"] == "osaurus-sync-ci"]
if phones:
    print(phones[0]["udid"])
else:
    types = [t["identifier"] for t in runtime.get("supportedDeviceTypes", []) if t["name"].startswith("iPhone")]
    if not types:
        sys.exit("the iOS runtime supports no iPhone device type")
    out = subprocess.run(["xcrun", "simctl", "create", "osaurus-sync-ci", types[-1], runtime["identifier"]], check=True, capture_output=True, text=True)
    print(out.stdout.strip())
'
