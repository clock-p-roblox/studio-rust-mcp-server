from __future__ import annotations

import argparse
import contextlib
import io
import traceback
from pathlib import Path

from . import output
from .errors import BridgeError
from .session import load_session
from .studio import (
    ensure_edit_mode,
    mode,
    official_generate_mesh,
    official_generate_procedural_model,
    official_insert_from_creator_store,
    official_ping,
    official_search_creator_store,
    official_store_image,
    official_wait_job,
    play,
    play_mode_logs,
    run_code_direct,
    screenshot,
    status,
    stop,
)

KNOWN_COMMANDS = {
    "status",
    "mode",
    "ensure-edit",
    "play",
    "stop",
    "screenshot",
    "play-mode-logs",
    "run-code-direct",
    "run-code",
    "official-ping",
    "official-store-image",
    "official-generate-mesh",
    "official-generate-procedural-model",
    "official-wait-job",
    "official-search-creator-store",
    "official-insert-from-creator-store",
}


class JSONArgumentParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        raise BridgeError("argument_error", message, {"usage": self.format_usage().strip()})


def main(argv: list[str]) -> int:
    command = _command_name(argv)
    captured_stdout = io.StringIO()
    captured_stderr = io.StringIO()
    try:
        with contextlib.redirect_stdout(captured_stdout), contextlib.redirect_stderr(captured_stderr):
            args = _parse_args(argv)
            command = args.command
            details = _run_command(args)
        return output.success(command, details)
    except BridgeError as exc:
        details = dict(exc.details)
        _attach_captured(details, captured_stdout, captured_stderr)
        return output.failure(command, exc.code, exc.message, details, exc.exit_code)
    except Exception as exc:  # noqa: BLE001 - CLI boundary must return JSON for all failures.
        details = {"traceback": traceback.format_exc()}
        _attach_captured(details, captured_stdout, captured_stderr)
        return output.failure(command, "unhandled_exception", str(exc), details)


def _parse_args(argv: list[str]) -> argparse.Namespace:
    if any(arg in ("-h", "--help") for arg in argv):
        raise BridgeError("help_requested", "help is not printed as text; use docs or inspect details.usage", {"usage": _build_parser().format_help()})
    parser = _build_parser()
    return parser.parse_args(argv)


def _build_parser() -> JSONArgumentParser:
    parser = JSONArgumentParser(prog="clockp-roblox-cli", add_help=False)
    parser.add_argument("--workspace", default=".", help="workspace directory containing .clock-p/session.json")
    subparsers = parser.add_subparsers(dest="command", required=True)

    for name in ("status", "mode", "ensure-edit", "play", "stop", "screenshot"):
        subparsers.add_parser(name, add_help=False)

    logs_parser = subparsers.add_parser("play-mode-logs", add_help=False)
    logs_parser.add_argument("--cursor", default=None)
    logs_parser.add_argument("--limit", type=int, default=None)

    for name in ("run-code-direct", "run-code"):
        run_parser = subparsers.add_parser(name, add_help=False)
        source = run_parser.add_mutually_exclusive_group(required=True)
        source.add_argument("--code")
        source.add_argument("--file")

    subparsers.add_parser("official-ping", add_help=False)

    store_image_parser = subparsers.add_parser("official-store-image", add_help=False)
    store_image_parser.add_argument("--file", required=True)

    mesh_parser = subparsers.add_parser("official-generate-mesh", add_help=False)
    mesh_parser.add_argument("--text-prompt", required=True)
    mesh_parser.add_argument("--size-x", type=float, default=None)
    mesh_parser.add_argument("--size-y", type=float, default=None)
    mesh_parser.add_argument("--size-z", type=float, default=None)
    mesh_parser.add_argument("--max-triangles", type=int, default=None)
    mesh_parser.add_argument("--part-names", default=None)
    mesh_parser.add_argument("--no-ensure-edit", action="store_true")

    procedural_parser = subparsers.add_parser("official-generate-procedural-model", add_help=False)
    procedural_parser.add_argument("--prompt", required=True)
    procedural_parser.add_argument("--attached-image-uri", default=None)
    procedural_parser.add_argument("--part-names", default=None)
    procedural_parser.add_argument("--no-ensure-edit", action="store_true")

    wait_parser = subparsers.add_parser("official-wait-job", add_help=False)
    wait_parser.add_argument("--generation-id", required=True)
    wait_parser.add_argument("--timeout", type=float, default=1.0)

    search_parser = subparsers.add_parser("official-search-creator-store", add_help=False)
    search_parser.add_argument("--query", default=None)
    search_parser.add_argument("--asset-type", default=None)
    search_parser.add_argument("--max-results", type=int, default=None)
    search_parser.add_argument("--price-filter", default=None)
    search_parser.add_argument("--min-price-cents", type=float, default=None)
    search_parser.add_argument("--max-price-cents", type=float, default=None)
    search_parser.add_argument("--verified-creators-only", action="store_true", default=None)

    insert_parser = subparsers.add_parser("official-insert-from-creator-store", add_help=False)
    insert_parser.add_argument("--asset-id", required=True)
    insert_parser.add_argument("--asset-name", default=None)
    insert_parser.add_argument("--asset-type", default=None)
    insert_parser.add_argument("--parent-path", default=None)
    insert_parser.add_argument("--no-ensure-edit", action="store_true")

    return parser


