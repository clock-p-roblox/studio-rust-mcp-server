from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path

from .code_sync.apply import apply_code_sync
from .errors import BridgeError
from .session import Session
from .studio import ensure_edit_mode, mode, play
from .workspace_config import load_workspace_config


def launch(
    session: Session,
    workspace: Path,
    prelaunch_path: Path,
    data: dict | None = None,
    wait_seconds: float = 0.0,
) -> dict:
    try:
        resolved_prelaunch_path = _resolve_workspace_path(
            workspace,
            prelaunch_path,
            "prelaunch_invalid_path",
            "prelaunch path must stay inside workspace",
        )
        config = load_prelaunch_config(resolved_prelaunch_path)
    except BridgeError as exc:
        raise _wrap_prelaunch_load_error(exc, prelaunch_path) from exc

    step_results: list[dict] = []
    for index, step in enumerate(config["steps"]):
        try:
            result = execute_step(session, workspace, step)
        except BridgeError as exc:
            raise BridgeError(
                "launch_prelaunch_failed",
                f"prelaunch step failed: {step.get('name') or step.get('kind') or index}",
                {
                    "step_index": index,
                    "step_name": step.get("name"),
                    "step_kind": step.get("kind"),
                    "step": step,
                    "prelaunch_results": step_results,
                    "step_error": _error_payload(exc),
                },
            ) from exc
        step_results.append(result)

    try:
        play_result = play(session, data)
    except BridgeError as exc:
        raise BridgeError(
            "launch_play_failed",
            "prelaunch succeeded but play failed",
            {
                "prelaunch_results": step_results,
                "play_error": _error_payload(exc),
            },
        ) from exc

    wait_result = None
    if wait_seconds > 0:
        started_at = time.monotonic()
        time.sleep(wait_seconds)
        wait_result = {
            "wait_ms": int((time.monotonic() - started_at) * 1000),
            "mode": mode(session),
        }

    return {
        "prelaunch": {
            "ok": True,
            "path": str(resolved_prelaunch_path),
            "steps": step_results,
        },
        "play": play_result,
        "post_play_wait": wait_result,
    }


def load_prelaunch_config(path: Path) -> dict:
    try:
        payload = json.loads(path.read_text(encoding="utf-8-sig"))
    except FileNotFoundError as exc:
        raise BridgeError("prelaunch_missing", "missing prelaunch.json; use play for direct launch or create prelaunch.json", {"path": str(path)}) from exc
    except UnicodeDecodeError as exc:
        raise BridgeError("prelaunch_invalid_config", str(exc), {"path": str(path)}) from exc
    except OSError as exc:
        raise BridgeError("prelaunch_read_failed", str(exc), {"path": str(path)}) from exc
    except json.JSONDecodeError as exc:
        raise BridgeError("prelaunch_invalid_config", str(exc), {"path": str(path)}) from exc

    if not isinstance(payload, dict):
        raise BridgeError("prelaunch_invalid_config", "prelaunch config must be a JSON object", {"path": str(path)})
    raw_steps = payload.get("steps")
    if not isinstance(raw_steps, list):
        raise BridgeError("prelaunch_invalid_config", "prelaunch.steps must be an array", {"path": str(path)})
    return {"steps": [_normalize_step(item, index) for index, item in enumerate(raw_steps)]}


def execute_step(session: Session, workspace: Path, step: dict) -> dict:
    kind = step["kind"]
    if kind == "ensure_edit":
        result = ensure_edit_mode(session)
        return {"kind": kind, "name": step.get("name"), "result": result}
    if kind == "shell":
        result = _run_shell_step(workspace, step)
        return {"kind": kind, "name": step.get("name"), **result}
    if kind == "code_sync_apply":
        config_path = _resolve_code_sync_config_path(workspace, step)
        if step["ensure_edit"]:
            ensure_edit_mode(session)
        result = apply_code_sync(session, workspace, config_path)
        return {
            "kind": kind,
            "name": step.get("name"),
            "config": step.get("config"),
            "ensure_edit": step["ensure_edit"],
            "result": result,
        }
    raise BridgeError("prelaunch_invalid_config", f"unsupported prelaunch step kind: {kind}", {"step": step})


