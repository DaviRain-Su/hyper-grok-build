#!/usr/bin/env python3
"""Deterministically sync and audit Hyper's provider catalog against Pi.

Normal builds never run this script and it performs no network access by
default. Regeneration requires either a caller-supplied, digest-locked npm
archive or the explicit ``--download`` flag.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
from typing import Any, Iterable
import urllib.request


SCRIPT_DIR = Path(__file__).resolve().parent
PACKAGE_ROOT = SCRIPT_DIR.parent
LOCK_PATH = PACKAGE_ROOT / "pi_provider_lock.json"
EXCLUSIONS_PATH = PACKAGE_ROOT / "pi_provider_exclusions.json"
REGISTRY_PATH = PACKAGE_ROOT / "platform_registry.json"
CATALOG_PATH = PACKAGE_ROOT / "platform_catalog.json"
SNAPSHOT_PATH = PACKAGE_ROOT / "pi_catalog_snapshot.json"
PARITY_PATH = PACKAGE_ROOT / "pi_provider_parity.json"
DATA_PREFIX = "package/dist/providers/data/"
MANIFEST_MEMBER = f"{DATA_PREFIX}.manifest.json"
SNAPSHOT_SCHEMA_VERSION = 1
PARITY_SCHEMA_VERSION = 1
POLICY_SCHEMA_VERSION = 1
LOCK_SCHEMA_VERSION = 1

# Dynamic providers do not appear in Pi's generated static model shards, so
# their protocol capabilities must be audited separately. Keep this map total
# over lock.model_data.dynamic_providers; adding a new dynamic provider without
# declaring its wire is a sync error rather than a silent empty parity row.
DYNAMIC_PROVIDER_APIS: dict[str, tuple[str, ...]] = {
    "radius": ("pi-messages",),
}


class SyncError(RuntimeError):
    pass


def _reject_duplicate_keys(pairs: Iterable[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise SyncError(f"duplicate JSON key: {key!r}")
        value[key] = item
    return value


def parse_json_bytes(data: bytes, label: str) -> Any:
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=_reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError, SyncError) as error:
        raise SyncError(f"{label} is not valid strict UTF-8 JSON: {error}") from error


def read_json(path: Path) -> Any:
    try:
        return parse_json_bytes(path.read_bytes(), str(path))
    except OSError as error:
        raise SyncError(f"cannot read {path}: {error}") from error


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")


def compact_json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")


def digest(data: bytes, algorithm: str) -> str:
    return hashlib.new(algorithm, data).hexdigest()


def require_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise SyncError(f"{label} must be a JSON object")
    return value


def require_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or value.strip() != value:
        raise SyncError(f"{label} must be a non-empty string without surrounding whitespace")
    return value


def require_positive_int(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise SyncError(f"{label} must be a positive integer")
    return value


def validate_lock(lock: dict[str, Any]) -> None:
    if lock.get("schema_version") != LOCK_SCHEMA_VERSION:
        raise SyncError(f"unsupported lock schema: {lock.get('schema_version')!r}")
    commit = require_string(lock.get("pi_commit"), "lock.pi_commit")
    if len(commit) != 40 or any(char not in "0123456789abcdef" for char in commit):
        raise SyncError("lock.pi_commit must be a lowercase 40-character Git commit")
    package = require_object(lock.get("package"), "lock.package")
    for field in ("name", "version", "tarball_url", "npm_integrity", "sha1", "sha512"):
        require_string(package.get(field), f"lock.package.{field}")
    if len(package["sha512"]) != 128:
        raise SyncError("lock.package.sha512 must contain a hexadecimal SHA-512 digest")
    expected_integrity = "sha512-" + base64.b64encode(bytes.fromhex(package["sha512"])).decode("ascii")
    if package["npm_integrity"] != expected_integrity:
        raise SyncError("lock.package npm_integrity and hexadecimal sha512 disagree")
    model_data = require_object(lock.get("model_data"), "lock.model_data")
    for field in ("schema_version", "static_provider_count", "static_model_count"):
        require_positive_int(model_data.get(field), f"lock.model_data.{field}")
    require_string(model_data.get("generated_at"), "lock.model_data.generated_at")
    require_string(model_data.get("structure_hash"), "lock.model_data.structure_hash")
    require_string(model_data.get("manifest_sha256"), "lock.model_data.manifest_sha256")
    api_counts = require_object(model_data.get("api_model_counts"), "lock.model_data.api_model_counts")
    if not api_counts:
        raise SyncError("lock.model_data.api_model_counts must not be empty")
    for api, count in api_counts.items():
        require_string(api, "lock.model_data.api_model_counts key")
        require_positive_int(count, f"lock.model_data.api_model_counts[{api!r}]")
    dynamic = model_data.get("dynamic_providers")
    if not isinstance(dynamic, list) or not all(isinstance(item, str) and item for item in dynamic):
        raise SyncError("lock.model_data.dynamic_providers must be a string array")
    if dynamic != sorted(set(dynamic)):
        raise SyncError("lock.model_data.dynamic_providers must be sorted and unique")
    if set(dynamic) != set(DYNAMIC_PROVIDER_APIS):
        raise SyncError(
            "dynamic provider API declarations disagree with lock: "
            f"lock={dynamic}, declared={sorted(DYNAMIC_PROVIDER_APIS)}"
        )
    for provider, apis in DYNAMIC_PROVIDER_APIS.items():
        if not apis or tuple(sorted(set(apis))) != apis:
            raise SyncError(f"dynamic provider {provider} APIs must be sorted and unique")
    source_files = require_object(lock.get("source_files"), "lock.source_files")
    if not source_files:
        raise SyncError("lock.source_files must not be empty")
    for path, sha256 in source_files.items():
        require_string(path, "lock.source_files path")
        if not isinstance(sha256, str) or len(sha256) != 64:
            raise SyncError(f"lock.source_files[{path!r}] is not a SHA-256 digest")
    outputs = require_object(lock.get("outputs"), "lock.outputs")
    for field in ("snapshot_sha256", "runtime_catalog_sha256", "parity_report_sha256"):
        value = outputs.get(field)
        if not isinstance(value, str) or (value and len(value) != 64):
            raise SyncError(f"lock.outputs.{field} must be blank or a SHA-256 digest")


def acquire_archive(lock: dict[str, Any], archive: Path | None, download: bool) -> bytes | None:
    if archive is not None:
        try:
            data = archive.read_bytes()
        except OSError as error:
            raise SyncError(f"cannot read archive {archive}: {error}") from error
    elif download:
        url = lock["package"]["tarball_url"]
        request = urllib.request.Request(url, headers={"User-Agent": "hyper-pi-provider-sync/1"})
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                data = response.read()
        except OSError as error:
            raise SyncError(f"failed to download locked archive {url}: {error}") from error
    else:
        return None

    actual = digest(data, "sha512")
    expected = lock["package"]["sha512"]
    if actual != expected:
        raise SyncError(f"npm archive SHA-512 mismatch: expected {expected}, got {actual}")
    return data


def tar_member_bytes(archive: tarfile.TarFile, name: str) -> bytes:
    try:
        member = archive.getmember(name)
    except KeyError as error:
        raise SyncError(f"npm archive is missing {name}") from error
    if not member.isfile():
        raise SyncError(f"npm archive member {name} is not a regular file")
    extracted = archive.extractfile(member)
    if extracted is None:
        raise SyncError(f"cannot read npm archive member {name}")
    return extracted.read()


def validate_model(provider: str, api: str, model_id: str, value: Any) -> dict[str, Any]:
    label = f"{provider}/{model_id}"
    model = require_object(value, label)
    if model.get("id") != model_id:
        raise SyncError(f"{label} has mismatched id {model.get('id')!r}")
    if model.get("provider") != provider:
        raise SyncError(f"{label} has mismatched provider {model.get('provider')!r}")
    if model.get("api") != api:
        raise SyncError(f"{label} has mismatched api {model.get('api')!r}")
    name = model.get("name")
    if not isinstance(name, str) or not name.strip():
        raise SyncError(f"{label}.name must contain a non-blank string")
    base_url = model.get("baseUrl")
    if not isinstance(base_url, str) or base_url.strip() != base_url:
        raise SyncError(f"{label}.baseUrl must be a string without surrounding whitespace")
    if not isinstance(model.get("reasoning"), bool):
        raise SyncError(f"{label}.reasoning must be boolean")
    inputs = model.get("input")
    if not isinstance(inputs, list) or not inputs or any(item not in ("text", "image") for item in inputs):
        raise SyncError(f"{label}.input contains unsupported modalities")
    require_positive_int(model.get("contextWindow"), f"{label}.contextWindow")
    require_positive_int(model.get("maxTokens"), f"{label}.maxTokens")
    if "compat" in model and not isinstance(model["compat"], dict):
        raise SyncError(f"{label}.compat must be an object when present")
    if "thinkingLevelMap" in model and not isinstance(model["thinkingLevelMap"], dict):
        raise SyncError(f"{label}.thinkingLevelMap must be an object when present")
    return model


def validate_providers(
    providers_value: Any,
    lock: dict[str, Any],
) -> tuple[dict[str, dict[str, dict[str, Any]]], dict[str, int], dict[str, int]]:
    providers = require_object(providers_value, "snapshot.providers")
    normalized: dict[str, dict[str, dict[str, Any]]] = {}
    api_counts: dict[str, int] = {}
    provider_counts: dict[str, int] = {}
    structure: dict[str, dict[str, str]] = {}

    for provider, groups_value in providers.items():
        require_string(provider, "provider id")
        groups = require_object(groups_value, f"provider {provider}")
        if not groups:
            raise SyncError(f"provider {provider} contains no API groups")
        seen_models: set[str] = set()
        normalized_groups: dict[str, dict[str, Any]] = {}
        provider_structure: dict[str, str] = {}
        for api, models_value in groups.items():
            require_string(api, f"provider {provider} API id")
            models = require_object(models_value, f"provider {provider} API {api}")
            if not models:
                raise SyncError(f"provider {provider} API {api} contains no models")
            normalized_models: dict[str, Any] = {}
            for model_id, model_value in models.items():
                require_string(model_id, f"provider {provider} model id")
                if model_id in seen_models:
                    raise SyncError(f"{provider}/{model_id} appears in more than one API group")
                seen_models.add(model_id)
                normalized_models[model_id] = validate_model(provider, api, model_id, model_value)
                provider_structure[model_id] = api
                api_counts[api] = api_counts.get(api, 0) + 1
            normalized_groups[api] = normalized_models
        normalized[provider] = normalized_groups
        provider_counts[provider] = len(seen_models)
        structure[provider] = provider_structure

    expected_provider_count = lock["model_data"]["static_provider_count"]
    if len(normalized) != expected_provider_count:
        raise SyncError(f"provider count mismatch: expected {expected_provider_count}, got {len(normalized)}")
    model_count = sum(provider_counts.values())
    expected_model_count = lock["model_data"]["static_model_count"]
    if model_count != expected_model_count:
        raise SyncError(f"model count mismatch: expected {expected_model_count}, got {model_count}")
    if api_counts != lock["model_data"]["api_model_counts"]:
        raise SyncError(
            "API model counts disagree with lock: "
            f"expected {lock['model_data']['api_model_counts']}, got {api_counts}"
        )
    structure_hash = digest(compact_json(structure), "sha256")
    if structure_hash != lock["model_data"]["structure_hash"]:
        raise SyncError(
            f"model structure hash mismatch: expected {lock['model_data']['structure_hash']}, got {structure_hash}"
        )
    return normalized, provider_counts, api_counts


def snapshot_from_archive(data: bytes, lock: dict[str, Any]) -> dict[str, Any]:
    try:
        archive = tarfile.open(fileobj=io.BytesIO(data), mode="r:gz")
    except tarfile.TarError as error:
        raise SyncError(f"locked npm artifact is not a valid tar.gz archive: {error}") from error
    with archive:
        manifest_bytes = tar_member_bytes(archive, MANIFEST_MEMBER)
        expected_manifest_digest = lock["model_data"]["manifest_sha256"]
        actual_manifest_digest = digest(manifest_bytes, "sha256")
        if actual_manifest_digest != expected_manifest_digest:
            raise SyncError(
                f"model-data manifest digest mismatch: expected {expected_manifest_digest}, got {actual_manifest_digest}"
            )
        manifest = require_object(parse_json_bytes(manifest_bytes, MANIFEST_MEMBER), "model-data manifest")
        if manifest.get("schemaVersion") != lock["model_data"]["schema_version"]:
            raise SyncError("model-data manifest schemaVersion disagrees with lock")
        if manifest.get("generatedAt") != lock["model_data"]["generated_at"]:
            raise SyncError("model-data manifest generatedAt disagrees with lock")
        if manifest.get("structureHash") != lock["model_data"]["structure_hash"]:
            raise SyncError("model-data manifest structureHash disagrees with lock")
        file_hashes = require_object(manifest.get("files"), "model-data manifest.files")
        expected_members = {f"{DATA_PREFIX}{filename}" for filename in file_hashes}
        actual_members = {
            member.name
            for member in archive.getmembers()
            if member.isfile()
            and member.name.startswith(DATA_PREFIX)
            and member.name.endswith(".json")
            and member.name != MANIFEST_MEMBER
        }
        if actual_members != expected_members:
            missing = sorted(expected_members - actual_members)
            extra = sorted(actual_members - expected_members)
            raise SyncError(f"model-data shard set mismatch; missing={missing}, extra={extra}")

        providers: dict[str, Any] = {}
        for filename, expected_sha256 in file_hashes.items():
            if not filename.endswith(".json") or "/" in filename:
                raise SyncError(f"unsafe model-data manifest filename: {filename!r}")
            member_name = f"{DATA_PREFIX}{filename}"
            shard_bytes = tar_member_bytes(archive, member_name)
            actual_sha256 = digest(shard_bytes, "sha256")
            if actual_sha256 != expected_sha256:
                raise SyncError(f"{filename} digest mismatch: expected {expected_sha256}, got {actual_sha256}")
            provider = filename.removesuffix(".json")
            providers[provider] = parse_json_bytes(shard_bytes, member_name)

    normalized, _, _ = validate_providers(providers, lock)
    return {
        "schema_version": SNAPSHOT_SCHEMA_VERSION,
        "source": {
            "pi_commit": lock["pi_commit"],
            "package": lock["package"]["name"],
            "package_version": lock["package"]["version"],
            "model_data_generated_at": lock["model_data"]["generated_at"],
            "model_data_structure_hash": lock["model_data"]["structure_hash"],
        },
        "providers": normalized,
    }


def validate_snapshot(snapshot_value: Any, lock: dict[str, Any]) -> dict[str, Any]:
    snapshot = require_object(snapshot_value, "snapshot")
    if snapshot.get("schema_version") != SNAPSHOT_SCHEMA_VERSION:
        raise SyncError(f"unsupported snapshot schema: {snapshot.get('schema_version')!r}")
    source = require_object(snapshot.get("source"), "snapshot.source")
    expected_source = {
        "pi_commit": lock["pi_commit"],
        "package": lock["package"]["name"],
        "package_version": lock["package"]["version"],
        "model_data_generated_at": lock["model_data"]["generated_at"],
        "model_data_structure_hash": lock["model_data"]["structure_hash"],
    }
    if source != expected_source:
        raise SyncError(f"snapshot source metadata disagrees with lock: expected {expected_source}, got {source}")
    providers, _, _ = validate_providers(snapshot.get("providers"), lock)
    return {"schema_version": SNAPSHOT_SCHEMA_VERSION, "source": source, "providers": providers}


def validate_exclusions(
    policy_value: Any,
    lock: dict[str, Any],
    static_providers: dict[str, Any],
) -> tuple[dict[str, str], dict[str, dict[str, Any]]]:
    policy = require_object(policy_value, "exclusion policy")
    if policy.get("schema_version") != POLICY_SCHEMA_VERSION:
        raise SyncError(f"unsupported exclusion policy schema: {policy.get('schema_version')!r}")
    if policy.get("pi_commit") != lock["pi_commit"]:
        raise SyncError("exclusion policy pi_commit disagrees with lock")
    id_map_value = require_object(policy.get("provider_id_map"), "exclusion policy.provider_id_map")
    id_map: dict[str, str] = {}
    for pi_provider, hyper_provider in id_map_value.items():
        id_map[require_string(pi_provider, "provider_id_map key")] = require_string(
            hyper_provider, f"provider_id_map[{pi_provider!r}]"
        )
    known = set(static_providers) | set(lock["model_data"]["dynamic_providers"])
    if not set(id_map).issubset(known):
        raise SyncError(f"provider_id_map contains unknown Pi providers: {sorted(set(id_map) - known)}")

    exclusions_value = policy.get("exclusions")
    if not isinstance(exclusions_value, list):
        raise SyncError("exclusion policy.exclusions must be an array")
    exclusions: dict[str, dict[str, Any]] = {}
    order: list[str] = []
    for index, raw in enumerate(exclusions_value):
        item = require_object(raw, f"exclusions[{index}]")
        provider = require_string(item.get("pi_provider"), f"exclusions[{index}].pi_provider")
        if provider in exclusions:
            raise SyncError(f"duplicate exclusion for {provider}")
        if provider not in known:
            raise SyncError(f"exclusion references unknown Pi provider {provider}")
        kind = item.get("kind")
        expected_kind = "static" if provider in static_providers else "dynamic"
        if kind != expected_kind:
            raise SyncError(f"exclusion {provider} kind must be {expected_kind!r}")
        target_wave = item.get("target_wave")
        if target_wave not in (1, 2, 3, 4):
            raise SyncError(f"exclusion {provider} target_wave must be 1..4")
        require_string(item.get("reason_code"), f"exclusion {provider}.reason_code")
        required_apis = item.get("required_apis")
        if (
            not isinstance(required_apis, list)
            or not required_apis
            or not all(isinstance(api, str) and api for api in required_apis)
            or required_apis != sorted(set(required_apis))
        ):
            raise SyncError(f"exclusion {provider}.required_apis must be sorted and unique")
        if expected_kind == "static" and set(required_apis) != set(static_providers[provider]):
            raise SyncError(
                f"exclusion {provider} required_apis {required_apis} do not match snapshot APIs "
                f"{sorted(static_providers[provider])}"
            )
        if expected_kind == "dynamic" and tuple(required_apis) != DYNAMIC_PROVIDER_APIS[provider]:
            raise SyncError(
                f"exclusion {provider} required_apis {required_apis} do not match declared dynamic APIs "
                f"{list(DYNAMIC_PROVIDER_APIS[provider])}"
            )
        exclusions[provider] = item
        order.append(provider)
    if order != sorted(order):
        raise SyncError("exclusion entries must be sorted by pi_provider")
    return id_map, exclusions


def _raw_compat(model: dict[str, Any] | None, allowed: set[str], label: str) -> dict[str, Any]:
    if model is None:
        return {}
    raw = model.get("compat", {})
    if not isinstance(raw, dict):
        raise SyncError(f"{label}.compat must be an object")
    unknown = set(raw) - allowed
    if unknown:
        raise SyncError(f"{label}.compat contains unhandled fields: {sorted(unknown)}")
    return raw


def resolve_chat_compat(provider: str, model_id: str, base_url: str, model: dict[str, Any] | None) -> dict[str, Any]:
    allowed = {
        "supportsStore",
        "supportsDeveloperRole",
        "supportsReasoningEffort",
        "supportsUsageInStreaming",
        "maxTokensField",
        "requiresToolResultName",
        "requiresAssistantAfterToolResult",
        "requiresThinkingAsText",
        "requiresReasoningContentOnAssistantMessages",
        "thinkingFormat",
        "chatTemplateKwargs",
        "openRouterRouting",
        "vercelGatewayRouting",
        "zaiToolStream",
        "supportsOpenAIGrammarTools",
        "supportsStrictMode",
        "cacheControlFormat",
        "sendSessionAffinityHeaders",
        "deferredToolsMode",
        "sessionAffinityFormat",
        "supportsLongCacheRetention",
    }
    raw = _raw_compat(model, allowed, f"{provider}/{model_id}")
    is_zai = provider in ("zai", "zai-coding-cn", "zai-coding") or "api.z.ai" in base_url or "open.bigmodel.cn" in base_url
    is_together = provider == "together" or "api.together.ai" in base_url or "api.together.xyz" in base_url
    is_moonshot = provider in ("moonshotai", "moonshotai-cn") or "api.moonshot." in base_url
    is_openrouter = provider == "openrouter" or "openrouter.ai" in base_url
    is_cloudflare_workers = provider == "cloudflare-workers-ai" or "api.cloudflare.com" in base_url
    is_cloudflare_gateway = provider == "cloudflare-ai-gateway" or "gateway.ai.cloudflare.com" in base_url
    is_nvidia = provider == "nvidia" or "integrate.api.nvidia.com" in base_url
    is_ant_ling = provider == "ant-ling" or "api.ant-ling.com" in base_url
    is_non_standard = (
        is_nvidia
        or provider in ("cerebras", "xai", "opencode", "opencode-go", "ollama")
        or "cerebras.ai" in base_url
        or "api.x.ai" in base_url
        or is_together
        or "chutes.ai" in base_url
        or "deepseek.com" in base_url
        or is_zai
        or is_moonshot
        or "opencode.ai" in base_url
        or is_cloudflare_workers
        or is_cloudflare_gateway
        or is_ant_ling
    )
    use_max_tokens = (
        "chutes.ai" in base_url
        or is_moonshot
        or is_cloudflare_gateway
        or is_together
        or is_nvidia
        or is_ant_ling
    )
    is_grok = provider == "xai" or "api.x.ai" in base_url
    is_deepseek = provider == "deepseek" or "deepseek.com" in base_url
    openrouter_developer = is_openrouter and (model_id.startswith("anthropic/") or model_id.startswith("openai/"))
    detected_thinking = (
        "deepseek"
        if is_deepseek
        else "zai"
        if is_zai
        else "together"
        if is_together
        else "ant-ling"
        if is_ant_ling
        else "openrouter"
        if is_openrouter
        else "openai"
    )
    detected = {
        "supports_store": not is_non_standard,
        "supports_developer_role": openrouter_developer or (not is_non_standard and not is_openrouter),
        "supports_reasoning_effort": not (
            is_grok or is_zai or is_moonshot or is_together or is_cloudflare_gateway or is_nvidia or is_ant_ling
        ),
        "supports_usage_in_streaming": True,
        "max_tokens_field": "max_tokens" if use_max_tokens else "max_completion_tokens",
        "requires_tool_result_name": False,
        "requires_assistant_after_tool_result": False,
        "requires_thinking_as_text": False,
        "requires_reasoning_content_on_assistant_messages": is_deepseek,
        "thinking_format": detected_thinking,
        "chat_template_kwargs": {},
        "openrouter_routing": {},
        "vercel_gateway_routing": {},
        "zai_tool_stream": False,
        "supports_openai_grammar_tools": False,
        "supports_strict_mode": not (is_moonshot or is_together or is_cloudflare_gateway or is_nvidia),
        "cache_control_format": "anthropic" if provider == "openrouter" and model_id.startswith("anthropic/") else None,
        "send_session_affinity_headers": False,
        "deferred_tools_mode": None,
        "session_affinity_format": "openrouter" if is_openrouter else "openai",
        "supports_long_cache_retention": not (
            is_together or is_cloudflare_workers or is_cloudflare_gateway or is_nvidia or is_ant_ling
        ),
    }
    mapping = {
        "supports_store": "supportsStore",
        "supports_developer_role": "supportsDeveloperRole",
        "supports_reasoning_effort": "supportsReasoningEffort",
        "supports_usage_in_streaming": "supportsUsageInStreaming",
        "max_tokens_field": "maxTokensField",
        "requires_tool_result_name": "requiresToolResultName",
        "requires_assistant_after_tool_result": "requiresAssistantAfterToolResult",
        "requires_thinking_as_text": "requiresThinkingAsText",
        "requires_reasoning_content_on_assistant_messages": "requiresReasoningContentOnAssistantMessages",
        "thinking_format": "thinkingFormat",
        "chat_template_kwargs": "chatTemplateKwargs",
        "openrouter_routing": "openRouterRouting",
        "vercel_gateway_routing": "vercelGatewayRouting",
        "zai_tool_stream": "zaiToolStream",
        "supports_openai_grammar_tools": "supportsOpenAIGrammarTools",
        "supports_strict_mode": "supportsStrictMode",
        "cache_control_format": "cacheControlFormat",
        "send_session_affinity_headers": "sendSessionAffinityHeaders",
        "deferred_tools_mode": "deferredToolsMode",
        "session_affinity_format": "sessionAffinityFormat",
        "supports_long_cache_retention": "supportsLongCacheRetention",
    }
    return {field: raw.get(source, default) for field, source in mapping.items() for default in [detected[field]]}


def resolve_responses_compat(provider: str, base_url: str, model_id: str, model: dict[str, Any] | None) -> dict[str, Any]:
    allowed = {
        "supportsDeveloperRole",
        "sessionAffinityFormat",
        "supportsLongCacheRetention",
        "supportsStrictMode",
        "supportsOpenAIGrammarTools",
        "supportsToolSearch",
        "supportsExplicitPromptCacheMode",
    }
    raw = _raw_compat(model, allowed, f"{provider}/{model_id}")
    affinity = "openrouter" if provider == "openrouter" or "openrouter.ai" in base_url else "openai"
    return {
        "supports_developer_role": raw.get("supportsDeveloperRole", True),
        "session_affinity_format": raw.get("sessionAffinityFormat", affinity),
        "supports_long_cache_retention": raw.get("supportsLongCacheRetention", True),
        "supports_strict_mode": raw.get("supportsStrictMode", False),
        "supports_openai_grammar_tools": raw.get("supportsOpenAIGrammarTools", False),
        "supports_tool_search": raw.get("supportsToolSearch", False),
        "supports_explicit_prompt_cache_mode": raw.get("supportsExplicitPromptCacheMode", False),
    }


def default_supports_tool_references(provider: str, model_id: str) -> bool:
    if provider != "anthropic" or "haiku" in model_id:
        return False
    import re

    match = re.match(r"^claude-(?:opus|sonnet|fable)-(\d+)(?:-(\d+))?(?:-|$)", model_id)
    if not match:
        return False
    major = int(match.group(1))
    minor_text = match.group(2)
    minor = int(minor_text) if minor_text and len(minor_text) < 8 else 0
    return major > 4 or (major == 4 and minor >= 5)


def resolve_messages_compat(provider: str, model_id: str, model: dict[str, Any] | None) -> dict[str, Any]:
    allowed = {
        "supportsEagerToolInputStreaming",
        "supportsLongCacheRetention",
        "sendSessionAffinityHeaders",
        "supportsCacheControlOnTools",
        "supportsTemperature",
        "forceAdaptiveThinking",
        "allowEmptySignature",
        "supportsStrictTools",
        "supportsToolReferences",
    }
    raw = _raw_compat(model, allowed, f"{provider}/{model_id}")
    return {
        "supports_eager_tool_input_streaming": raw.get("supportsEagerToolInputStreaming", True),
        "supports_long_cache_retention": raw.get("supportsLongCacheRetention", True),
        "send_session_affinity_headers": raw.get("sendSessionAffinityHeaders", False),
        "supports_cache_control_on_tools": raw.get("supportsCacheControlOnTools", True),
        "supports_temperature": raw.get("supportsTemperature", True),
        "force_adaptive_thinking": raw.get("forceAdaptiveThinking", False),
        "allow_empty_signature": raw.get("allowEmptySignature", False),
        "supports_strict_tools": raw.get("supportsStrictTools", False),
        "supports_tool_references": raw.get(
            "supportsToolReferences", default_supports_tool_references(provider, model_id)
        ),
    }


def resolve_bedrock_compat(provider: str, model_id: str, model: dict[str, Any] | None) -> dict[str, Any]:
    allowed = {
        "supportsStrictMode",
        "thinkingLevelMap",
    }
    raw = _raw_compat(model, allowed, f"{provider}/{model_id}")
    return {
        "supports_strict_mode": raw.get("supportsStrictMode", False),
        "thinking_level_map": {k: v for k, v in (model.get("thinkingLevelMap") or {}).items()} if model is not None else {},
    }


def find_pi_model(
    pi_provider: str,
    model_id: str,
    backend: str,
    providers: dict[str, dict[str, dict[str, Any]]],
) -> dict[str, Any] | None:
    api_names = {
        "chat_completions": ("openai-completions", "mistral-conversations"),
        "responses": ("openai-responses", "openai-codex-responses", "azure-openai-responses"),
        "messages": ("anthropic-messages",),
        "google_generate_content": ("google-generative-ai", "google-vertex"),
        "bedrock_converse_stream": ("bedrock-converse-stream",),
    }.get(backend)
    if api_names is None:
        raise SyncError(f"cannot enrich unsupported runtime backend {backend!r}")
    groups = providers.get(pi_provider, {})
    for api in api_names:
        model = groups.get(api, {}).get(model_id)
        if model is not None:
            return model
    return None


PI_API_TO_RUNTIME_BACKEND = {
    "anthropic-messages": "messages",
    "bedrock-converse-stream": "bedrock_converse_stream",
    "azure-openai-responses": "responses",
    "google-generative-ai": "google_generate_content",
    "google-vertex": "google_generate_content",
    "mistral-conversations": "chat_completions",
    "openai-codex-responses": "responses",
    "openai-completions": "chat_completions",
    "openai-responses": "responses",
}


def materialize_missing_active_models(
    catalog: dict[str, Any],
    snapshot: dict[str, Any],
    registry: dict[str, Any],
) -> dict[str, Any]:
    """Append missing Pi rows for active registry providers and supported wires.

    Existing rows are deliberately preserved: Hyper carries a few curated
    aliases and provider-specific fallbacks beyond Pi. Static model shards are
    materialized only for the API mapping below; dynamic providers such as
    Radius are audited separately via ``DYNAMIC_PROVIDER_APIS``.
    """
    if catalog.get("version") not in (2, 3) or not isinstance(catalog.get("models"), list):
        raise SyncError("platform_catalog.json must use catalog schema v2 or v3")
    registry_rows = registry.get("providers")
    if not isinstance(registry_rows, list):
        raise SyncError("platform_registry.json providers must be an array")

    output_rows = [dict(require_object(row, "platform catalog row")) for row in catalog["models"]]
    existing: set[tuple[str, str]] = set()
    for row in output_rows:
        key = (
            require_string(row.get("platform"), "platform catalog provider"),
            require_string(row.get("model"), "platform catalog model"),
        )
        if key in existing:
            raise SyncError(f"duplicate platform catalog model {key[0]}/{key[1]}")
        existing.add(key)

    candidates: list[tuple[str, str, str, dict[str, Any], dict[str, Any]]] = []
    providers = snapshot["providers"]
    for raw_registry_row in registry_rows:
        registry_row = require_object(raw_registry_row, "platform registry row")
        if registry_row.get("status") != "active" or registry_row.get("catalog_source") != "pi":
            continue
        hyper_provider = require_string(registry_row.get("id"), "platform registry id")
        pi_provider = registry_row.get("pi_id")
        if not isinstance(pi_provider, str) or pi_provider not in providers:
            continue
        for api, models in providers[pi_provider].items():
            backend = PI_API_TO_RUNTIME_BACKEND.get(api)
            if backend is None:
                continue
            for model_id, model in models.items():
                candidates.append((hyper_provider, model_id, backend, registry_row, model))

    for hyper_provider, model_id, backend, registry_row, model in sorted(
        candidates, key=lambda item: (item[0], item[1], item[2])
    ):
        key = (hyper_provider, model_id)
        if key in existing:
            continue
        raw_name = model.get("name")
        if not isinstance(raw_name, str) or not raw_name.strip():
            raise SyncError(f"{hyper_provider}/{model_id}.name must be a non-blank string")
        name = raw_name.strip()
        raw_base_url = model.get("baseUrl")
        if hyper_provider == "azure-openai-responses" and raw_base_url == "":
            # Pi resolves Azure from AZURE_OPENAI_BASE_URL/resource name at
            # runtime; its generated catalog intentionally stores no base.
            base_url = None
        else:
            base_url = require_string(raw_base_url, f"{hyper_provider}/{model_id}.baseUrl")
            if hyper_provider == "mistral" and base_url.rstrip("/") == "https://api.mistral.ai":
                # Pi's Mistral SDK serverURL is the root; the SDK appends /v1/chat/completions.
                # Hyper's sampler joins base + route directly, so store the versioned base.
                base_url = "https://api.mistral.ai/v1"
            if hyper_provider == "google-vertex" and "{location}" in base_url:
                base_url = base_url.replace("{location}", "{GOOGLE_CLOUD_LOCATION}")
        row: dict[str, Any] = {
            "api_backend": backend,
            "context_window": require_positive_int(
                model.get("contextWindow"), f"{hyper_provider}/{model_id}.contextWindow"
            ),
            "description": f"Official Pi catalog ({hyper_provider})",
            "max_completion_tokens": require_positive_int(
                model.get("maxTokens"), f"{hyper_provider}/{model_id}.maxTokens"
            ),
            "model": model_id,
            "name": name,
            "platform": hyper_provider,
            "source": "earendil-works/pi packages/ai providers/data",
            "supported_in_api": True,
            "supports_reasoning_effort": bool(model.get("reasoning")),
        }
        if base_url is not None and base_url != registry_row.get("default_base_url"):
            row["base_url_override"] = base_url
        output_rows.append(row)
        existing.add(key)

    return {
        "version": 3,
        "source": catalog.get("source"),
        "models": output_rows,
    }


def enrich_runtime_catalog(
    catalog: dict[str, Any],
    snapshot: dict[str, Any],
    policy: dict[str, Any],
    registry: dict[str, Any],
) -> dict[str, Any]:
    if catalog.get("version") not in (2, 3) or not isinstance(catalog.get("models"), list):
        raise SyncError("platform_catalog.json must use catalog schema v2 or v3")
    id_map = require_object(policy.get("provider_id_map"), "exclusion policy.provider_id_map")
    hyper_to_pi = {hyper: pi for pi, hyper in id_map.items()}
    registry_rows = require_object(
        {row["id"]: row for row in registry.get("providers", []) if isinstance(row, dict) and isinstance(row.get("id"), str)},
        "platform registry rows",
    )
    providers = snapshot["providers"]
    output_rows: list[dict[str, Any]] = []
    for index, original in enumerate(catalog["models"]):
        row = dict(require_object(original, f"platform_catalog.models[{index}]"))
        hyper_provider = require_string(row.get("platform"), f"platform_catalog.models[{index}].platform")
        model_id = require_string(row.get("model"), f"platform_catalog.models[{index}].model")
        backend = require_string(row.get("api_backend"), f"platform_catalog.models[{index}].api_backend")
        registry_row = registry_rows.get(hyper_provider)
        if registry_row is None:
            raise SyncError(f"runtime catalog provider {hyper_provider} is absent from provider registry")
        pi_provider = hyper_to_pi.get(hyper_provider, hyper_provider)
        pi_model = find_pi_model(pi_provider, model_id, backend, providers)
        pi_base_url = (
            pi_model.get("baseUrl")
            if pi_model is not None and isinstance(pi_model.get("baseUrl"), str)
            else None
        )
        base_url = pi_base_url or row.get("base_url_override") or registry_row.get("default_base_url")
        if not isinstance(base_url, str):
            raise SyncError(f"cannot resolve base URL for {hyper_provider}/{model_id}")
        if backend == "chat_completions":
            settings = resolve_chat_compat(pi_provider, model_id, base_url, pi_model)
        elif backend == "responses":
            settings = resolve_responses_compat(pi_provider, base_url, model_id, pi_model)
        elif backend == "messages":
            settings = resolve_messages_compat(pi_provider, model_id, pi_model)
        elif backend == "google_generate_content":
            settings = {
                "supports_strict_tool_sampling": model_id.startswith("gemini-3") or model_id.startswith("gemma-4") or model_id in ("gemini-flash-latest", "gemini-flash-lite-latest"),
                "thinking_level_map": {k: v for k, v in (pi_model.get("thinkingLevelMap") or {}).items()},
                "thinking_budgets": {},
            }
        elif backend == "bedrock_converse_stream":
            settings = resolve_bedrock_compat(pi_provider, model_id, pi_model)
        else:
            raise SyncError(f"unsupported runtime backend {backend!r} for {hyper_provider}/{model_id}")
        auth_policy = require_object(registry_row.get("credentials"), f"registry {hyper_provider}.credentials").get("auth")
        if backend == "bedrock_converse_stream":
            route_auth = "bearer"
        elif auth_policy == "x_goog_api_key" or backend == "google_generate_content":
            route_auth = "x_goog_api_key"
        elif auth_policy == "x_api_key" or (auth_policy == "protocol_default" and backend == "messages"):
            route_auth = "x_api_key"
        elif auth_policy == "api_key":
            route_auth = "api_key"
        elif auth_policy == "cf_aig_authorization":
            route_auth = "cf_aig_authorization"
        else:
            route_auth = "bearer"
        headers: dict[str, str] = {}
        if pi_model is not None:
            raw_headers = pi_model.get("headers", {})
            if not isinstance(raw_headers, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in raw_headers.items()):
                raise SyncError(f"{pi_provider}/{model_id}.headers must be a string map")
            headers.update(raw_headers)
        if backend == "messages":
            headers.setdefault("anthropic-version", "2023-06-01")
        if hyper_provider == "kimi-code":
            headers.setdefault("User-Agent", "KimiCLI/1.5")
        query_params: dict[str, str] = {}
        if hyper_provider == "azure-openai-responses":
            query_params["api-version"] = "v1"
        row["request_compat"] = {backend: settings}
        row["route"] = {
            "auth": route_auth,
            "headers": headers,
            "path": {
                "chat_completions": "chat/completions",
                "responses": "responses",
                "messages": "messages",
                "google_generate_content": "models/{model}:generateContent",
                "bedrock_converse_stream": "model/{model}/converse-stream",
            }[backend],
            "query_params": query_params,
        }
        output_rows.append(row)
    return {
        "version": 3,
        "source": catalog.get("source"),
        "models": output_rows,
    }


def model_ids(groups: dict[str, dict[str, Any]]) -> set[str]:
    return {model_id for models in groups.values() for model_id in models}


def build_parity(
    snapshot: dict[str, Any],
    lock: dict[str, Any],
    policy: dict[str, Any],
    registry: dict[str, Any],
    catalog: dict[str, Any],
) -> dict[str, Any]:
    providers = snapshot["providers"]
    id_map, exclusions = validate_exclusions(policy, lock, providers)
    registry_rows = registry.get("providers")
    if registry.get("version") != 2 or not isinstance(registry_rows, list):
        raise SyncError("platform_registry.json must use provider registry schema v2")
    registry_by_id: dict[str, dict[str, Any]] = {}
    for raw in registry_rows:
        row = require_object(raw, "platform registry row")
        provider_id = require_string(row.get("id"), "platform registry id")
        if provider_id in registry_by_id:
            raise SyncError(f"duplicate platform registry id {provider_id}")
        registry_by_id[provider_id] = row
    catalog_rows = catalog.get("models")
    if catalog.get("version") != 3 or not isinstance(catalog_rows, list):
        raise SyncError("platform_catalog.json must use catalog schema v3")
    catalog_ids: dict[str, set[str]] = {}
    for raw in catalog_rows:
        row = require_object(raw, "platform catalog row")
        provider_id = require_string(row.get("platform"), "platform catalog provider")
        model_id = require_string(row.get("model"), "platform catalog model")
        if model_id in catalog_ids.setdefault(provider_id, set()):
            raise SyncError(f"duplicate platform catalog model {provider_id}/{model_id}")
        catalog_ids[provider_id].add(model_id)

    rows: list[dict[str, Any]] = []
    supported_count = 0
    planned_count = 0
    mapped_hyper_ids: set[str] = set()
    drift: list[str] = []
    for pi_provider in sorted(providers):
        hyper_provider = id_map.get(pi_provider, pi_provider)
        mapped_hyper_ids.add(hyper_provider)
        exclusion = exclusions.get(pi_provider)
        registry_row = registry_by_id.get(hyper_provider)
        if exclusion is None:
            if registry_row is None or registry_row.get("status") != "active":
                raise SyncError(
                    f"Pi provider {pi_provider} is neither excluded nor mapped to an active Hyper provider "
                    f"({hyper_provider})"
                )
            status = "supported"
            supported_count += 1
        else:
            if registry_row is not None and registry_row.get("status") == "active":
                raise SyncError(
                    f"Pi provider {pi_provider} is still excluded but Hyper provider {hyper_provider} is active"
                )
            status = "planned"
            planned_count += 1

        pi_ids = model_ids(providers[pi_provider])
        hyper_ids = catalog_ids.get(hyper_provider, set())
        shared = pi_ids & hyper_ids
        missing = pi_ids - hyper_ids
        extra = hyper_ids - pi_ids
        if missing or extra:
            drift.append(pi_provider)
        rows.append(
            {
                "apis": sorted(providers[pi_provider]),
                "hyper_catalog_model_count": len(hyper_ids),
                "hyper_extra_model_count": len(extra),
                "hyper_provider": hyper_provider,
                "kind": "static",
                "missing_from_hyper_count": len(missing),
                "pi_model_count": len(pi_ids),
                "pi_provider": pi_provider,
                "reason_code": exclusion.get("reason_code") if exclusion else None,
                "shared_model_count": len(shared),
                "status": status,
                "target_wave": exclusion.get("target_wave") if exclusion else None,
            }
        )

    dynamic = lock["model_data"]["dynamic_providers"]
    for pi_provider in dynamic:
        hyper_provider = id_map.get(pi_provider, pi_provider)
        mapped_hyper_ids.add(hyper_provider)
        exclusion = exclusions.get(pi_provider)
        registry_row = registry_by_id.get(hyper_provider)
        if exclusion is None:
            if registry_row is None or registry_row.get("status") != "active":
                raise SyncError(
                    f"dynamic Pi provider {pi_provider} is neither excluded nor mapped to an active Hyper provider"
                )
            status = "supported"
            supported_count += 1
        else:
            if registry_row is not None and registry_row.get("status") == "active":
                raise SyncError(f"dynamic provider {pi_provider} is excluded but active in Hyper")
            status = "planned"
            planned_count += 1
        rows.append(
            {
                "apis": list(DYNAMIC_PROVIDER_APIS[pi_provider]),
                "hyper_catalog_model_count": len(catalog_ids.get(hyper_provider, set())),
                "hyper_extra_model_count": len(catalog_ids.get(hyper_provider, set())),
                "hyper_provider": hyper_provider,
                "kind": "dynamic",
                "missing_from_hyper_count": 0,
                "pi_model_count": 0,
                "pi_provider": pi_provider,
                "reason_code": exclusion.get("reason_code") if exclusion else None,
                "shared_model_count": 0,
                "status": status,
                "target_wave": exclusion.get("target_wave") if exclusion else None,
            }
        )

    if set(exclusions) != {row["pi_provider"] for row in rows if row["status"] == "planned"}:
        raise SyncError("exclusion policy does not exactly match planned parity rows")
    rows.sort(key=lambda row: row["pi_provider"])
    return {
        "schema_version": PARITY_SCHEMA_VERSION,
        "source": snapshot["source"],
        "summary": {
            "api_model_counts": lock["model_data"]["api_model_counts"],
            "dynamic_provider_count": len(dynamic),
            "hyper_only_registry_providers": sorted(set(registry_by_id) - mapped_hyper_ids),
            "model_drift_providers": sorted(drift),
            "pi_model_count": lock["model_data"]["static_model_count"],
            "planned_provider_count": planned_count,
            "static_provider_count": len(providers),
            "supported_provider_count": supported_count,
            "total_provider_count": len(providers) + len(dynamic),
        },
        "providers": rows,
    }


def verify_pi_source(pi_root: Path, lock: dict[str, Any]) -> None:
    root = pi_root.resolve()
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise SyncError(f"cannot resolve Git commit for --pi-root {root}: {error}") from error
    actual_commit = result.stdout.strip()
    if actual_commit != lock["pi_commit"]:
        raise SyncError(f"Pi source commit mismatch: expected {lock['pi_commit']}, got {actual_commit}")
    for relative, expected_sha256 in lock["source_files"].items():
        path = root / relative
        try:
            data = path.read_bytes()
        except OSError as error:
            raise SyncError(f"cannot read locked Pi source file {path}: {error}") from error
        actual_sha256 = digest(data, "sha256")
        if actual_sha256 != expected_sha256:
            raise SyncError(f"Pi source file digest mismatch for {relative}: expected {expected_sha256}, got {actual_sha256}")


def write_atomic(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def verify_or_write(path: Path, data: bytes, write: bool) -> None:
    if write:
        write_atomic(path, data)
        return
    try:
        existing = path.read_bytes()
    except OSError as error:
        raise SyncError(f"cannot read generated output {path}: {error}") from error
    if existing != data:
        raise SyncError(f"generated output is stale: {path}; rerun with --archive <tgz> --write")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--archive", type=Path, help="digest-locked @earendil-works/pi-ai .tgz")
    source.add_argument("--download", action="store_true", help="explicitly download the locked npm tarball")
    parser.add_argument("--write", action="store_true", help="rewrite snapshot and parity outputs")
    parser.add_argument("--pi-root", type=Path, help="optionally verify a local Pi checkout and locked source files")
    args = parser.parse_args(argv)

    lock = require_object(read_json(LOCK_PATH), "provider lock")
    validate_lock(lock)
    if args.pi_root is not None:
        verify_pi_source(args.pi_root, lock)

    archive_data = acquire_archive(lock, args.archive, args.download)
    if args.write and archive_data is None:
        raise SyncError("--write requires --archive or explicit --download")
    if archive_data is None:
        snapshot = validate_snapshot(read_json(SNAPSHOT_PATH), lock)
    else:
        snapshot = snapshot_from_archive(archive_data, lock)

    policy = require_object(read_json(EXCLUSIONS_PATH), "exclusion policy")
    registry = require_object(read_json(REGISTRY_PATH), "platform registry")
    catalog = require_object(read_json(CATALOG_PATH), "platform catalog")
    catalog = materialize_missing_active_models(catalog, snapshot, registry)
    catalog = enrich_runtime_catalog(catalog, snapshot, policy, registry)
    parity = build_parity(snapshot, lock, policy, registry, catalog)
    snapshot_bytes = canonical_json(snapshot)
    catalog_bytes = canonical_json(catalog)
    parity_bytes = canonical_json(parity)

    verify_or_write(SNAPSHOT_PATH, snapshot_bytes, args.write)
    verify_or_write(CATALOG_PATH, catalog_bytes, args.write)
    verify_or_write(PARITY_PATH, parity_bytes, args.write)

    snapshot_sha256 = digest(snapshot_bytes, "sha256")
    catalog_sha256 = digest(catalog_bytes, "sha256")
    parity_sha256 = digest(parity_bytes, "sha256")
    locked_outputs = lock["outputs"]
    for field, actual in (
        ("snapshot_sha256", snapshot_sha256),
        ("runtime_catalog_sha256", catalog_sha256),
        ("parity_report_sha256", parity_sha256),
    ):
        expected = locked_outputs[field]
        if expected and expected != actual:
            raise SyncError(f"locked output digest mismatch for {field}: expected {expected}, got {actual}")
        if not expected and not args.write:
            raise SyncError(f"lock.outputs.{field} is blank; set it to {actual}")

    action = "wrote" if args.write else "verified"
    print(
        f"{action} Pi provider snapshot: {len(snapshot['providers'])} static providers, "
        f"{lock['model_data']['static_model_count']} models"
    )
    print(f"snapshot_sha256={snapshot_sha256}")
    print(f"runtime_catalog_sha256={catalog_sha256}")
    print(f"parity_report_sha256={parity_sha256}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SyncError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
