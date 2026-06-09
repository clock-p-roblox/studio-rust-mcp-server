#!/usr/bin/env python3
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, wait
import json
import sys
import time
import urllib.error
import urllib.request


DEFAULT_REQUEST_TIMEOUT_SEC = 0.75


def load_json(url: str, *, timeout: float = DEFAULT_REQUEST_TIMEOUT_SEC) -> dict:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"GET {url} failed with HTTP {exc.code}: {body}") from exc
    except urllib.error.URLError as exc:
        raise RuntimeError(f"GET {url} failed: {exc}") from exc


def load_bytes(url: str, *, timeout: float = DEFAULT_REQUEST_TIMEOUT_SEC) -> bytes:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            if response.status >= 500:
                raise RuntimeError(f"GET {url} failed with HTTP {response.status}")
            return response.read()
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"GET {url} failed with HTTP {exc.code}: {body}") from exc
    except urllib.error.URLError as exc:
        raise RuntimeError(f"GET {url} failed: {exc}") from exc


def collect_readonly_snapshots(readers: dict[str, object], *, timeout: float) -> tuple[dict[str, object], list[str], int]:
    if not readers:
        return {}, [], 0

    started = time.perf_counter()
    results: dict[str, object] = {}
    errors: list[str] = []
    max_workers = min(len(readers), 16)

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = {executor.submit(reader): label for label, reader in readers.items()}  # type: ignore[arg-type]
        done, not_done = wait(futures, timeout=max(timeout + 0.25, 0.25))
        for future in done:
            label = futures[future]
            try:
                results[label] = future.result()
            except RuntimeError as exc:
                errors.append(f"{label}: {exc}")
            except Exception as exc:  # noqa: BLE001
                errors.append(f"{label}: {type(exc).__name__}: {exc}")
        for future in not_done:
            future.cancel()

    elapsed_ms = int((time.perf_counter() - started) * 1000)
    missing = [label for label in readers if label not in results and not any(error.startswith(f"{label}:") for error in errors)]
    for label in missing:
        errors.append(f"{label}: did not complete within parallel preflight budget")
    return results, errors, elapsed_ms


def try_load_json(label: str, url: str, errors: list[str], *, timeout: float) -> dict | None:
    try:
        return load_json(url, timeout=timeout)
    except RuntimeError as exc:
        errors.append(f"{label}: {exc}")
        return None


def expect_equal(label: str, actual, expected, errors: list[str]) -> None:
    if actual != expected:
        errors.append(f"{label} mismatch: expected {expected!r}, got {actual!r}")


def check_hub_task(task: dict, *, task_id: str, helper_id: str, place_id: str, errors: list[str]) -> None:
    expect_equal("hub task_id", task.get("task_id"), task_id, errors)
    expect_equal("hub place_id", str(task.get("place_id")), place_id, errors)
    expect_equal("hub service_state", task.get("service_state"), "ready", errors)
    expect_equal("hub accepting_launches", task.get("accepting_launches"), True, errors)
    expect_equal("hub released", task.get("released"), False, errors)
    claimed_by = task.get("claimed_by_helper_id")
    if claimed_by not in (None, helper_id):
        errors.append(f"hub task claimed_by_helper_id mismatch: expected {helper_id!r} or null, got {claimed_by!r}")


def check_hub_helper(helper: dict, *, task_id: str, helper_id: str, require_active_task: bool, errors: list[str]) -> None:
    expect_equal("hub helper_id", helper.get("helper_id"), helper_id, errors)
    expect_equal("hub helper blocked", helper.get("blocked"), False, errors)
    active_tasks = helper.get("active_tasks") or []
    matching = [task for task in active_tasks if task.get("task_id") == task_id]
    if require_active_task and not matching:
        errors.append(f"hub helper active_tasks does not include task {task_id}")


def check_hub_status(payload: dict, *, task_id: str, helper_id: str, place_id: str, errors: list[str]) -> None:
    expect_equal("hub status ok", payload.get("ok"), True, errors)
    matching_tasks = [task for task in payload.get("tasks", []) if task.get("task_id") == task_id]
    if len(matching_tasks) != 1:
        errors.append(f"hub status task count mismatch: expected exactly one task {task_id}, got {len(matching_tasks)}")

    claimed_same_place = []
    for helper in payload.get("helpers", []):
        for task in helper.get("active_tasks", []) or []:
            if str(task.get("place_id")) == place_id:
                claimed_same_place.append((helper.get("helper_id"), task.get("task_id")))
    unexpected = [item for item in claimed_same_place if item != (helper_id, task_id)]
    if unexpected:
        errors.append(f"hub status has unexpected claimed tasks for place {place_id}: {unexpected!r}")


