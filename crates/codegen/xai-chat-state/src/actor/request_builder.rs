//! ConversationRequest assembly — image compaction, pruning, repair, memory injection.

use xai_grok_sampling_types::{
    ContentPart, ConversationItem, ConversationRequest, ToolSpec, TraceContext,
};

use super::ChatStateActor;
use crate::events::ChatStateEvent;
use crate::types::PruningConfig;

/// Placeholder inserted when a tool result is hard-cleared.
///
/// `pub(super)` so that `mutations.rs` can use the same string when it
/// hard-clears tool results in the retained in-memory conversation.
pub(super) const HARD_CLEAR_PLACEHOLDER: &str = "[Tool result omitted — too old]";

/// Separator inserted between head and tail in soft-trimmed results.
const SOFT_TRIM_SEPARATOR: &str = "\n\n[…trimmed…]\n\n";

impl ChatStateActor {
    /// Build a `ConversationRequest` from the current actor state.
    ///
    /// 1. Evict oldest inline images when the inline-image bytes near 50 MB
    /// 2. Prune old tool results if over 50% context utilization
    /// 3. Optionally persist the memory reminder into actor state
    /// 4. Inject memory reminder into the request clone (if needed)
    /// 5. Assemble and return the `ConversationRequest`
    ///
    /// # Repair invariant
    ///
    /// The `BuildConversationRequest` command handler calls
    /// `ensure_conversation_integrity()` on the actor's own conversation
    /// **before** this function runs. The clone therefore starts from an
    /// already-repaired state, so there is no need to run
    /// `dedup_duplicate_tool_results` / `repair_dangling_tool_calls` on the
    /// clone — those would be O(n) no-ops.
    pub(super) fn build_conversation_request(
        &mut self,
        tool_definitions: Vec<ToolSpec>,
        memory_reminder: Option<String>,
        persist_memory_reminder: bool,
        trace: Option<Box<dyn TraceContext>>,
        conv_id: String,
        req_id: String,
    ) -> ConversationRequest {
        let needs_prune = should_prune(
            self.state.total_tokens,
            self.state.sampling_config.context_window,
        );
        let mut memory_reminder = memory_reminder;
        if let Some(reminder) = memory_reminder.as_deref()
            && persist_memory_reminder
        {
            // A live in-place inject can prepend a `System` item, shifting indices
            // under an active capture; snapshot + rebase like the other mutators.
            self.snapshot_turn_slice();
            let injected = inject_memory_reminder(&mut self.state.conversation, reminder);
            if injected {
                self.persistence.replace_history(&self.state.conversation);
                memory_reminder = None;
            }
            self.rebase_turn_capture_offset();
        }
        // Measure the exact serialized body and evict only once it approaches
        // the 50 MB ceiling. `conversation_body_bytes` is wire-accurate yet
        // cheap — it skips the multi-MB base64 escape scan (see its docs) — so
        // it runs inline on every turn with no blocking-thread offload.
        // Eviction rewrites earlier turns and busts the KV-cache prefix, so we
        // only pay it when the body is actually near the limit (the original
        // behavior — evicting every turn — caused chronic cache misses).
        let body_bytes = conversation_body_bytes(&self.state.conversation);
        let inline_images = inline_image_count(&self.state.conversation);
        let needs_image_compaction = body_bytes >= IMAGE_COMPACT_TRIGGER_BYTES;
        let needs_mutation = needs_prune || memory_reminder.is_some() || needs_image_compaction;

        // Only allocate the mutable working copy when a mutation path is taken.
        let mut eviction: Option<ImageEvictionOutcome> = None;
        let items = if needs_mutation {
            let mut items = self.state.conversation.clone();

            // Step 1: When the body nears the 50 MB ceiling, evict oldest
            // images down to the low-water mark (not just under the trigger).
            // Reclaiming a batch frees headroom for many subsequent image
            // turns, so the prefix is rewritten once and then stays cache-warm
            // — instead of re-triggering and re-busting the cache every turn.
            if needs_image_compaction {
                eviction = Some(compact_images_to_byte_budget(
                    &mut items,
                    body_bytes,
                    IMAGE_COMPACT_RECLAIM_TARGET_BYTES,
                ));
            }

            // Step 2: Prune old tool results if context is > 50% utilized
            if needs_prune {
                prune_conversation(&mut items, &self.pruning_config);
            }

            // Step 3: Inject memory reminder into the system message
            if let Some(reminder) = memory_reminder {
                inject_memory_reminder(&mut items, &reminder);
            }

            items
        } else {
            // Hot path: no pruning, no memory reminder, no old images —
            // clone directly into the request without any intermediate mutation passes.
            self.state.conversation.clone()
        };

        // Per-turn image-budget record for local verification. Emitted on the
        // ChatState event channel (chat-state can't reach the shell's unified
        // log directly); the session consumer writes it to the local log file.
        // Only on image-bearing turns to avoid noise.
        if inline_images > 0 {
            self.send_event(ChatStateEvent::ImageBudget {
                body_bytes,
                trigger_bytes: IMAGE_COMPACT_TRIGGER_BYTES,
                reclaim_target_bytes: IMAGE_COMPACT_RECLAIM_TARGET_BYTES,
                inline_images,
                needs_image_compaction,
                evicted: eviction.as_ref().map_or(0, |o| o.evicted),
                body_bytes_after: eviction.as_ref().map_or(body_bytes, |o| o.body_bytes_after),
            });
        }

        // Step 4: Assemble request
        ConversationRequest {
            items,
            tools: tool_definitions,
            hosted_tools: vec![],
            tool_choice: None,
            model: Some(self.state.sampling_config.model.clone()),
            temperature: self.state.sampling_config.temperature,
            max_output_tokens: self.state.sampling_config.max_completion_tokens,
            top_p: self.state.sampling_config.top_p,
            x_grok_conv_id: Some(conv_id),
            x_grok_req_id: Some(req_id),
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace,
            reasoning_effort: self.state.sampling_config.reasoning_effort,
            json_schema: None,
        }
    }
}

