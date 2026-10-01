"""Package CI inputs and gate releases on binary identity, coverage and evidence."""

import argparse
import json
import os
from pathlib import Path
import shutil

from common import GROUPS, PLATFORM, check_report, sha256, verify_manifest, write_json, write_junit


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def verify_evidence(gateway, evidence, group, platform, *, require_pipeline=False, checker=None):
    summary = read_json(evidence / "summary.json")
    digest = sha256(gateway)
    commit = os.environ.get("GITHUB_SHA")
    if (summary["group"], summary["platform"], summary["gateway_sha256"], summary["commit"]) != (group, platform, digest, commit):
        raise ValueError("E2E summary: group, platform, binary or commit mismatch")
    expected = list(GROUPS[group])
    if require_pipeline or any(suite["name"] == "pipeline" for suite in summary["suites"]):
        if group != "headless":
            raise ValueError("pipeline suite belongs to the headless group")
        expected.append("pipeline")
    if [suite["name"] for suite in summary["suites"]] != expected:
        raise ValueError("E2E summary: missing, duplicate or unexpected suites")
    reports = []
    for suite in summary["suites"]:
        name = suite["name"]
        if suite["exit_code"] != 0 or suite["timed_out"]:
            raise ValueError(f"{name}: unsuccessful suite process")
        directory = evidence / name
        verify_manifest(directory)
        report = read_json(directory / "report.json")
        check_report(report, name, digest, platform, commit)
        if checker and name in ("http", "pipeline") and report["grok_decoder_sha256"] != sha256(checker):
            raise ValueError(f"{name}: wire decoder mismatch")
        reports.append(report)
    return reports


def package(args):
    args.output.mkdir(parents=True, exist_ok=True)
    binaries = {}
    for name, path in (("gateway", args.gateway), ("checker", args.checker)):
        shutil.copy2(path, args.output / path.name)
        binaries[name] = {"file": path.name, "sha256": sha256(path)}
    for path, name in ((Path("Cargo.lock"), "gateway.Cargo.lock"),
                       (Path("tests/grok-wire/Cargo.lock"), "wire.Cargo.lock"),
                       (Path("tests/grok-wire.sha256"), "grok-wire.sha256")):
        shutil.copy2(path, args.output / name)
    write_json(args.output / "build.json", {"schema_version": 1, "commit": os.environ["GITHUB_SHA"],
        "platform": PLATFORM, "binaries": binaries})


def gate(args):
    reports = []
    verified = {"schema_version": 1, "commit": os.environ["GITHUB_SHA"],
        "run": f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}",
        "platforms": {}}
    args.output.mkdir(parents=True, exist_ok=True)
    for platform in ("linux", "windows"):
        build = args.artifacts / f"build-{platform}"
        metadata = read_json(build / "build.json")
        if metadata["commit"] != verified["commit"] or metadata["platform"] != platform:
            raise ValueError(f"{platform}: build identity mismatch")
        paths = {}
        for name, binary in metadata["binaries"].items():
            path = build / binary["file"]
            if path.parent != build or sha256(path) != binary["sha256"]:
                raise ValueError(f"{platform}: {name} build checksum mismatch")
            paths[name] = path
        platform_reports = []
        for group in GROUPS:
            platform_reports.extend(verify_evidence(paths["gateway"], args.artifacts / f"e2e-{platform}-{group}",
                group, platform, require_pipeline=group == "headless", checker=paths["checker"]))
        reports.extend(platform_reports)
        filename = "messages-gateway.exe" if platform == "windows" else "messages-gateway-linux-x64"
        shutil.copy2(paths["gateway"], args.output / filename)
        verified["platforms"][platform] = {"file": filename, "sha256": sha256(paths["gateway"]),
            "suites": [{"suite": report["suite"], "cases": len(report["cases"]),
                        "duration_seconds": report["duration_seconds"]} for report in platform_reports]}
    write_json(args.output / "verification.json", verified)
    write_junit(args.output / "junit.xml", reports)
    names = ("messages-gateway.exe", "messages-gateway-linux-x64", "verification.json", "junit.xml")
    (args.output / "SHA256SUMS").write_text("".join(f"{sha256(args.output / name)}  {name}\n" for name in names), encoding="utf-8")
    print(f"Verified {sum(len(report['cases']) for report in reports)} E2E cases across Windows and Linux")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    packaging = commands.add_parser("package")
    packaging.add_argument("--gateway", type=Path, required=True)
    packaging.add_argument("--checker", type=Path, required=True)
    packaging.add_argument("--output", type=Path, required=True)
    verification = commands.add_parser("verify-evidence")
    verification.add_argument("--gateway", type=Path, required=True)
    verification.add_argument("--evidence", type=Path, required=True)
    verification.add_argument("--group", choices=GROUPS, required=True)
    verification.add_argument("--platform", choices=("linux", "windows"), required=True)
    verification.add_argument("--require-pipeline", action="store_true")
    aggregate = commands.add_parser("gate")
    aggregate.add_argument("--artifacts", type=Path, required=True)
    aggregate.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "package":
        package(args)
    elif args.command == "gate":
        gate(args)
    else:
        reports = verify_evidence(args.gateway, args.evidence, args.group, args.platform, require_pipeline=args.require_pipeline)
        print(f"Verified {len(reports)} suites")
