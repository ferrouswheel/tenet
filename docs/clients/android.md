# Android Client Plan

This document describes the plan for building a native Android client for Tenet. The feature set
targets parity with the web client, extended with Android-native capabilities (background sync,
system notifications, Keystore-backed key protection, and share integration).

## Architecture Overview

The Android client is split into two layers:

```
┌──────────────────────────────────────────────────────────┐
│                   Android Application                    │
│  ┌────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │  Timeline  │  │     DMs      │  │  Peers / Groups  │  │
│  └──────┬─────┘  └──────┬───────┘  └────────┬─────────┘  │
│         └───────────────┴──────────────────-┘            │
│               Jetpack Compose UI + ViewModel             │
│  ┌───────────────────────────────────────────────────┐   │
│  │          TenetRepository (Kotlin)                 │   │
│  └───────────────────────┬───────────────────────────┘   │
│                          │ UniFFI-generated Kotlin API   │
└──────────────────────────┼───────────────────────────────┘
                           │ JNI
┌──────────────────────────┴───────────────────────────────┐
│               libtenet_ffi.so  (Rust)                    │
│  ┌────────────────────────────────────────────────────┐  │
│  │   tenet-ffi crate  (thin UniFFI wrapper)           │  │
│  ├────────────────────────────────────────────────────┤  │
│  │   tenet library (protocol, crypto, storage,        │  │
│  │                  identity, groups, client)         │  │
│  ├────────────────────────────────────────────────────┤  │
│  │   rusqlite (bundled SQLite)                        │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

### Key design choices

| Concern | Choice | Rationale |
|---------|--------|-----------|
| Rust–Kotlin binding | [UniFFI](https://github.com/mozilla/uniffi-rs) | Generates type-safe Kotlin bindings from a UDL interface; used in production by Firefox for Android |
| Database | `rusqlite` (bundled SQLite) | Reuses the existing Tenet schema and `Storage` type; no ORM translation layer |
| UI framework | Jetpack Compose | Modern declarative Android UI |
| State management | ViewModel + StateFlow | Lifecycle-aware, idiomatic Kotlin |
| Background sync | WorkManager (`PeriodicWorkRequest`) | Battery-efficient, survives process death; OS-managed scheduling |
| System notifications | `NotificationManager` + notification channels | Android 8+ channels for messages, friend requests, group invites |
| Key protection | Android Keystore wrapping | Wrap a random AEK with an Android Keystore key; unlock on biometric or screen lock |
| File picking | `ActivityResultContracts.GetContent` | Standard Android file/image picker |
| Image loading | Coil | Kotlin-first, Compose-compatible |

---

## Feature Scope

All features from the web client are in scope. The table below maps web client capabilities to the
Android equivalent.

### Messaging

| Feature | Web | Android |
|---------|-----|---------|
| Public timeline (paginated feed) | ✓ | ✓ |
| Friend group feed tab | ✓ | ✓ |
| Compose public message | ✓ | ✓ |
| Send direct message | ✓ | ✓ |
| Send group message | ✓ | ✓ |
| Thread / reply to post | ✓ | ✓ |
| Reactions (upvote / downvote) | ✓ | ✓ |
| Attachments (images and files) | ✓ | ✓ |
| Mark message read | ✓ | ✓ |
| Unread indicators | ✓ | ✓ |

### Peers

| Feature | Web | Android |
|---------|-----|---------|
| Peer list (sorted by activity) | ✓ | ✓ |
| Peer detail view | ✓ | ✓ |
| Add peer (by peer ID + key) | ✓ | ✓ |
| Remove peer | ✓ | ✓ |
| Block / unblock | ✓ | ✓ |
| Mute / unmute | ✓ | ✓ |
| Online presence indicators | ✓ | ✓ |
| Recent peer activity | ✓ | ✓ |

### Social

| Feature | Web | Android |
|---------|-----|---------|
| Send friend request | ✓ | ✓ |
| Accept / ignore / block request | ✓ | ✓ |
| Friend list | ✓ | ✓ |
| Friend request notifications | ✓ | ✓ |

### Groups

| Feature | Web | Android |
|---------|-----|---------|
| Create group | ✓ | ✓ |
| Group invite flow | ✓ | ✓ |
| Accept / ignore invite | ✓ | ✓ |
| Group message timeline | ✓ | ✓ |
| Add / remove member | ✓ | ✓ |
| Leave group | ✓ | ✓ |

### Profiles

| Feature | Web | Android |
|---------|-----|---------|
| View / edit own profile | ✓ | ✓ |
| Avatar upload | ✓ | ✓ |
| View peer profiles | ✓ | ✓ |
| Request profile from peer | ✓ | ✓ |
| Custom fields (public / friends) | ✓ | ✓ |

### Notifications

| Feature | Web | Android |
|---------|-----|---------|
| In-app notification list | ✓ | ✓ |
| Unread badge count | ✓ | ✓ |
| Mark read / mark all read | ✓ | ✓ |
| OS system notifications | — | ✓ Android-native |

### Sync

| Feature | Web | Android |
|---------|-----|---------|
| Manual sync trigger | ✓ | ✓ |
| Background sync | Tokio timer | WorkManager |
| Relay connection status | ✓ | ✓ |

### Android-only features

| Feature | Notes |
|---------|-------|
| System notification channels | Separate channels: "Messages", "Friend Requests", "Group Invites" |
| Deep links from notification tap | Navigate directly to message / conversation / request |
| Biometric / screen-lock protection | Keystore-wrapped identity key requires authentication to unlock |
| Share-to intent | Receive shared text/images from other apps to compose a new post |
| QR code peer ID sharing | Show own peer ID as QR; scan a peer's QR to add them |
| Large file share via SAF | Use Android Storage Access Framework for files > system picker |

---

## Rust FFI Layer (`tenet-ffi`)

### Crate structure

Add a new crate at `android/tenet-ffi/` (a `cdylib` target):

```
android/
  tenet-ffi/
    Cargo.toml          # [lib] crate-type = ["cdylib"]
    src/
      lib.rs            # UniFFI entry point + exported functions
      types.rs          # Mirror types for FFI boundary (simple structs, no generics)
    tenet_ffi.udl       # UniFFI interface definition language file
