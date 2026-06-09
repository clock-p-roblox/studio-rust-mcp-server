#!/usr/bin/env python3
from __future__ import annotations

import argparse
import select
import socket
import threading


def pipe(left: socket.socket, right: socket.socket) -> None:
    sockets = [left, right]
    try:
        while True:
            readable, _, _ = select.select(sockets, [], [], 1.0)
            if not readable:
                continue
            for source in readable:
                data = source.recv(65536)
                if not data:
                    return
                target = right if source is left else left
                target.sendall(data)
    finally:
        for sock in sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            sock.close()


def serve(listen_host: str, listen_port: int, target_host: str, target_port: int) -> None:
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((listen_host, listen_port))
    listener.listen()
    print(f"[tcp_proxy] listening {listen_host}:{listen_port} -> {target_host}:{target_port}", flush=True)
    try:
        while True:
            client, _addr = listener.accept()
            try:
                upstream = socket.create_connection((target_host, target_port), timeout=5)
            except OSError:
                client.close()
                continue
            threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
    finally:
        listener.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen-host", default="127.0.0.1")
    parser.add_argument("--listen-port", type=int, required=True)
    parser.add_argument("--target-host", default="127.0.0.1")
    parser.add_argument("--target-port", type=int, required=True)
    args = parser.parse_args()
    serve(args.listen_host, args.listen_port, args.target_host, args.target_port)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
