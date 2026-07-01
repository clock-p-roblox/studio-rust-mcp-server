from __future__ import annotations

from dataclasses import dataclass, field

from ..errors import BridgeError


def _blake3_hex(data: bytes) -> str:
    try:
        import blake3  # type: ignore[import-not-found]
    except ModuleNotFoundError as exc:
        raise BridgeError(
            "code_sync_missing_dependency",
            "Python package 'blake3' is required for code-sync hashing",
            {"package": "blake3"},
        ) from exc
    return blake3.blake3(data).hexdigest()


def s(value: str | int) -> bytes:
    text = str(value)
    data = text.encode("utf-8")
    return str(len(data)).encode("ascii") + b":" + data


def hash_parts(*parts: bytes) -> str:
    return _blake3_hex(b"".join(parts))


def child_summary_bytes(name: str, kind: str, entry_hash: str) -> bytes:
    return s(name) + s(kind) + s(entry_hash)


def sort_key_name_kind(item: tuple[str, str, object]) -> tuple[bytes, bytes]:
    return (item[0].encode("utf-8"), item[1].encode("utf-8"))


@dataclass
class LogicalNode:
    name: str
    kind: str
    source: str = ""
    children: list["LogicalNode"] = field(default_factory=list)

    def entry_hash(self) -> str:
        summaries = [
            (child.name, child.kind, child.entry_hash())
            for child in self.children
        ]
        summaries.sort(key=sort_key_name_kind)
        encoded_children = b"".join(child_summary_bytes(name, kind, entry_hash) for name, kind, entry_hash in summaries)
        if self.kind == "Folder":
            return hash_parts(
                s("clockp.code_sync.v1.folder"),
                s(self.name),
                s(len(summaries)),
                encoded_children,
            )
        if self.kind in {"ModuleScript", "Script", "LocalScript"}:
            return hash_parts(
                s("clockp.code_sync.v1.script"),
                s(self.kind),
                s(self.name),
                s(self.source),
                s(len(summaries)),
                encoded_children,
            )
        raise BridgeError("code_sync_unsupported_mapping", f"unsupported logical node kind: {self.kind}", {"kind": self.kind})

    def child_summaries(self) -> list[dict]:
        children = sorted(self.children, key=lambda child: (child.name.encode("utf-8"), child.kind.encode("utf-8")))
        return [
            {
                "name": child.name,
                "entry_kind": child.kind,
                "entry_hash": child.entry_hash(),
            }
            for child in children
        ]

    def to_payload(self) -> dict:
        return {
            "name": self.name,
            "kind": self.kind,
            "source": self.source,
            "children": [child.to_payload() for child in sorted(self.children, key=lambda child: (child.name.encode("utf-8"), child.kind.encode("utf-8")))],
        }


def root_hash(root_id: str, mapping_profile: str, root: LogicalNode) -> str:
    return hash_parts(
        s("clockp.code_sync.v1.root"),
        s(root_id),
        s(mapping_profile),
        s(root.kind),
        s(root.entry_hash()),
    )


def combined_hash(mapping_profile: str, roots: list[tuple[str, str]]) -> str:
    ordered = sorted(roots, key=lambda item: item[0].encode("utf-8"))
    encoded = b"".join(s(root_id) + s(root_hash_value) for root_id, root_hash_value in ordered)
    return hash_parts(s("clockp.code_sync.v1.combined"), s(mapping_profile), s(len(ordered)), encoded)


def config_hash(protocol_version: int, mapping_profile: str, roots: list[dict]) -> str:
    root_parts = []
    for root in sorted(roots, key=lambda item: str(item["root_id"]).encode("utf-8")):
        studio_path = list(root["studio_path"])
        include = sorted(root.get("include", []), key=lambda item: str(item).encode("utf-8"))
        exclude = sorted(root.get("exclude", []), key=lambda item: str(item).encode("utf-8"))
        root_parts.append(
            s(str(root["root_id"]))
            + s(str(root["local_path"]).replace("\\", "/"))
            + s(len(studio_path))
            + b"".join(s(str(segment)) for segment in studio_path)
            + s(len(include))
            + b"".join(s(str(pattern)) for pattern in include)
            + s(len(exclude))
            + b"".join(s(str(pattern)) for pattern in exclude)
        )
    return hash_parts(
        s("clockp.code_sync.v1.config"),
        s(protocol_version),
        s(mapping_profile),
        s(len(root_parts)),
        b"".join(root_parts),
    )