```

`Cargo.toml` depends on the workspace `tenet` library crate and `uniffi`.

### UDL design principles

The UDL file declares the public Kotlin-facing API:

- **No Rust-specific types** at the boundary — use `String`, `i64`, `u32`, `bytes`, and flat
  structs. Convert internal types (e.g. `MessageRow`, `PeerRow`) into simple mirror structs.
- **Errors** use UniFFI error enums so Kotlin sees typed exceptions.
- **Blocking calls only** — the FFI functions are synchronous. Kotlin coroutines dispatch to an
  IO thread; no async required inside Rust.

### Proposed UDL interface (sketch)

```webidl
namespace tenet_ffi {
  // Identity
  TenetIdentity init_identity(string data_dir);
  TenetIdentity load_identity(string data_dir);

  // Messages
  sequence<FfiMessage> list_messages(TenetClient client, string kind, u32 limit, i64 before_ts);
  FfiMessage get_message(TenetClient client, string message_id);
  void send_direct(TenetClient client, string recipient_id, string body, sequence<FfiAttachment> attachments);
  void send_public(TenetClient client, string body, sequence<FfiAttachment> attachments);
  void send_group(TenetClient client, string group_id, string body, sequence<FfiAttachment> attachments);
  void mark_read(TenetClient client, string message_id);
  void react(TenetClient client, string message_id, string reaction);
  void unreact(TenetClient client, string message_id);
  void reply(TenetClient client, string message_id, string body);

  // Peers
  sequence<FfiPeer> list_peers(TenetClient client);
  FfiPeer get_peer(TenetClient client, string peer_id);
  void add_peer(TenetClient client, string peer_id, string display_name, string signing_public_key_hex);
  void remove_peer(TenetClient client, string peer_id);
  void block_peer(TenetClient client, string peer_id);
  void unblock_peer(TenetClient client, string peer_id);
  void mute_peer(TenetClient client, string peer_id);
  void unmute_peer(TenetClient client, string peer_id);

  // Friends
  void send_friend_request(TenetClient client, string peer_id, string? message);
  sequence<FfiFriendRequest> list_friend_requests(TenetClient client);
  void accept_friend_request(TenetClient client, i64 request_id);
  void ignore_friend_request(TenetClient client, i64 request_id);
  void block_friend_request(TenetClient client, i64 request_id);

  // Groups
  sequence<FfiGroup> list_groups(TenetClient client);
  FfiGroup get_group(TenetClient client, string group_id);
  void create_group(TenetClient client, string group_id, sequence<string> member_ids);
  void add_group_member(TenetClient client, string group_id, string peer_id);
  void remove_group_member(TenetClient client, string group_id, string peer_id);
  void leave_group(TenetClient client, string group_id);
  sequence<FfiGroupInvite> list_group_invites(TenetClient client);
  void accept_group_invite(TenetClient client, i64 invite_id);
  void ignore_group_invite(TenetClient client, i64 invite_id);