// ============================================================================
// Pruning (standalone functions, no actor state needed)
// ============================================================================

/// Check whether pruning should run based on context utilization.
///
/// Returns `true` when `total_tokens` exceeds 50% of `context_window`.
pub(crate) fn should_prune(total_tokens: u64, context_window: std::num::NonZeroU64) -> bool {
    total_tokens > context_window.get() / 2
}

/// Prune old, large tool results from the conversation in place.
///
/// Turn age is estimated by walking backward through the conversation and
/// counting `User` items to determine which "turn" each tool result belongs to.
pub(crate) fn prune_conversation(conversation: &mut [ConversationItem], config: &PruningConfig) {
    if !config.enabled {
        return;
    }

    let mut turn_from_end: usize = 0;
    let mut seen_first_user = false;

    for i in (0..conversation.len()).rev() {
        if matches!(&conversation[i], ConversationItem::User(_)) {
            if seen_first_user {
                turn_from_end += 1;
            }
            seen_first_user = true;
            continue;
        }

        let ConversationItem::ToolResult(tool_result) = &mut conversation[i] else {
            continue;
        };

        // Never prune recent turns.
        if turn_from_end < config.keep_last_n_turns {
            continue;
        }

        // Hard clear: very old tool results → replace entirely.
        if turn_from_end >= config.hard_clear_age_turns {
            if tool_result.content.as_ref() != HARD_CLEAR_PLACEHOLDER {
                tool_result.content = std::sync::Arc::<str>::from(HARD_CLEAR_PLACEHOLDER);
            }
            continue;
        }

        // Soft trim: large tool results → keep head + tail.
        let content_len = tool_result.content.chars().count();
        if content_len > config.soft_trim_threshold {
            let head = safe_char_slice(&tool_result.content, 0, config.soft_trim_head);
            let tail = safe_char_slice_tail(&tool_result.content, config.soft_trim_tail);
            tool_result.content =
                std::sync::Arc::<str>::from(format!("{head}{SOFT_TRIM_SEPARATOR}{tail}"));
        }
    }
}

// ============================================================================
// Image size-gated compaction (request-copy only)
// ============================================================================

/// Replaces an inline image evicted to keep the request body under the proxy's
/// 50 MB limit. Phrased so the model treats the image as gone rather than
/// describing it from memory — a silently-stripped image otherwise induces
/// confident hallucination of its contents.
const IMAGE_COMPACT_PLACEHOLDER: &str = "[An earlier image was removed to keep the request within its size limit and is no longer visible. Do not describe or reason about its contents from memory; ask the user to re-share it if you need to see it again.]";

/// Hard request-body ceiling enforced by the inference proxy
/// (nginx `proxy-body-size`). Bodies larger than this are rejected with HTTP
/// 413 — or a connection reset before the response is written. Inline image
/// `data:` URLs (base64) are the dominant term in this size.
const MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;

