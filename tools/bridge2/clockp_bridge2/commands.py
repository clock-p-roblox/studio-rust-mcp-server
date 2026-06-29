from __future__ import annotations

import argparse
import contextlib
import io
import traceback
from pathlib import Path

from . import output
from .errors import BridgeError
from .session import load_session
from .studio import ensure_edit_mode, mode, play, play_mode_logs, run_code_direct, screenshot, status, stop

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
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise BridgeError("code_file_read_failed", str(exc), {"path": str(path)}) from exc


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