  // Profiles
  FfiProfile get_own_profile(TenetClient client);
  void update_profile(TenetClient client, string? display_name, string? bio, bytes? avatar_bytes);
  FfiProfile get_peer_profile(TenetClient client, string peer_id);

  // Attachments
  string upload_attachment(TenetClient client, bytes data, string content_type);
  bytes download_attachment(TenetClient client, string content_hash);

  // Notifications
  sequence<FfiNotification> list_notifications(TenetClient client, boolean unread_only);
  u32 notification_count(TenetClient client);
  void mark_notification_read(TenetClient client, i64 notification_id);
  void mark_all_notifications_read(TenetClient client);

  // Sync
  FfiSyncResult sync(TenetClient client);
  string my_peer_id(TenetClient client);
  string relay_url(TenetClient client);
};

interface TenetClient {
  constructor(string data_dir, string relay_url);
};
```

The `TenetClient` wraps `RelayClient` + `Storage` + `GroupManager` + identity, all owned together.
Internally it locks a `Mutex` before each operation so concurrent Kotlin coroutine calls are safe.

### FFI mirror types (examples)

```rust
// tenet-ffi/src/types.rs
pub struct FfiMessage {
    pub message_id: String,
    pub sender_id: String,
    pub recipient_id: Option<String>,
    pub kind: String,           // "public" | "direct" | "friend_group"
    pub group_id: Option<String>,
    pub body: String,
    pub timestamp: i64,
    pub is_read: bool,
    pub reply_to: Option<String>,
    pub attachment_hashes: Vec<String>,
    pub reactions_up: u32,
    pub reactions_down: u32,
    pub my_reaction: Option<String>,
}

pub struct FfiPeer {
    pub peer_id: String,
    pub display_name: Option<String>,
    pub is_online: bool,
    pub is_blocked: bool,
    pub is_muted: bool,
    pub is_friend: bool,
    pub last_seen: Option<i64>,
}

pub struct FfiSyncResult {
    pub fetched: u32,
    pub new_messages: u32,
    pub errors: Vec<String>,
}
```

---

## Android Application

### Technology stack

| Layer | Library / Tool |
|-------|---------------|
| Language | Kotlin |
| UI | Jetpack Compose (Material 3) |
| Navigation | Navigation Compose |
| State | ViewModel + StateFlow |
| Async | Kotlin Coroutines (`Dispatchers.IO` for FFI calls) |
| DI | Hilt |
| Image loading | Coil |
| Background work | WorkManager |
| QR code | ZXing Android Embedded |
| Build | Gradle + Kotlin DSL |

### Screen inventory

| Screen | Description |
|--------|-------------|
| **Setup** | First-run identity creation; enter relay URL |
| **Timeline** | Paginated feed with "Public" / "Friend Group" tabs; compose FAB |
| **Compose** | New post / DM / group message; attachment picker; group selector |
| **Post Detail** | Full post, reactions, comment thread |
| **Conversations** | DM conversation list |
| **Conversation Detail** | Messages with one peer; compose box |
| **Peers** | Peer list sorted by activity; add peer by ID or QR scan |
| **Peer Detail** | Profile info, online status, block/mute/DM/friend actions, activity feed |
| **Friends** | Friend list + incoming / outgoing requests |
| **Groups** | Group list + create group; pending invites |
| **Group Detail** | Group members, leave / add member |
| **Profile** | Own profile view + edit; avatar picker |
| **Notifications** | Notification list; mark read |
| **Settings** | Relay URL, sync interval, identity export |

### Navigation structure

```
BottomNavBar ──► Timeline
             ──► Conversations ──► ConversationDetail
             ──► Friends
             ──► Peers ──► PeerDetail
             ──► Profile

Notification tap ──► deep-link to any of the above
```

### ViewModel / Repository pattern

```
UI (Composables)
    │  observe StateFlow<UiState>
    ▼
ViewModel
    │  suspend fun via Dispatchers.IO
    ▼
TenetRepository (Kotlin)
    │  calls UniFFI-generated Kotlin functions
    ▼
