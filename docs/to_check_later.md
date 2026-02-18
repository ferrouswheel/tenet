# Things to Check Later

This file collects gaps, inconsistencies, and open questions found while consolidating the docs.

## Inconsistencies

### 1. Group key distribution — plan vs. implementation

`group_invite_plan.md` (now `docs/groups.md`) describes the consent-based invite flow as the
**intended** design, but notes that the old code had `return false` for `group_key_distribution`
messages. The git log shows `src/web_client/handlers/group_invites.rs` as a new untracked file,
suggesting partial implementation.

**To check**: Is the full invite flow (GroupInvite → GroupInviteAccept → group_key_distribution
with consent check) actually implemented end-to-end in `sync.rs`? Or is `group_invites.rs` the
handler file only?

### 2. `StorageMessageHandler` — proposal vs. reality

`client_managed_storage.md` (now `docs/clients/howto.md`) is clearly marked as a design
proposal. As of this writing, `sync.rs` still contains the duplicated envelope-processing logic
described in the problem statement.

**To check**: Has any of this proposal been implemented? Does `src/message_handler.rs` exist?
If not, the howto.md should remain clearly labelled as a proposal/future direction.

### 3. Web binary name

`WEBUI_PLAN.md` consistently refers to the binary as `tenet-web`. The `README.md` also
references `tenet-web`. CLAUDE.md does not mention `tenet-web`. The `Cargo.toml` binary targets
should be the source of truth.

**To check**: Verify whether the binary is actually named `tenet-web` or something else in
`Cargo.toml`.

### 4. `POST /api/groups/:group_id/join`

`WEBUI_PLAN.md` listed `POST /api/groups/:group_id/join` (for joining when invited). The new
group invite flow uses `POST /api/group-invites/:id/accept` instead. The `/join` endpoint may
not exist or may do something different.

**To check**: Is `POST /api/groups/:group_id/join` still a valid endpoint or was it replaced
by the group-invites accept flow?

### 5. `POST /api/groups/:group_id/leave`

`WEBUI_PLAN.md` listed a leave endpoint. This is not covered in `group_invite_plan.md` and the
current state of this endpoint is unclear.

**To check**: Does `leave` work? Does it trigger key rotation (which is known to be deferred)?

### 6. Key rotation on member removal

`group_invite_plan.md` notes: "Key rotation on member removal: The current code has a TODO
comment; still deferred."

**To check**: Add a TODO comment in `src/web_client/handlers/groups.rs` if it doesn't already
exist, so this doesn't get lost.

### 7. Migration from JSON/JSONL

`WEBUI_PLAN.md` describes a one-time migration from `identity.json`, `peers.json`, etc. to
SQLite. It's unclear whether this migration code exists and whether it still matters.

**To check**: Does `src/storage.rs` or startup code contain the migration logic? Or was this
never implemented since development started fresh with SQLite?

### 8. Relay WebSocket endpoint (`/ws/{recipient_id}`)

`WEBUI_SUMMARY.md` lists `/ws/{recipient_id}` as a relay endpoint for real-time delivery. The
relay architecture doc doesn't clarify whether this is actually implemented or was a planned
feature.

**To check**: Does the relay in `src/relay.rs` actually implement WebSocket delivery, or does
it only support HTTP polling?

## Gaps

### 1. No doc for the CLI (`tenet` binary)

The `tenet` CLI (`cargo run --bin tenet`) supports `init`, `add-peer`, `send`, etc. but has no
dedicated documentation. `docs/clients/` should eventually include a `cli.md`.

### 2. No doc for the simulation TUI internals

The simulation TUI is powered by `ratatui`. There are no docs describing the TUI layout or how
to extend it.

### 3. Group name vs. group ID ambiguity

Both the groups.md and the code treat `group_id` as the display name. This is acknowledged as a
known limitation. There's no migration plan if we ever separate them.

### 4. Multi-device / multi-identity

There's no documentation for how multi-device or multi-identity scenarios should work. The
`src/identity.rs` module handles "multi-identity management" per CLAUDE.md, but this isn't
documented.

### 5. Attachment encryption

`WEBUI_PLAN.md` mentions "Attachments are encrypted alongside the message payload before
transmission." The actual encryption behavior for attachments (whether they're encrypted
in transit vs. stored in plaintext) is not documented.

### 6. Profile broadcast protocol

The profile update protocol (as a `tenet.profile` content type on `Public`/`Meta` payloads) is
described in `WEBUI_PLAN.md` but not in dedicated docs. Profiles are implemented but the
propagation mechanism isn't clearly documented anywhere.

### 7. `friend_group` UI status

WEBUI_SUMMARY.md marks "Friend Groups UI" as "Partial (protocol exists, basic filtering)". The
nature of "partial" isn't described — what works, what doesn't?