/// Evict old images once the serialized body reaches this size.
///
/// We gate on the exact body (see [`conversation_body_bytes`]) — system prompt,
/// all message text, tool results, and image `data:` URLs are all counted
/// precisely. This sits 3 MB below [`MAX_REQUEST_BYTES`] as headroom for the
/// only parts of the wire request the body measurement does **not** include:
/// - **tool definitions** — sent alongside the conversation but not part of it
///   (tool JSON schemas + MCP tools); this is the bulk of the gap.
/// - the request envelope and sampling params.
/// - the small delta between our internal `ContentPart` JSON and the public-API
///   wire format (the dominant base64 image bytes are identical in both).
///
/// The uncounted remainder is only sub-MB to low-MB in practice, so 3 MB covers
/// it without needlessly sacrificing image capacity. The sampler's reactive 413
/// image-strip is the final backstop if this is ever under-estimated.
///
/// Below this threshold every image stays in place so the KV-cache prefix is
/// byte-stable across turns; eviction rewrites earlier turns and busts the
/// prefix cache, so we only pay that cost when a 413 is actually near.
pub(crate) const IMAGE_COMPACT_TRIGGER_BYTES: usize = MAX_REQUEST_BYTES - 3 * 1024 * 1024;

/// Low-water mark that eviction reclaims down to once it fires (hysteresis).
///
/// Eviction is **gated** at [`IMAGE_COMPACT_TRIGGER_BYTES`] but **reclaims** to
/// this strictly lower mark. Evicting only enough to clear the trigger means
/// the next image-bearing turn re-crosses it and evicts again — rewriting the
/// prefix and busting the KV cache on essentially every turn once the body sits
/// at the ceiling. Dropping to half the hard limit instead frees ~25 MB of
/// headroom, so the prefix is rewritten once and then stays stable (cache-warm)
/// across many turns until the headroom is consumed again. The oldest images
/// (least useful) are sacrificed in a batch rather than one-per-turn — a
/// high-water trigger paired with a lower reclaim mark (classic hysteresis).
pub(crate) const IMAGE_COMPACT_RECLAIM_TARGET_BYTES: usize = MAX_REQUEST_BYTES / 2;

// Hysteresis invariant: eviction is gated at the trigger but reclaims to a
// strictly lower mark, so one batch eviction buys many cache-warm turns rather
// than re-triggering (and re-busting the prompt cache) every turn at the
// ceiling. Enforced at compile time so the two constants can't drift together.
const _: () = assert!(IMAGE_COMPACT_RECLAIM_TARGET_BYTES < IMAGE_COMPACT_TRIGGER_BYTES);

/// An [`std::io::Write`] sink that counts bytes instead of storing them. Lets
/// us measure a `serde_json` encoding's length without allocating the full
/// (potentially tens-of-MB) output buffer.
#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exact JSON-serialized byte length of any value, measured through a
/// [`ByteCounter`] so no encoded buffer is allocated. JSON quoting and string
/// escaping are captured precisely (not estimated from field lengths).
fn serialized_json_bytes<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = ByteCounter::default();
    if let Err(err) = serde_json::to_writer(&mut counter, value) {
        // Serializing in-memory state to a byte sink is infallible in
        // practice; if it ever fails, fall back to the bytes counted so far
        // (a lower bound) rather than forcing a needless compaction.
        tracing::warn!(%err, "failed to measure serialized size");
    }
    counter.0
}

/// Serialized JSON frame of one image content part with an empty URL —
/// `{"type":"image","url":""}`. The real payload adds exactly `url.len()` on
/// top: an inline base64 `data:` URL contains no JSON-escaped characters, so
/// its encoded length equals its raw length. Identical in our internal JSON and
/// on the public-API wire (the base64 bytes are the same in both).
const IMAGE_PART_FRAME_BYTES: usize = r#"{"type":"image","url":""}"#.len();

/// Exact serialized size of a single inline image part (frame + raw URL bytes).
fn image_part_bytes(url: &str) -> usize {
    IMAGE_PART_FRAME_BYTES + url.len()
}

/// Count of inline images in the conversation — for observability only.
fn inline_image_count(conversation: &[ConversationItem]) -> usize {
    conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::User(u) => Some(u),
            _ => None,
        })
        .flat_map(|u| u.content.iter())
        .filter(|p| matches!(p, ContentPart::Image { .. }))
        .count()
}