TenetClient (Rust, via JNI)
```

`TenetRepository` is a singleton (Hilt `@Singleton`) holding the `TenetClient` instance. All
repository functions are `suspend` functions that dispatch to `Dispatchers.IO` so the FFI blocking
call never touches the main thread.

### Background sync (WorkManager)

A `PeriodicWorkRequest` runs `SyncWorker` every 15 minutes (the minimum WorkManager interval) while
the relay URL is configured. Constraints: `NetworkType.CONNECTED`.

```kotlin
class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        try {
            val result = repository.sync()
            if (result.newMessages > 0) postSystemNotifications(result)
            Result.success()
        } catch (e: Exception) {
            Result.retry()
        }
    }
}
```

When the app is in the foreground a shorter poll (every 30 s) runs via a `viewModelScope` coroutine
that cancels on lifecycle stop, matching the web client's timer behaviour.

### System notifications

Three notification channels:

| Channel | ID | Messages |
|---------|-----|---------|
| New messages | `tenet_messages` | Direct messages and group messages |
| Friend requests | `tenet_friends` | Incoming friend requests and acceptances |
| Group invites | `tenet_groups` | Group invite received |

Tapping a notification deep-links into the relevant screen using an `Intent` with a `tenet://`
custom scheme URI (e.g. `tenet://conversation/<peer_id>`, `tenet://post/<message_id>`).

### Identity key protection (Android Keystore)

The Tenet identity (private HPKE + signing keys) is stored encrypted on disk. On first launch the
app generates a random 256-bit data-encryption key (DEK) and wraps it with an `AES/GCM`
`SecretKey` stored in the Android Keystore, configured to require user authentication (biometric or
screen lock) before use. The Keystore key never leaves secure hardware on supported devices.

On subsequent launches the Keystore key is retrieved and used to unwrap the DEK, which decrypts the
identity file before handing the path to `TenetClient`.

The Rust library itself is unmodified — key protection is an Android layer concern.

### Share-to intent

The app declares an intent filter for `ACTION_SEND` with `text/plain` and `image/*`. When another
app shares content to Tenet, the Compose screen launches with the shared content pre-populated in
the compose form.

### QR code peer ID sharing

- **Show**: Display own peer ID as a QR code (ZXing). Useful for in-person peer addition.
- **Scan**: Camera-based QR scan to populate the "Add Peer" form.

---

## Project Structure

```
tenet/                          ← existing Rust workspace root
  Cargo.toml                    ← add `android/tenet-ffi` to workspace members
  android/
    tenet-ffi/                  ← new Rust crate
      Cargo.toml
      src/
        lib.rs
        types.rs
      tenet_ffi.udl
    app/                        ← Android Studio project
      build.gradle.kts
      settings.gradle.kts
      local.properties          ← NDK path (git-ignored)
      app/
        build.gradle.kts        ← declares jniLibs, uniffi dependency
        src/main/
          AndroidManifest.xml
          java/com/example/tenet/
            MainActivity.kt
            TenetApplication.kt
            data/
              TenetRepository.kt
              SyncWorker.kt
            ui/
              timeline/
              compose/
              conversations/
              peers/
              friends/
              groups/
              profile/
              notifications/
              settings/
              setup/
            theme/
              Theme.kt
          jniLibs/               ← compiled .so files land here (git-ignored)
            arm64-v8a/
              libtenet_ffi.so
            armeabi-v7a/
              libtenet_ffi.so
            x86_64/
              libtenet_ffi.so
          assets/
```

---

## Build Guide

### Prerequisites

Install the following before building.

**Rust toolchain**

```bash
# Install rustup if not already present
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Add Android cross-compilation targets
rustup target add aarch64-linux-android   # arm64 (modern devices)
rustup target add armv7-linux-androideabi # 32-bit ARM (older devices)
rustup target add x86_64-linux-android    # x86_64 emulator

# Install cargo-ndk — wraps cargo to use the NDK toolchain
cargo install cargo-ndk
```

**Android SDK and NDK**

Install via Android Studio → SDK Manager:

- Android SDK Platform 34 (or later)
- Android NDK (Side by side) — version 25 or later
  (`ANDROID_NDK_HOME` must point to the NDK root, e.g. `~/Android/Sdk/ndk/25.2.9519653`)
- Android SDK Build-Tools 34

Alternatively set `ANDROID_HOME` and let `sdkmanager` handle installation:

```bash
sdkmanager "platforms;android-34" "ndk;25.2.9519653" "build-tools;34.0.0"
```

**Java**

JDK 17 or later (required by recent Gradle). `JAVA_HOME` must be set.

**uniffi-bindgen** (generates Kotlin bindings from the UDL file)