def _run_command(args: argparse.Namespace) -> dict:
    session = load_session(args.workspace)
    command = args.command
    if command == "status":
        return _checked_helper_result(command, status(session))
    if command == "mode":
        return _checked_helper_result(command, mode(session))
    if command == "ensure-edit":
        return ensure_edit_mode(session)
    if command == "play":
        return _checked_helper_result(command, play(session))
    if command == "stop":
        return _checked_helper_result(command, stop(session))
    if command == "screenshot":
        return _checked_helper_result(command, screenshot(session))
    if command == "play-mode-logs":
        return _checked_helper_result(command, play_mode_logs(session, args.cursor, args.limit))
    if command == "run-code-direct":
        return _checked_helper_result(command, run_code_direct(session, _read_code(args)))
    if command == "run-code":
        ensure_result = ensure_edit_mode(session)
        direct_result = _checked_helper_result(command, run_code_direct(session, _read_code(args)))
        return {"ensure_edit": ensure_result, "run_code": direct_result}
    if command == "official-ping":
        return _checked_helper_result(command, official_ping(session))
    if command == "official-store-image":
        return _checked_helper_result(command, official_store_image(session, args.file))
    if command == "official-generate-mesh":
        ensure_result = None if args.no_ensure_edit else ensure_edit_mode(session)
        official_result = _checked_helper_result(command, official_generate_mesh(session, _mesh_payload(args)))
        return _with_optional_ensure(ensure_result, official_result)
    if command == "official-generate-procedural-model":
        ensure_result = None if args.no_ensure_edit else ensure_edit_mode(session)
        official_result = _checked_helper_result(command, official_generate_procedural_model(session, _procedural_payload(args)))
        return _with_optional_ensure(ensure_result, official_result)
    if command == "official-wait-job":
        return _checked_helper_result(command, official_wait_job(session, _wait_job_payload(args)))
    if command == "official-search-creator-store":
        return _checked_helper_result(command, official_search_creator_store(session, _search_payload(args)))
    if command == "official-insert-from-creator-store":
        ensure_result = None if args.no_ensure_edit else ensure_edit_mode(session)
        official_result = _checked_helper_result(command, official_insert_from_creator_store(session, _insert_payload(args)))
        return _with_optional_ensure(ensure_result, official_result)
    raise BridgeError("unknown_command", f"unknown command: {command}")


def _checked_helper_result(command: str, result: dict) -> dict:
    if result.get("ok") is not False:
        return result
    command_result = result.get("command_result")
    command_error = ""
    if isinstance(command_result, dict):
        command_error = str(command_result.get("error") or "")
    code = str(result.get("code") or "helper_command_failed")
    message = str(result.get("message") or result.get("error") or command_error or f"{command} failed")
    raise BridgeError(code, message, result)


def _read_code(args: argparse.Namespace) -> str:
    if args.code is not None:
        return args.code
    path = Path(args.file)
    try:
        return path.read_text(encoding="utf-8-sig")
    except OSError as exc:
        raise BridgeError("code_file_read_failed", str(exc), {"path": str(path)}) from exc


def _with_optional_ensure(ensure_result: dict | None, official_result: dict) -> dict:
    if ensure_result is None:
        return official_result
    return {"ensure_edit": ensure_result, "official": official_result}


def _mesh_payload(args: argparse.Namespace) -> dict:
    payload: dict = {"text_prompt": args.text_prompt}
    _copy_if_present(payload, "max_triangles", args.max_triangles)
    _copy_if_present(payload, "part_names", args.part_names)
    size_values = (args.size_x, args.size_y, args.size_z)
    if any(value is not None for value in size_values):
        if not all(value is not None for value in size_values):
            raise BridgeError("argument_error", "--size-x, --size-y and --size-z must be provided together")
        payload["size"] = {"x": args.size_x, "y": args.size_y, "z": args.size_z}
    return payload


def _procedural_payload(args: argparse.Namespace) -> dict:
    payload: dict = {"prompt": args.prompt}
    _copy_if_present(payload, "attached_image_uri", args.attached_image_uri)
    _copy_if_present(payload, "part_names", args.part_names)
    return payload


def _wait_job_payload(args: argparse.Namespace) -> dict:
    payload: dict = {"generation_id": args.generation_id}
    _copy_if_present(payload, "timeout", args.timeout)
    return payload


def _search_payload(args: argparse.Namespace) -> dict:
    payload: dict = {}
    _copy_if_present(payload, "query", args.query)
    _copy_if_present(payload, "asset_type", args.asset_type)
    _copy_if_present(payload, "max_results", args.max_results)
    _copy_if_present(payload, "price_filter", args.price_filter)
    _copy_if_present(payload, "min_price_cents", args.min_price_cents)
    _copy_if_present(payload, "max_price_cents", args.max_price_cents)
    if args.verified_creators_only is True:
        payload["verified_creators_only"] = True
    return payload


def _insert_payload(args: argparse.Namespace) -> dict:
    payload: dict = {"asset_id": args.asset_id}
    _copy_if_present(payload, "asset_name", args.asset_name)
    _copy_if_present(payload, "asset_type", args.asset_type)
    _copy_if_present(payload, "parent_path", args.parent_path)
    return payload


def _copy_if_present(payload: dict, key: str, value: object | None) -> None:
    if value is None:
        return
    if isinstance(value, str) and value == "":
        return
    payload[key] = value


def _command_name(argv: list[str]) -> str:
    for arg in argv:
        if arg in KNOWN_COMMANDS:
            return arg
    return "unknown"


def _attach_captured(details: dict, captured_stdout: io.StringIO, captured_stderr: io.StringIO) -> None:
    stdout_text = captured_stdout.getvalue()
    stderr_text = captured_stderr.getvalue()
    if stdout_text:
        details["captured_stdout"] = stdout_text
    if stderr_text:
        details["captured_stderr"] = stderr_text