/// Outcome of [`compact_images_to_byte_budget`], surfaced for logging and
/// local verification.
pub(crate) struct ImageEvictionOutcome {
    /// Number of inline images replaced with the placeholder.
    pub evicted: usize,
    /// Estimated serialized body size after eviction (`current_bytes` minus the
    /// net bytes freed) — at or below `target_bytes` once enough images go.
    pub body_bytes_after: usize,
}

/// Exact serialized size of the conversation body — the figure the inference
/// proxy weighs against its 50 MB limit — computed **without** scanning the
/// multi-MB base64 image payloads.
///
/// `serde_json` escape-scans every byte of every string, so encoding the real
/// conversation would walk tens of MB of base64 on every turn. Instead we
/// serialize a copy with image URLs blanked (cheap: only the small non-image
/// content — system prompt, message text, tool results — is scanned, and it is
/// measured *exactly*, escaping included) and add back each URL's raw length.
/// Because base64 never escapes, that length is its exact serialized
/// contribution, so the result is byte-for-byte the true body size.
///
/// The blanking copy is cheap: image data lives behind `Arc<str>`, so cloning
/// only bumps refcounts and the blanked clone drops them without copying bytes.
fn conversation_body_bytes(conversation: &[ConversationItem]) -> usize {
    let mut blanked = conversation.to_vec();
    let mut image_url_bytes = 0usize;
    for item in &mut blanked {
        if let ConversationItem::User(user) = item {
            for part in &mut user.content {
                if let ContentPart::Image { url } = part {
                    image_url_bytes += url.len();
                    *url = std::sync::Arc::<str>::from("");
                }
            }
        }
    }
    serialized_json_bytes(&blanked) + image_url_bytes
}

/// Replace the oldest inline images with [`IMAGE_COMPACT_PLACEHOLDER`] until
/// the serialized request body drops back to `target_bytes`, keeping the
/// newest images. `current_bytes` is the already-measured whole-body size (see
/// [`conversation_body_bytes`]); each eviction drops `running` by the image
/// part's exact serialized size minus the placeholder that replaces it, so it
/// tracks the true body byte-for-byte as images are removed.
///
/// Operates on a mutable slice — intended for the request *copy* so the stored
/// conversation is never modified.
///
/// ## Cache behavior
///
/// Eviction is **oldest-first**, which is sticky by construction: because we
/// always retain the newest images, an image only transitions image →
/// placeholder as *newer/larger* payloads push the body past the limit, never
/// placeholder → image within a stable prefix. (Token compaction removes old
/// turns wholesale and can free room to restore a previously-evicted image,
/// but that already rewrites the prefix and invalidates the server-side prompt
/// cache, so the restore is free.)
///
/// The caller gates eviction at [`IMAGE_COMPACT_TRIGGER_BYTES`] but passes the
/// lower [`IMAGE_COMPACT_RECLAIM_TARGET_BYTES`] as `target_bytes`, so one
/// eviction reclaims a batch of the oldest images and frees headroom for many
/// later image turns. This turns "rewrite the prefix on essentially every turn
/// once at the ceiling" into one larger, rare rewrite followed by a long
/// cache-warm stretch — the prefix-cache cost of dropping the oldest (least
/// useful) image is paid infrequently instead of per turn.
///
/// This replaces the previous policy — strip every image older than the most
/// recent user turn on *every* request — which (a) busted the prompt-cache
/// prefix on the turn after any image, and (b) dropped images the model still
/// needed one turn later, causing it to hallucinate their contents.
pub(crate) fn compact_images_to_byte_budget(
    conversation: &mut [ConversationItem],
    current_bytes: usize,
    target_bytes: usize,
) -> ImageEvictionOutcome {
    if current_bytes <= target_bytes {
        return ImageEvictionOutcome {
            evicted: 0,
            body_bytes_after: current_bytes,
        };
    }

    // The text part each evicted image is replaced with. Measured once: every
    // eviction shrinks the body by the image part's bytes and grows it back by
    // this placeholder's bytes, so the net saving is `image - placeholder`.
    let placeholder = ContentPart::Text {
        text: std::sync::Arc::<str>::from(IMAGE_COMPACT_PLACEHOLDER),
    };
    let placeholder_bytes = serialized_json_bytes(&placeholder);

    // (item_idx, part_idx, exact serialized image-part bytes) for every inline
    // image, oldest-first.
    let mut images: Vec<(usize, usize, usize)> = Vec::new();
    for (i, item) in conversation.iter().enumerate() {
        if let ConversationItem::User(user) = item {
            for (j, part) in user.content.iter().enumerate() {
                if let ContentPart::Image { url } = part {
                    images.push((i, j, image_part_bytes(url)));
                }
            }
        }
    }

    // Evict oldest-first until the body fits again, keeping the newest images.
    let mut running = current_bytes;
    let mut evicted = 0usize;
    for &(i, j, image_bytes) in &images {
        if running <= target_bytes {
            break;
        }
        if let ConversationItem::User(user) = &mut conversation[i]
            && let Some(part) = user.content.get_mut(j)
        {
            *part = placeholder.clone();
            // Net body saving: the image part leaves, the placeholder takes its
            // slot. Everything else (siblings, commas, brackets) is untouched,
            // so this is the exact change in the serialized body size.
            running = running.saturating_sub(image_bytes.saturating_sub(placeholder_bytes));
            evicted += 1;
        }
    }

    ImageEvictionOutcome {
        evicted,
        body_bytes_after: running,
    }
}