```bash
cargo install uniffi-bindgen
```

---

### Step 1 — Build the Rust FFI library

From the workspace root:

```bash
cd android/tenet-ffi

# Build for all three Android ABIs
cargo ndk \
  -t aarch64-linux-android \
  -t armv7-linux-androideabi \
  -t x86_64-linux-android \
  -o ../app/app/src/main/jniLibs \
  build --release
```

`cargo-ndk` places the compiled `.so` files directly into the correct ABI subdirectories under
`jniLibs/`. The output is:

```
android/app/app/src/main/jniLibs/
  arm64-v8a/libtenet_ffi.so
  armeabi-v7a/libtenet_ffi.so
  x86_64-linux-android/libtenet_ffi.so   # renamed to x86_64/ by cargo-ndk
```

For a debug build (faster, larger binary) omit `--release`.

---

### Step 2 — Generate Kotlin bindings

UniFFI generates Kotlin source from the UDL file:

```bash
uniffi-bindgen generate \
  android/tenet-ffi/tenet_ffi.udl \
  --language kotlin \
  --out-dir android/app/app/src/main/java/com/example/tenet/uniffi/
```

This writes `tenet_ffi.kt` (and a runtime helper) into the specified package directory. Re-run
whenever `tenet_ffi.udl` changes. Committing the generated file is optional; most projects
regenerate it during the build.

To automate generation as part of the Gradle build, add an `exec` task in `app/build.gradle.kts`
that runs `uniffi-bindgen` before `compileDebugKotlin`.

---

### Step 3 — Configure the Android project

In `app/build.gradle.kts`, declare the `jniLibs` source set and add the UniFFI runtime dependency:

```kotlin
android {
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
}

dependencies {
    // UniFFI Kotlin runtime (version must match uniffi-bindgen version)
    implementation("net.java.dev.jna:jna:5.14.0@aar")
}
```

UniFFI uses JNA to call into the native library. Include the `@aar` classifier to get the Android
variant of JNA.

Set `ANDROID_NDK_HOME` in `local.properties` (this file is git-ignored):

```
# local.properties
sdk.dir=/home/user/Android/Sdk
ndk.dir=/home/user/Android/Sdk/ndk/25.2.9519653
```

---

### Step 4 — Build the Android app

```bash
cd android/app

# Debug APK
./gradlew assembleDebug

# Release APK (requires signing config)
./gradlew assembleRelease

# Install on connected device / emulator
./gradlew installDebug
```

The APK bundles only the ABI(s) required by the target device. For release you can split by ABI to
reduce download size:

```kotlin
// app/build.gradle.kts
android {
    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "armeabi-v7a", "x86_64")
            isUniversalApk = false
        }
    }
}
```

---

### Step 5 — Run tests

**Rust unit tests** (host machine, no device required):

```bash
cargo test -p tenet-ffi
```

**Android instrumented tests** (requires device or emulator):

```bash
cd android/app
./gradlew connectedAndroidTest
```

**Android unit tests** (JVM, fast):

```bash
./gradlew test
```

---

### Convenience script

A shell script `android/build.sh` can wrap steps 1–3:

```bash
#!/usr/bin/env bash
set -e

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFI_CRATE="$WORKSPACE_ROOT/android/tenet-ffi"
JNI_OUT="$WORKSPACE_ROOT/android/app/app/src/main/jniLibs"
KOTLIN_OUT="$WORKSPACE_ROOT/android/app/app/src/main/java/com/example/tenet/uniffi"

echo "==> Building Rust FFI library..."
cargo ndk \
  --manifest-path "$FFI_CRATE/Cargo.toml" \
  -t aarch64-linux-android \
  -t armv7-linux-androideabi \
  -t x86_64-linux-android \
  -o "$JNI_OUT" \
  build --release

echo "==> Generating Kotlin bindings..."
uniffi-bindgen generate \
  "$FFI_CRATE/tenet_ffi.udl" \
  --language kotlin \
  --out-dir "$KOTLIN_OUT"

echo "==> Done. Run 'cd android/app && ./gradlew assembleDebug' to build the APK."
```

---

## Implementation Roadmap

### Phase 1 — Foundation ✓ Complete