def _normalize_step(value: object, index: int) -> dict:
    if not isinstance(value, dict):
        raise BridgeError("prelaunch_invalid_config", "prelaunch step must be an object", {"step_index": index, "step": value})
    kind = value.get("kind")
    if not isinstance(kind, str) or not kind:
        raise BridgeError("prelaunch_invalid_config", "prelaunch step kind must be a non-empty string", {"step_index": index, "step": value})
    name = value.get("name")
    if name is not None and (not isinstance(name, str) or not name):
        raise BridgeError("prelaunch_invalid_config", "prelaunch step name must be a non-empty string when present", {"step_index": index, "step": value})
    if kind == "ensure_edit":
        return {"kind": kind, "name": name}
    if kind == "shell":
        argv = value.get("argv")
        if not isinstance(argv, list) or not argv or not all(isinstance(item, str) and item for item in argv):
            raise BridgeError("prelaunch_invalid_config", "shell step argv must be a non-empty string array", {"step_index": index, "step": value})
        cwd = value.get("cwd", ".")
        if not isinstance(cwd, str) or not cwd:
            raise BridgeError("prelaunch_invalid_config", "shell step cwd must be a non-empty string", {"step_index": index, "step": value})
        return {"kind": kind, "name": name, "argv": list(argv), "cwd": cwd}
    if kind == "code_sync_apply":
        config = value.get("config")
        ensure_edit = value.get("ensure_edit", True)
        if config is not None and (not isinstance(config, str) or not config):
            raise BridgeError("prelaunch_invalid_config", "code_sync_apply config must be a non-empty string when present", {"step_index": index, "step": value})
        if not isinstance(ensure_edit, bool):
            raise BridgeError("prelaunch_invalid_config", "code_sync_apply ensure_edit must be a boolean", {"step_index": index, "step": value})
        return {"kind": kind, "name": name, "config": config, "ensure_edit": ensure_edit}
    raise BridgeError("prelaunch_invalid_config", f"unsupported prelaunch step kind: {kind}", {"step_index": index, "step": value})


def _run_shell_step(workspace: Path, step: dict) -> dict:
    cwd = _resolve_workspace_path(
        workspace,
        Path(step["cwd"]),
        "prelaunch_shell_invalid_cwd",
        "shell step cwd must stay inside workspace",
        {"step": step},
    )
    if not cwd.exists() or not cwd.is_dir():
        raise BridgeError("prelaunch_shell_invalid_cwd", "shell step cwd must exist and be a directory", {"cwd": str(cwd), "step": step})
    try:
        completed = subprocess.run(
            step["argv"],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError as exc:
        raise BridgeError(
            "prelaunch_shell_exec_failed",
            str(exc),
            {
                "argv": list(step["argv"]),
                "cwd": str(cwd),
            },
        ) from exc
    result = {
        "argv": list(step["argv"]),
        "cwd": str(cwd),
        "exit_code": completed.returncode,
        "stdout": completed.stdout,
        "stderr": completed.stderr,
    }
    if completed.returncode != 0:
        raise BridgeError("prelaunch_shell_failed", "shell step returned non-zero exit code", result)
    return result


def _resolve_code_sync_config_path(workspace: Path, step: dict) -> Path:
    config_value = step.get("config")
    if config_value is None:
        config_value = load_workspace_config(workspace).code_sync_config
    return _resolve_workspace_path(
        workspace,
        Path(config_value),
        "prelaunch_invalid_config_path",
        "code_sync_apply config path must stay inside workspace",
        {"step": step},
    )


def _resolve_workspace_path(
    workspace: Path,
    candidate: Path,
    error_code: str,
    error_message: str,
    extra_details: dict | None = None,
) -> Path:
    workspace_root = workspace.resolve()
    resolved = (workspace / candidate).resolve() if not candidate.is_absolute() else candidate.resolve()
    try:
        resolved.relative_to(workspace_root)
    except ValueError as exc:
        details = {"path": str(resolved), "workspace": str(workspace_root)}
        if extra_details:
            details.update(extra_details)
        raise BridgeError(error_code, error_message, details) from exc
    return resolved


def _wrap_prelaunch_load_error(exc: BridgeError, prelaunch_path: Path) -> BridgeError:
    return BridgeError(
        "launch_prelaunch_failed",
        "failed to load prelaunch config",
        {
            "step_index": None,
            "step_name": None,
            "step_kind": None,
            "step": None,
            "prelaunch_path": str(prelaunch_path),
            "prelaunch_results": [],
            "step_error": _error_payload(exc),
        },
    )


def _error_payload(exc: BridgeError) -> dict:
    return {
        "code": exc.code,
        "message": exc.message,
        "details": exc.details,
    }
