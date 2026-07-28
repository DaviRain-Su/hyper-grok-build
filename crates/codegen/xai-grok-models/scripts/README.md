# Pi provider synchronization

`sync_pi_providers.py` converts the published, generated provider shards from
`@earendil-works/pi-ai` into Hyper's normalized snapshot, runtime catalog, and
parity report. For active Pi-backed registry providers it deterministically
appends missing models that use Hyper's implemented static wire adapters.
Dynamic providers (currently Radius over `pi-messages`) are audited through a
separate, total protocol declaration because they have no generated model
shard. Normal Cargo builds do not invoke the script.

The inputs are pinned in `../pi_provider_lock.json` by:

- Pi Git commit and selected source-file SHA-256 digests;
- npm package version, integrity, SHA-1, and SHA-512;
- Pi model-data manifest, structure hash, provider/model/API counts; and
- generated snapshot/report SHA-256 digests.

The script reads tar members in memory and never extracts untrusted paths.
Network access is opt-in only.

```bash
# Offline integrity/parity check using checked-in data.
python3 crates/codegen/xai-grok-models/scripts/sync_pi_providers.py

# Rebuild in memory from a previously downloaded, digest-locked artifact.
python3 crates/codegen/xai-grok-models/scripts/sync_pi_providers.py \
  --archive target/pi-ai-0.82.1.tgz

# Explicitly download the locked artifact and rewrite generated files.
python3 crates/codegen/xai-grok-models/scripts/sync_pi_providers.py \
  --download --write
```

When intentionally advancing Pi, update the commit/package/manifest fields
first, regenerate, review `pi_provider_parity.json`, then copy the three printed
output digests into the lock. Every unsupported provider must have an explicit,
sorted entry in `pi_provider_exclusions.json`; otherwise parity generation
fails.