- [x] Create `android/tenet-ffi/` crate with minimal UDL covering identity init, sync, list/send messages
- [ ] Verify `cargo-ndk` build succeeds for all three ABIs *(requires Android NDK — see Build Guide)*
- [ ] Generate Kotlin bindings and verify JNI linkage in a stub Android project *(requires `uniffi-bindgen` and NDK)*
- [x] Implement `TenetClient` Rust struct (wraps `RelayClient` + `Storage` behind a `Mutex`)
- [x] Basic Android project skeleton: `MainActivity`, Hilt setup, `TenetRepository`
- [x] Timeline screen (read-only): fetch and display public messages

#### Phase 1 implementation notes

**Rust FFI crate (`android/tenet-ffi/`)**

- The UDL file lives at `android/tenet-ffi/src/tenet_ffi.udl` (not the crate root).
  UniFFI's `guess_crate_root` walks upward from the UDL file's *parent* directory to find
  `Cargo.toml`; if the UDL is at the crate root, it looks in the wrong grandparent and panics.
  Placing the UDL inside `src/` ensures the crate root is correctly resolved.

- `build.rs` must supply an **absolute path** to the UDL file using `CARGO_MANIFEST_DIR`:
  ```rust
  let udl_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("src").join("tenet_ffi.udl");
  uniffi::generate_scaffolding(udl_path.to_str().unwrap()).unwrap();
  ```

- The workspace `Cargo.toml` gains a `[workspace]` section so that `cargo build -p tenet-ffi`
  resolves the path dependency on the `tenet` library correctly:
  ```toml
  [workspace]
  members = [".", "android/tenet-ffi"]
  ```

- **HPKE constants** in the FFI (`FFI_HPKE_INFO = b"tenet-web-v1"`, `FFI_PAYLOAD_AAD = b"tenet-payload-v1"`)
  must match the web client constants (`WEB_HPKE_INFO` / `WEB_PAYLOAD_AAD` in
  `src/web_client/config.rs`). Mismatched values silently break decryption across clients.

- **Sync pattern**: to avoid holding the `Mutex` across the network call, `sync()` extracts
  the keypair, relay URL, peers, and DB path under a short lock, drops the lock, then opens a
  *second* `Storage` connection for `StorageMessageHandler` (SQLite WAL mode permits this).
  This mirrors the web client's `sync_once()` pattern exactly.

- `send_public_message()` on `RelayClient` already calls `post_envelope()` and
  `forward_public_message_to_peers()` internally. Do **not** call `post_envelope()` again
  from the FFI layer or the envelope will be double-posted.

- `validate_hex_key()` in `tenet::crypto` takes three arguments
  (`hex_str`, `expected_len: usize`, `field_name: &str`) — not one.
  Ed25519 signing keys are 32 bytes.

**Android Kotlin skeleton (`android/app/`)**

- The `uniffi/` source directory (generated by `uniffi-bindgen`) and `jniLibs/` (compiled
  `.so` files) are **git-ignored** — they are regenerated by `android/build.sh` before each
  Android build.

- `TenetRepository` uses a double-checked lock pattern (`@Volatile` + `synchronized`) so the
  `TenetClient` is created exactly once even under concurrent coroutine calls.

- The `SetupScreen` handles first-run initialization; subsequent launches with a configured
  relay URL will skip Setup and go directly to Timeline (to be wired in Phase 2 by checking
  `TenetRepository.isInitialized()` in a splash/entry composable).

### Phase 2 — Core messaging ✓ Complete

- [x] Compose screen: send public messages
- [x] Conversation list + conversation detail (DMs)
- [x] Post detail with reply thread
- [x] Reactions (upvote / downvote)
- [x] Attachment upload / download (content-addressed, SHA256 hash)
- [x] Friend-group messages (`send_group`) via direct protocol path (see note)
- [x] Bottom navigation bar (Timeline ↔ Messages)

#### Phase 2 implementation notes

**Rust FFI additions (`android/tenet-ffi/src/lib.rs`)**

- Added `sha2 = "0.10"`, `base64 = "0.22"`, and `rand = "0.8"` to `android/tenet-ffi/Cargo.toml`
  for attachment hashing, base64url encoding, and random salts.

- **`send_group` bypasses `RelayClient::send_group_message()`** because that method calls
  `group.is_member(self.id())` and `GroupManager::add_group_key()` creates a `GroupInfo` with an
  empty member set, causing the check to fail for the local peer. The FFI layer instead calls the
  protocol primitives directly:
  ```rust
  let payload = build_group_message_payload(body.as_bytes(), &group_key, group_id.as_bytes())?;
  let envelope = build_envelope_from_payload(..., MessageKind::FriendGroup, ...)?;
  post_envelope(&inner.relay_url, &envelope)?;
  ```

