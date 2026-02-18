# Things to Check Later

This file collects genuine gaps and open questions in the codebase. The inconsistencies that were
originally listed here have been investigated and resolved in the relevant docs.

## Resolved (for reference)

| Item | Finding |
|------|---------|
| Group invite flow implemented? | Yes — fully implemented end-to-end in `sync.rs` and `handlers/group_invites.rs` |
| `StorageMessageHandler` proposal vs. reality | Fully implemented and in use; web `sync.rs` migrated to `WebClientHandler` wrapping `StorageMessageHandler` (migration step 4 done) |
| Web binary name | `tenet-web` confirmed in `Cargo.toml` |
| `/api/groups/:id/join` endpoint | Does not exist; the old plan was superseded by the `group-invites` accept flow |
| `/api/groups/:id/leave` endpoint | Exists and works; removes self from `group_members` |
| Key rotation TODO comments | Already present in `handlers/groups.rs` for both `remove_group_member` and `leave_group` |
| JSON/JSONL migration code | Implemented in `src/storage.rs::migrate_from_json()`; tested |
| Relay WebSocket | Fully implemented — `ws_handler` in `src/relay.rs` pushes envelopes in real time |
| No doc for the CLI (`tenet` binary) | Resolved — `docs/clients/cli.md` now covers all commands |
| Multi-device / multi-identity undocumented | Resolved — `docs/clients/identity.md` documents the full system; known limitation (signing keys not portable) noted |
| Attachment encryption in transit | Resolved — plaintext in local SQLite; encrypted in transit as inline base64 inside the HPKE-encrypted message payload for Direct/Group; plaintext for Public |
| Friend Groups UI partial status | Resolved — create/send/accept-invite work; no UI for add-member, remove-member, or leave-group |
| `StorageMessageHandler` auto-accepts group invites | Resolved — auto-accept removed; `GroupInvite` is now stored as `"pending"` with a notification, matching the web client's UI-prompt flow. See `src/message_handler.rs` |
| Web `sync.rs` not yet migrated to `StorageMessageHandler` | Resolved — `sync.rs` now uses `WebClientHandler` (wrapping `StorageMessageHandler`); all duplicate `process_*` helpers removed. See `docs/clients/howto.md` |

## Genuine Gaps

### 1. Key rotation on member removal

`leave_group_handler` and `remove_group_member_handler` in `src/web_client/handlers/groups.rs`
both have `// TODO: Implement key rotation for remaining members` comments. Until this is done,
removed members can still decrypt future group messages if they retained the group key. This is a
protocol correctness issue.

### 4. Group name vs. group ID ambiguity

`group_id` doubles as the display name throughout the codebase. Separating them would require a
schema migration and protocol change.

### 5. Multi-device signing key portability

`import-key` generates new Ed25519 signing keys rather than importing them. As a result, a peer
operating the same X25519 identity on two devices will have different signing keys per device.
Signatures from device A are unverifiable on device B and vice versa. There is no documented or
implemented path to carry signing keys across devices.