// ============================================================================
// Memory reminder injection
// ============================================================================

use crate::types::MEMORY_CONTEXT_OPEN_TAG;

/// Upsert a memory reminder into the conversation's system message.
///
/// If the first item is a `System` message, any previously injected memory
/// reminder section is replaced in-place; otherwise the reminder is appended.
/// If no system message exists, a new `System` item is prepended.
///
/// Returns `true` when the conversation was changed.
pub(super) fn inject_memory_reminder(items: &mut Vec<ConversationItem>, reminder: &str) -> bool {
    let reminder = reminder.trim();
    if reminder.is_empty() {
        return false;
    }

    if let Some(ConversationItem::System(sys)) = items.first_mut() {
        upsert_memory_reminder_text(&mut sys.content, reminder)
    } else {
        items.insert(0, ConversationItem::system(reminder));
        true
    }
}

fn upsert_memory_reminder_text(system_prompt: &mut std::sync::Arc<str>, reminder: &str) -> bool {
    let existing_start = system_prompt
        .find(MEMORY_CONTEXT_OPEN_TAG)
        .map(|idx| system_prompt[..idx].trim_end_matches('\n').len());

    let updated: String = if let Some(prefix_len) = existing_start {
        let prefix = system_prompt[..prefix_len].trim_end_matches('\n');
        if prefix.is_empty() {
            reminder.to_string()
        } else {
            format!("{prefix}\n\n{reminder}")
        }
    } else if system_prompt.trim_end() == reminder {
        system_prompt.as_ref().to_owned()
    } else if system_prompt.is_empty() {
        reminder.to_string()
    } else {
        format!("{}\n\n{reminder}", system_prompt.trim_end_matches('\n'))
    };

    if system_prompt.as_ref() == updated.as_str() {
        false
    } else {
        *system_prompt = std::sync::Arc::<str>::from(updated);
        true
    }
}

// ============================================================================
// String helpers
// ============================================================================

fn safe_char_slice(s: &str, start: usize, count: usize) -> String {
    s.chars().skip(start).take(count).collect()
}