- **`list_direct_messages`** — there is no native peer-pair index in storage. The implementation
  fetches a batch of `"direct"` messages (4× the requested limit, min 200) and filters to those
  where `sender_id` or `recipient_id` matches the requested peer.

- **Reactions** use a raw JSON body in a plaintext `MessageKind::Meta` envelope. The `MetaMessage`
  enum has no `Reaction` variant; the web client embeds the reaction as a raw string body, so the
  FFI layer mirrors this pattern exactly:
  ```rust
  let reaction_body = serde_json::json!({
      "target_message_id": &target_message_id,
      "reaction": &reaction,
      "timestamp": now,
  }).to_string();
  // build_plaintext_envelope(..., MessageKind::Meta, ..., &reaction_body, ...)
  ```

- **Attachments** use SHA256 content-addressing: `SHA256(data)` → base64url-encoded
  `content_hash` stored via `storage.insert_attachment()`. This matches the web client's
  `ContentId` scheme.

- **`list_conversations`** calls `storage.list_conversations(&my_id)` (takes the local peer ID to
  exclude self from the peer column), then enriches each row with the display name from the peer
  table.

- **Helper functions** extracted to keep `TenetClient` impl readable:
  `client_config()`, `row_to_ffi()`, `make_msg_row()`, `reaction_summary()`.

**Android Kotlin additions (`android/app/`)**

- **New screens and view models** under `ui/`:
  - `compose/` — `ComposeScreen` + `ComposeViewModel`: text field + send button; posts as public
    message; returns to caller on success.
  - `conversations/` — `ConversationsScreen` + `ConversationsViewModel`: DM list grouped by peer,
    showing last message, timestamp, and unread badge.
  - `conversations/` — `ConversationDetailScreen` + `ConversationDetailViewModel`: bubble-style
    chat thread (newest at bottom via `reverseLayout = true`), compose bar for new DMs.
    `SavedStateHandle["peerId"]` supplies the destination peer.
  - `postdetail/` — `PostDetailScreen` + `PostDetailViewModel`: parent post card with
    upvote/downvote buttons, scrollable reply thread, and reply compose bar.
    `SavedStateHandle["messageId"]` supplies the target message.

- **`TimelineViewModel`** gains a `TimelineTab` enum (`PUBLIC` / `FRIEND_GROUP`) and
  `selectTab()` function; `loadMessages()` and `sync()` both respect the active tab.

- **`TimelineScreen`** now shows a `TabRow` for Public/Friends, a compose `FloatingActionButton`
  (+ a smaller sync FAB above it), and reaction icons on each card as visual affordances.
  Tapping a card navigates to `PostDetailScreen`.

- **`TenetNavHost`** restructured with an outer `Scaffold` holding a `NavigationBar` that is only
  shown for the `timeline` and `conversations` routes.  Route constants split into base path
  (`POST_BASE = "post"`) and full pattern (`POST_DETAIL = "post/{messageId}"`) to allow safe
  `navController.navigate("post/$id")` without string template collision.

### Phase 3 — Social features ✓ Complete

- [x] Peer list and peer detail screens
- [x] Add peer by ID (manual entry)
- [x] Friend request send / accept / ignore / block
- [x] Group create, invite, accept invite, leave
- [x] Profile view and edit (with avatar)

#### Phase 3 implementation notes

**Rust FFI additions (`android/tenet-ffi/src/`)**

- **New FFI types** added to `types.rs` and the UDL:
  `FfiFriendRequest`, `FfiGroup`, `FfiGroupMember`, `FfiGroupInvite`, `FfiProfile`,
  `FfiNotification`.

- **Peer detail** — `get_peer()` and `block_peer()` / `unblock_peer()` / `mute_peer()` /
  `unmute_peer()` delegate directly to `storage.set_peer_blocked()` /
  `storage.set_peer_muted()`, which are already idempotent.

- **Friend request upsert** — `send_friend_request()` checks
  `storage.find_request_between()` first; if a prior request exists it calls
  `storage.refresh_friend_request()` (resets to `pending`) rather than inserting a
  duplicate.  Acceptance uses `INSERT OR REPLACE` (`insert_peer`) with `is_friend: true` to
  atomically update the peer row.  `block_friend_request()` additionally calls
  `set_peer_blocked()` so the peer is silenced app-wide.

- **Group creation** — `create_group()` generates a random URL-safe base64 group ID
  (16 bytes) and a random 32-byte group key via `OsRng`, then calls
  `storage.insert_group_with_members()`.  The creator is automatically added as a member if
  not already present in the supplied list.