def check_helper_status(payload: dict, *, task_id: str, helper_id: str, place_id: str, errors: list[str]) -> None:
    expect_equal("helper status helper_id", payload.get("helper_id"), helper_id, errors)
    claimed = [task for task in payload.get("claimed_tasks", []) or [] if task.get("task_id") == task_id]
    if len(claimed) != 1:
        errors.append(f"helper status claimed task count mismatch: expected exactly one task {task_id}, got {len(claimed)}")
    for task in claimed:
        expect_equal("helper status claimed place_id", str(task.get("place_id")), place_id, errors)

    launched = [studio for studio in payload.get("launched_studios", []) or [] if studio.get("task_id") == task_id]
    if len(launched) > 1:
        errors.append(f"helper status has multiple launched studios for task {task_id}: {len(launched)}")
    for studio in launched:
        expect_equal("helper status launched place_id", str(studio.get("place_id")), place_id, errors)


def check_helper_task(payload: dict, *, task_id: str, place_id: str, require_plugin: bool, errors: list[str]) -> None:
    expect_equal("helper debug ok", payload.get("ok"), True, errors)
    expect_equal("helper debug task_id", payload.get("task_id"), task_id, errors)
    claimed = payload.get("claimed_task") or {}
    expect_equal("helper claimed task_id", claimed.get("task_id"), task_id, errors)
    expect_equal("helper claimed place_id", str(claimed.get("place_id")), place_id, errors)
    launch = payload.get("launch_process")
    if launch is not None:
        expect_equal("helper launch task_id", launch.get("task_id"), task_id, errors)
        expect_equal("helper launch place_id", str(launch.get("place_id")), place_id, errors)
        if not launch.get("studio_pid"):
            errors.append("helper launch_process has no studio_pid")
    instances = payload.get("instances") or []
    if require_plugin and not instances:
        errors.append("helper debug instances is empty; Studio plugin is not registered")
    for instance in instances:
        expect_equal("helper instance task_id", instance.get("task_id"), task_id, errors)
        expect_equal("helper instance place_id", str(instance.get("place_id")), place_id, errors)


def check_server_state(payload: dict, *, task_id: str, require_helper: bool, errors: list[str]) -> None:
    expect_equal("server debug ok", payload.get("ok"), True, errors)
    expect_equal("server task_id", payload.get("task_id"), task_id, errors)
    helper = payload.get("active_helper")
    if require_helper and helper is None:
        errors.append("server active_helper is missing")
    if helper is not None:
        expect_equal("server active_helper task_id", helper.get("task_id"), task_id, errors)
        task_status = helper.get("task_status")
        if task_status is not None:
            expect_equal("server helper task_status task_id", task_status.get("task_id"), task_id, errors)


def check_server_status(payload: dict, *, task_id: str, errors: list[str]) -> None:
    expect_equal("server status task_id", payload.get("task_id"), task_id, errors)
    if payload.get("status_source") not in ("hub", "hub_unconfigured"):
        errors.append(f"server status_source is not healthy: {payload.get('status_source')!r}")


def check_studio_log_payload(payload: dict, errors: list[str]) -> None:
    lines = "\n".join(payload.get("lines") or [])
    bad_markers = [
        "DataModelLoadingFailure",
        "Error fetching latest place version",
        "Place does not exist",
    ]
    for marker in bad_markers:
        if marker in lines:
            errors.append(f"Studio log contains {marker!r}; place did not open cleanly")


def check_rojo_probe(payload: bytes, errors: list[str]) -> None:
    if not payload:
        errors.append("rojo probe returned empty body")