fn safe_char_slice_tail(s: &str, count: usize) -> String {
    let total = s.chars().count();
    if count >= total {
        return s.to_string();
    }
    s.chars().skip(total - count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_prune_gating() {
        use std::num::NonZeroU64;
        let cw = NonZeroU64::new(10000).unwrap();
        assert!(!should_prune(1000, cw)); // 10%
        assert!(should_prune(6000, cw)); // 60%
        assert!(!should_prune(5000, cw)); // 50% exact (> not >=)
    }

    #[test]
    fn prune_disabled_is_noop() {
        let mut conv = vec![ConversationItem::tool_result("c1", "x".repeat(10_000))];
        let config = PruningConfig {
            enabled: false,
            ..Default::default()
        };
        prune_conversation(&mut conv, &config);
        if let ConversationItem::ToolResult(ref tr) = conv[0] {
            assert_eq!(tr.content.len(), 10_000);
        }
    }

    #[test]
    fn inject_memory_into_existing_system() {
        let mut items = vec![
            ConversationItem::system("You are helpful."),
            ConversationItem::user("hi"),
        ];
        inject_memory_reminder(&mut items, "Remember: user likes rust");
        if let ConversationItem::System(ref sys) = items[0] {
            assert!(sys.content.contains("Remember: user likes rust"));
            assert!(sys.content.starts_with("You are helpful."));
        }
        assert_eq!(items.len(), 2); // no new item added
    }

    #[test]
    fn inject_memory_prepends_when_no_system() {
        let mut items = vec![ConversationItem::user("hi")];
        inject_memory_reminder(&mut items, "Remember: user likes rust");
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], ConversationItem::System(_)));
    }

    // -- image size-gated compaction tests --

    /// A user message with a small fixed inline image.
    fn user_with_image(text: &str) -> ConversationItem {
        let mut item = ConversationItem::user(text);
        item.add_image("data:image/png;base64,iVBORw0KGgo=");
        item
    }

    /// A user message carrying an inline image whose `data:` URL is exactly
    /// `url_bytes` long (must be >= the data-URL prefix length).
    fn user_with_image_of_bytes(text: &str, url_bytes: usize) -> ConversationItem {
        const PREFIX: &str = "data:image/png;base64,";
        let pad = url_bytes.saturating_sub(PREFIX.len());
        let mut item = ConversationItem::user(text);
        item.add_image(format!("{PREFIX}{}", "A".repeat(pad)));
        item
    }

    fn has_image(item: &ConversationItem) -> bool {
        matches!(
            item,
            ConversationItem::User(u)
                if u.content.iter().any(|p| matches!(p, ContentPart::Image { .. }))
        )
    }

    fn has_placeholder(item: &ConversationItem) -> bool {
        matches!(
                    item,
                    ConversationItem::User(u) if u.content.iter().any(|p| matches!(
                        p,
                        ContentPart::Text { text }
        if text.as_ref() == IMAGE_COMPACT_PLACEHOLDER
                    ))
                )
    }

    // Images are sized ~100 KB so the ~235 B placeholder that replaces an
    // evicted image is negligible: each eviction frees ~one image's bytes.
    const TEST_IMG_BYTES: usize = 100_000;

    #[test]
    fn no_eviction_when_at_or_below_target() {
        // Multiple old image turns are *retained* when the body already fits —
        // the key behavior change from the old "strip everything but newest".
        let mut conv = vec![
            ConversationItem::system("sys"),
            user_with_image_of_bytes("first", TEST_IMG_BYTES),
            ConversationItem::assistant("a"),
            user_with_image_of_bytes("second", TEST_IMG_BYTES),
            user_with_image_of_bytes("third", TEST_IMG_BYTES),
        ];
        // current < target: nothing to do.
        compact_images_to_byte_budget(&mut conv, 300_000, 400_000);
        assert_eq!(conv.iter().filter(|i| has_image(i)).count(), 3);
    }

    #[test]
    fn evicts_oldest_until_under_target() {
        let mut conv = vec![
            user_with_image_of_bytes("oldest", TEST_IMG_BYTES),
            user_with_image_of_bytes("middle", TEST_IMG_BYTES),
            user_with_image_of_bytes("newest", TEST_IMG_BYTES),
        ];
        // current 300k, target 250k: evicting the oldest (~100 KB) fits.
        compact_images_to_byte_budget(&mut conv, 300_000, 250_000);
        assert!(has_placeholder(&conv[0]), "oldest evicted");
        assert!(has_image(&conv[1]), "middle kept");
        assert!(has_image(&conv[2]), "newest kept");
    }

    #[test]
    fn evicts_more_oldest_for_lower_target() {
        let mut conv = vec![
            user_with_image_of_bytes("oldest", TEST_IMG_BYTES),
            user_with_image_of_bytes("middle", TEST_IMG_BYTES),
            user_with_image_of_bytes("newest", TEST_IMG_BYTES),
        ];
        // current 300k, target 150k: must drop the two oldest to fit.
        compact_images_to_byte_budget(&mut conv, 300_000, 150_000);
        assert!(has_placeholder(&conv[0]));
        assert!(has_placeholder(&conv[1]));
        assert!(has_image(&conv[2]), "newest kept");
    }

    #[test]
    fn eviction_reclaims_batch_to_low_water_mark() {
        // Mirror production: a body sitting just over the trigger, made of many
        // equal images, is reclaimed in one pass down to the low-water mark —
        // dropping a *batch* of the oldest, not just the one image needed to
        // clear the trigger. This is the hysteresis that keeps the prefix
        // cache-warm for the following turns.
        let img_bytes = 1_000_000usize; // ~1 MB url each
        let n = (IMAGE_COMPACT_TRIGGER_BYTES / img_bytes) + 2; // body just over trigger
        let mut conv: Vec<ConversationItem> = (0..n)
            .map(|i| user_with_image_of_bytes(&format!("i{i}"), img_bytes))
            .collect();
        let current = n * img_bytes;
        assert!(current > IMAGE_COMPACT_TRIGGER_BYTES);

        compact_images_to_byte_budget(&mut conv, current, IMAGE_COMPACT_RECLAIM_TARGET_BYTES);

        let kept = conv.iter().filter(|i| has_image(i)).count();
        let evicted = conv.iter().filter(|i| has_placeholder(i)).count();

        // Clearing only the trigger would evict ~3 images; reclaiming to the
        // low-water mark (~half the ceiling) must evict far more.
        assert!(
            evicted > n / 4,
            "expected batch eviction to the low-water mark, only {evicted}/{n} evicted"
        );
        // Oldest-first stops at the mark, so the most recent image survives.
        assert!(kept > 0);
        assert!(
            has_image(conv.last().unwrap()),
            "most recent image must be retained"
        );
    }

    #[test]
    fn evicts_all_when_target_below_one_image() {
        let mut conv = vec![
            user_with_image_of_bytes("a", TEST_IMG_BYTES),
            user_with_image_of_bytes("b", TEST_IMG_BYTES),
        ];
        compact_images_to_byte_budget(&mut conv, 200_000, 50_000);
        assert!(has_placeholder(&conv[0]));
        assert!(has_placeholder(&conv[1]));
    }

    #[test]
    fn eviction_keeps_newest_and_is_idempotent() {
        let mut conv = vec![
            user_with_image_of_bytes("i0", TEST_IMG_BYTES),
            user_with_image_of_bytes("i1", TEST_IMG_BYTES),
            user_with_image_of_bytes("i2", TEST_IMG_BYTES),
            user_with_image_of_bytes("i3", TEST_IMG_BYTES),
        ];
        // current 400k, target 250k: drop the two oldest, keep the newest two.
        compact_images_to_byte_budget(&mut conv, 400_000, 250_000);
        assert!(has_placeholder(&conv[0]) && has_placeholder(&conv[1]));
        assert!(has_image(&conv[2]) && has_image(&conv[3]));

        // Re-running with the now-smaller body is a no-op (sticky): the two
        // surviving images already fit.
        compact_images_to_byte_budget(&mut conv, 200_000, 250_000);
        assert!(has_placeholder(&conv[0]) && has_placeholder(&conv[1]));
        assert!(has_image(&conv[2]) && has_image(&conv[3]));
    }

    #[test]
    fn evicted_image_uses_honest_placeholder() {
        let mut conv = vec![user_with_image_of_bytes("x", TEST_IMG_BYTES)];
        compact_images_to_byte_budget(&mut conv, 100_000, 10);
        assert!(has_placeholder(&conv[0]));
    }

    // -- conversation_body_bytes tests --

    #[test]
    fn conversation_body_bytes_empty_is_json_array() {
        // serde encodes an empty slice as "[]" (2 bytes).
        assert_eq!(conversation_body_bytes(&[]), 2);
    }

    #[test]
    fn conversation_body_bytes_matches_serde_json_exactly() {
        // The blank-and-add-URLs measurement must equal a full serde_json
        // encode byte-for-byte — including non-image content and string
        // escaping. The `"` in the system text is escaped by serde; the
        // measurement must account for it.
        let conv = vec![
            ConversationItem::system("system \"quoted\" prompt"),
            user_with_image("look"),
            ConversationItem::assistant("a longer assistant reply with text"),
            ConversationItem::user("plain follow-up turn"),
        ];
        let expected = serde_json::to_vec(&conv).unwrap().len();
        assert_eq!(conversation_body_bytes(&conv), expected);
    }

    #[test]
    fn conversation_body_bytes_matches_serde_json_with_large_image() {
        // Exact even for a multi-KB base64 payload — the scan we deliberately
        // skip still lands on the same byte count.
        let conv = vec![user_with_image_of_bytes("big", 50_000)];
        let expected = serde_json::to_vec(&conv).unwrap().len();
        assert_eq!(conversation_body_bytes(&conv), expected);
    }

    #[test]
    fn conversation_body_bytes_small_image_is_below_trigger() {
        // A normal small inline image must not trip the 50 MB gate — the case
        // the cache-miss fix preserves.
        let conv = vec![
            user_with_image("old"),
            ConversationItem::assistant("reply"),
            ConversationItem::user("current"),
        ];
        assert!(conversation_body_bytes(&conv) < IMAGE_COMPACT_TRIGGER_BYTES);
    }

    #[test]
    fn conversation_body_bytes_large_image_reaches_trigger() {
        let conv = vec![user_with_image_of_bytes("big", IMAGE_COMPACT_TRIGGER_BYTES)];
        assert!(conversation_body_bytes(&conv) >= IMAGE_COMPACT_TRIGGER_BYTES);
    }

    // -- edge cases: exactness, boundaries, ordering --

    #[test]
    fn body_bytes_parity_multi_image_unicode_escaping() {
        // The gate is only as correct as this equality. Exercise multiple
        // images in one turn, multibyte unicode (passed through, not escaped),
        // and chars serde *does* escape (`"`, `\`, control).
        let mut turn = ConversationItem::user("two pics 🚀 with \"quotes\" and \\ slash");
        turn.add_image("data:image/png;base64,AAAA");
        turn.add_image("data:image/png;base64,BBBBBB");
        let conv = vec![
            ConversationItem::system("sys 日本語 \t control"),
            turn,
            ConversationItem::assistant("reply"),
            ConversationItem::user("plain follow-up"),
        ];
        assert_eq!(
            conversation_body_bytes(&conv),
            serde_json::to_vec(&conv).unwrap().len()
        );
    }

    #[test]
    fn no_eviction_when_exactly_at_target() {
        // The no-op guard is `current <= target`; pin the inclusive boundary.
        let mut conv = vec![user_with_image_of_bytes("a", TEST_IMG_BYTES)];
        compact_images_to_byte_budget(&mut conv, 250_000, 250_000);
        assert!(has_image(&conv[0]), "exactly at target must not evict");
    }

    #[test]
    fn terminates_when_placeholder_exceeds_image() {
        // Tiny images: each "saving" saturates to 0, but the loop must still
        // terminate and replace every image when the target is unreachable.
        let mut conv = vec![
            user_with_image_of_bytes("a", 40),
            user_with_image_of_bytes("b", 40),
        ];
        compact_images_to_byte_budget(&mut conv, 1_000, 10);
        assert!(has_placeholder(&conv[0]) && has_placeholder(&conv[1]));
    }

    #[test]
    fn evicts_oldest_image_parts_first() {
        // `has_image`/`has_placeholder` are per-item, so count actual image
        // parts to verify oldest-first ordering across parts within a turn.
        fn image_parts(conv: &[ConversationItem]) -> usize {
            conv.iter()
                .filter_map(|i| match i {
                    ConversationItem::User(u) => Some(u),
                    _ => None,
                })
                .flat_map(|u| u.content.iter())
                .filter(|p| matches!(p, ContentPart::Image { .. }))
                .count()
        }
        let mut newest = ConversationItem::user("newest turn");
        newest.add_image(format!(
            "data:image/png;base64,{}",
            "A".repeat(TEST_IMG_BYTES)
        ));
        newest.add_image(format!(
            "data:image/png;base64,{}",
            "B".repeat(TEST_IMG_BYTES)
        ));
        let mut conv = vec![user_with_image_of_bytes("oldest", TEST_IMG_BYTES), newest];
        assert_eq!(image_parts(&conv), 3);

        // ~300k body, reclaim to 150k: drop the two oldest, keep the newest.
        compact_images_to_byte_budget(&mut conv, 300_000, 150_000);
        assert_eq!(image_parts(&conv), 1, "newest image survives");
        assert!(has_placeholder(&conv[0]), "oldest turn evicted");
        assert!(has_image(&conv[1]), "newest turn keeps an image");
    }

    #[test]
    fn escaped_remote_url_is_a_lower_bound_only() {
        // base64 `data:` URLs are exact; a remote URL with a JSON-escaped char
        // under-counts by the escape bytes. Pin that documented bound so the
        // measurement can't silently drift past it.
        let mut item = ConversationItem::user("");
        item.add_image(r#"https://example.com/a"b"#);
        let conv = vec![item];
        assert!(conversation_body_bytes(&conv) <= serde_json::to_vec(&conv).unwrap().len());
    }
}