- **`leave_group()`** — removes self from the member list; deletes the group record if
  no members remain.  No protocol message is sent in this implementation (best-effort).

- **Profile** — `update_own_profile()` merges with the existing profile row so that
  unset fields (e.g. `bio: None`) do not overwrite previously stored values.  Avatar upload
  is handled separately via `upload_attachment()` — pass the returned hash to
  `update_own_profile(avatar_hash: Some(...))`.

- **Notifications** — `list_notifications()` / `notification_count()` /
  `mark_notification_read()` / `mark_all_notifications_read()` delegate directly to the
  matching `Storage` methods.

**Android Kotlin additions (`android/app/`)**

- **New screens and view models** under `ui/`:
  - `peers/` — `PeersScreen` + `PeersViewModel`: sorted peer list (online-first), add-peer
    dialog accepting peer ID + optional display name + signing key hex.  Long-pressing a peer
    navigates to `PeerDetailScreen`.
  - `peers/` — `PeerDetailScreen` + `PeerDetailViewModel`: shows peer ID, profile bio,
    online/friend/blocked/muted status chips; action buttons for DM, friend request (with
    optional greeting message), block/unblock, mute/unmute, remove.  Friend request button is
    disabled if an outgoing `pending` request already exists.
  - `friends/` — `FriendsScreen` + `FriendsViewModel`: two lists — incoming pending requests
    (accept / ignore / block row actions) and outgoing requests (status badge).  A Groups icon
    in the TopAppBar navigates to `GroupsScreen`.
  - `groups/` — `GroupsScreen` + `GroupsViewModel`: list of joined groups + pending incoming
    invites (join / ignore); FAB opens a create-group dialog where member peer IDs are entered
    as a comma-separated string.
  - `groups/` — `GroupDetailScreen` + `GroupDetailViewModel`: member list with per-member
    remove buttons; add-member dialog; leave-group confirmation dialog.  `leftGroup` state
    triggers automatic back navigation on leave.
  - `profile/` — `ProfileScreen` + `ProfileViewModel`: read-only view showing peer ID,
    display name, bio; edit mode via TopAppBar icon shows OutlinedTextFields for display name
    and bio.  Avatar upload left for Phase 4 (requires image picker integration).

- **Navigation changes** (`TenetNavHost.kt`):
  - Bottom nav expanded from 2 to 5 items: **Timeline**, **Messages**, **Peers**,
    **Friends**, **Profile**.
  - New routes: `peers`, `friends`, `groups`, `profile` (top-level); `peer/{peerId}`,
    `group/{groupId}` (detail, full-screen, no bottom nav).
  - `GroupsScreen` is reachable via the Groups icon in `FriendsScreen`'s TopAppBar (not a
    bottom-nav root, consistent with the architecture diagram).

### Phase 4 — Android-native features

- [ ] WorkManager background sync with system notifications
- [ ] Notification deep-link navigation
- [ ] Android Keystore identity key wrapping
- [ ] QR code show/scan for peer ID sharing
- [ ] Share-to intent handler

### Phase 5 — Polish

- [ ] Settings screen (relay URL, sync interval, identity export)
- [ ] Offline / no-relay error states
- [ ] Accessibility (content descriptions, large-text support)
- [ ] Dark theme
- [ ] Release signing and Play Store listing preparation

---

## Open Questions

- **Identity export / import**: Should the app support exporting the keypair (e.g. as a passphrase-
  encrypted backup) for migration to a new device? The existing CLI `tenet init` flow could inform
  this.

- **Multiple identities**: The web client and CLI already support multiple identities via
  `TENET_IDENTITY`. The Android app could expose identity switching in Settings, but the UDL
  interface above assumes a single active identity per `TenetClient` instance.

- **Relay discovery**: Currently the relay URL is user-configured. A default relay URL baked into
  the app would lower the barrier to entry, but raises trust questions.

- **FCM integration**: WorkManager polling is sufficient for now. If lower-latency background wakeup
  is needed, the relay could optionally send an FCM push to trigger an immediate sync. This would
  require relay-side FCM support and registration token management in the app.

- **NDK minimum version**: NDK 25 is recommended (stable LLVM toolchain). Earlier versions (21+)
  may work but are untested.

- **`armeabi-v7a` support**: 32-bit ARM devices are rare on recent Android versions (API 21+).
  Dropping this target simplifies builds and reduces APK size; worth revisiting based on target
  device distribution.