def main() -> int:
    parser = argparse.ArgumentParser(description="Read hub/helper/server debug HTTP state before Studio testing.")
    parser.add_argument("--hub-url", required=True)
    parser.add_argument("--helper-url", required=True)
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--helper-id", required=True)
    parser.add_argument("--place-id", required=True)
    parser.add_argument("--require-helper-active-task", action="store_true")
    parser.add_argument("--require-plugin", action="store_true")
    parser.add_argument("--require-server-helper", action="store_true")
    parser.add_argument("--check-studio-log", action="store_true")
    parser.add_argument("--rojo-url")
    parser.add_argument("--timeout-sec", type=float, default=DEFAULT_REQUEST_TIMEOUT_SEC)
    parser.add_argument("--max-elapsed-ms", type=int, default=1000)
    args = parser.parse_args()

    hub_url = args.hub_url.rstrip("/")
    helper_url = args.helper_url.rstrip("/")
    server_url = args.server_url.rstrip("/")
    task_id = args.task_id
    helper_id = args.helper_id
    place_id = args.place_id
    timeout = args.timeout_sec

    try:
        errors: list[str] = []
        readers = {
            "hub status": lambda: load_json(f"{hub_url}/status", timeout=timeout),
            "hub task": lambda: load_json(f"{hub_url}/v1/tasks/{task_id}", timeout=timeout),
            "hub helper": lambda: load_json(f"{hub_url}/v1/debug/helpers/{helper_id}", timeout=timeout),
            "helper status": lambda: load_json(f"{helper_url}/status", timeout=timeout),
            "helper task": lambda: load_json(f"{helper_url}/v1/debug/tasks/{task_id}", timeout=timeout),
            "task-server status": lambda: load_json(f"{server_url}/status", timeout=timeout),
            "task-server state": lambda: load_json(f"{server_url}/v1/debug/state", timeout=timeout),
        }
        if args.check_studio_log:
            readers["studio log"] = lambda: load_json(
                f"{helper_url}/v1/helper/studio-log?start_line=-200&line_count=200",
                timeout=timeout,
            )
        if args.rojo_url:
            rojo_url = args.rojo_url.rstrip("/")
            readers["rojo api"] = lambda: load_bytes(f"{rojo_url}/api/rojo", timeout=timeout)

        snapshots, collect_errors, elapsed_ms = collect_readonly_snapshots(readers, timeout=timeout)
        errors.extend(collect_errors)
        if elapsed_ms > args.max_elapsed_ms:
            errors.append(
                f"parallel preflight exceeded budget: elapsed_ms={elapsed_ms}, budget_ms={args.max_elapsed_ms}"
            )

        hub_status = snapshots.get("hub status")
        hub_task = snapshots.get("hub task")
        hub_helper = snapshots.get("hub helper")
        helper_status = snapshots.get("helper status")
        helper_task = snapshots.get("helper task")
        server_status = snapshots.get("task-server status")
        server_state = snapshots.get("task-server state")
        studio_log = snapshots.get("studio log")
        rojo_api = snapshots.get("rojo api")

        if isinstance(hub_status, dict):
            check_hub_status(hub_status, task_id=task_id, helper_id=helper_id, place_id=place_id, errors=errors)
        if hub_task is not None:
            check_hub_task(hub_task, task_id=task_id, helper_id=helper_id, place_id=place_id, errors=errors)  # type: ignore[arg-type]
        if hub_helper is not None:
            check_hub_helper(
                hub_helper,  # type: ignore[arg-type]
                task_id=task_id,
                helper_id=helper_id,
                require_active_task=args.require_helper_active_task,
                errors=errors,
            )
        if isinstance(helper_status, dict):
            check_helper_status(helper_status, task_id=task_id, helper_id=helper_id, place_id=place_id, errors=errors)
        if helper_task is not None:
            check_helper_task(
                helper_task,  # type: ignore[arg-type]
                task_id=task_id,
                place_id=place_id,
                require_plugin=args.require_plugin,
                errors=errors,
            )
        if isinstance(server_status, dict):
            check_server_status(server_status, task_id=task_id, errors=errors)
        if server_state is not None:
            check_server_state(server_state, task_id=task_id, require_helper=args.require_server_helper, errors=errors)  # type: ignore[arg-type]
        if isinstance(studio_log, dict):
            check_studio_log_payload(studio_log, errors)
        if isinstance(rojo_api, bytes):
            check_rojo_probe(rojo_api, errors)
        if errors:
            raise RuntimeError("; ".join(errors))
    except RuntimeError as exc:
        print(f"[studio_debug_preflight] FAIL: {exc}", file=sys.stderr)
        return 1

    print("[studio_debug_preflight] OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
