#!/usr/bin/env python3
"""Regenerate src/prompt/prompt_encrypted.rs from templates/*.md.

The agent crate ships its three system-prompt templates XOR-encrypted
(position-dependent seeds) so the plaintext isn't greppable in the binary.
`prompt::template::tests::test_encrypted_templates_not_stale` re-derives the
ciphertext from templates/*.md and fails when this file is stale — re-run this
script after editing any template:

    python3 scripts/encrypt_templates.py

Run from the repository root.
"""

from pathlib import Path

AGENT_DIR = Path("crates/codegen/xai-grok-agent")
OUT = AGENT_DIR / "src/prompt/prompt_encrypted.rs"

# (const name, template path, seed) — order matches PROMPT_SEEDS.
ENTRIES = [
    ("BASE_PROMPT_ENC", "prompt.md", 0x5A),
    ("CODEX_PROMPT_ENC", "apply_patch_prompt.md", 0x7B),
    ("SUBAGENT_PROMPT_ENC", "subagent_prompt.md", 0x3D),
]

PER_LINE = 20


def xor_encrypt(data: bytes, seed: int) -> list[int]:
    # Mirrors prompt/template.rs: b ^ seed.wrapping_add(i as u8)
    return [b ^ ((seed + i) & 0xFF) for i, b in enumerate(data)]


def format_const(name: str, numbers: list[int]) -> str:
    parts = [str(n) for n in numbers]
    lines = []
    for i in range(0, len(parts), PER_LINE):
        chunk = ", ".join(parts[i : i + PER_LINE])
        lines.append(chunk + ",")
    # Last line loses its trailing comma.
    lines[-1] = lines[-1].rstrip(",")
    body = "\n".join(
        ("    " if i else "") + line for i, line in enumerate(lines)
    )
    return f"#[rustfmt::skip]\npub(crate) const {name}: &[u8] = &[\n{body}];\n"


def main() -> None:
    out = [
        "// Auto-generated -- do not edit.",
        "// Regenerate: python3 scripts/encrypt_templates.py",
        "// XOR-encrypted prompt templates (key = position-dependent seed).",
        "",
    ]
    for name, template, seed in ENTRIES:
        data = (AGENT_DIR / "templates" / template).read_bytes()
        out.append(format_const(name, xor_encrypt(data, seed)))
    seeds = ", ".join(f"0x{seed:02X}" for _, _, seed in ENTRIES)
    out.append(f"pub(crate) const PROMPT_SEEDS: [u8; 3] = [{seeds}];")
    OUT.write_text("\n".join(out) + "\n", encoding="utf-8")
    print(f"regenerated {OUT} from {len(ENTRIES)} templates")


if __name__ == "__main__":
    main()
