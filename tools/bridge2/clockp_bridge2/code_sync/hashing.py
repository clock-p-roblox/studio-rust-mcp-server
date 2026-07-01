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


def canonical_studio_path(studio_path: list[str]) -> str:
    return "studio-path-v1:" + "".join(f"{len(segment.encode('utf-8'))}:{segment}" for segment in studio_path)


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


def target_hash(studio_path: list[str], mapping_profile: str, root: LogicalNode) -> str:
    return hash_parts(
        s("clockp.code_sync.v2.target"),
        s(canonical_studio_path(studio_path)),
        s(mapping_profile),
        s(root.kind),
        s(root.entry_hash()),
    )


def combined_hash(mapping_profile: str, targets: list[tuple[str, str]]) -> str:
    ordered = sorted(targets, key=lambda item: item[0].encode("utf-8"))
    encoded = b"".join(s(target_id) + s(target_hash_value) for target_id, target_hash_value in ordered)
    return hash_parts(s("clockp.code_sync.v2.combined"), s(mapping_profile), s(len(ordered)), encoded)


def config_hash(protocol_version: int, mapping_profile: str, nodes: list[dict]) -> str:
    node_parts = []
    for node in sorted(nodes, key=lambda item: canonical_studio_path(list(item["studio_path"])).encode("utf-8")):
        studio_path = list(node["studio_path"])
        include = sorted(node.get("include", []), key=lambda item: str(item).encode("utf-8"))
        exclude = sorted(node.get("exclude", []), key=lambda item: str(item).encode("utf-8"))
        node_parts.append(
            s(canonical_studio_path(studio_path))
            + s(str(node["kind"] or ""))
            + s(str(node["local_path"]).replace("\\", "/"))
            + s(len(studio_path))
            + b"".join(s(str(segment)) for segment in studio_path)
            + s(len(include))
            + b"".join(s(str(pattern)) for pattern in include)
            + s(len(exclude))
            + b"".join(s(str(pattern)) for pattern in exclude)
        )
    return hash_parts(
        s("clockp.code_sync.v2.config"),
        s(protocol_version),
        s(mapping_profile),
        s(len(node_parts)),
        b"".join(node_parts),
    )


def target_authority_hash(targets: list[list[str]]) -> str:
    target_ids = sorted(canonical_studio_path(target) for target in targets)
    encoded = b"".join(s(target_id) for target_id in target_ids)
    return hash_parts(s("clockp.code_sync.v2.target_authority"), s(len(target_ids)), encoded)
