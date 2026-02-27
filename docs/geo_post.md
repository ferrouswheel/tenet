# Geographic Posts

## Status

- [ ] Plan written
- [ ] `src/protocol.rs` — `GeoLocation`, `GeoPrecision`, new `MetaMessage` variants
- [ ] `src/storage.rs` — schema migration + geo queries
- [ ] `src/message_handler.rs` — geo mesh protocol arms
- [ ] `src/web_client/handlers/messages.rs` — `geo` field in POST + `geo_within` filter
- [ ] `src/web_client/config.rs` — identity geo defaults + constants
- [ ] Tests passing

---

## Problem Statement

Public messages are global by default. A peer returning from offline fetches
all public messages from the last 24 hours with no way to express interest in
only those relevant to their location. For communities that use Tenet for local
coordination (neighbourhood groups, city-level events, regional announcements)
this imposes unnecessary bandwidth, storage, and noise cost.

Two related features are needed:

1. **Geo-tagged posts** — attaching optional, user-controlled geographic
   metadata to public messages so recipients can filter by location.
2. **Geographic mesh queries** — extending the mesh catch-up protocol so a
   peer can ask for public messages within a geographic region, avoiding
   download of globally-scoped content it does not want.

Both features must respect user privacy. Location sharing is opt-in, defaults
to coarse granularity, and can be overridden per post.

---

## Privacy Model

Location precision is controlled at two levels:

| Level | Scope | Overrides |
|-------|-------|-----------|
| **Identity default** | Applies to all posts from this identity | Per-post override |
| **Per-post** | Applies to a single post | — |

A per-post setting always wins over the identity default.

### Precision Levels

| Precision | Example representation | Typical radius | Geohash length |
|-----------|------------------------|----------------|----------------|
| `none` | — | — | — |
| `country` | `"US"` | ~continent | n/a (country code) |
| `region` | `"US/California"` | ~500 km | n/a (text) |
| `city` | `"US/California/San Francisco"` | ~20 km | 4 chars |
| `neighborhood` | geohash `"9q8y"` prefix | ~5 km | 5 chars |
| `exact` | lat/lon + geohash | ~5 m | 9 chars |

`none` emits no location fields. Any post with `precision = none` is treated as
global and excluded from geographic mesh responses.

**Recommended default:** `city`. It allows local content discovery without
exposing neighbourhood or street-level information.

**`exact` is discouraged** for social posts. It should only be used for
content where precise location is the point (e.g., lost/found item). The UI
SHOULD warn users when selecting `exact`.

---

## Data Model

### `GeoLocation` struct (embedded in message payload)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    /// Precision level — determines which fields are populated.
    pub precision: GeoPrecision,

    /// ISO 3166-1 alpha-2 country code. Present for country, region, city.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,

    /// Region/state name (human-readable). Present for region, city.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// City name (human-readable). Present for city.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,

    /// Base-32 geohash string. Present for neighborhood, exact.
    /// Length encodes precision: 5 ≈ 5 km, 9 ≈ 5 m.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geohash: Option<String>,

    /// Latitude in decimal degrees. Present only for exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,

    /// Longitude in decimal degrees. Present only for exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoPrecision {
    None,
    Country,
    Region,
    City,
    Neighborhood,
    Exact,
}
```

`GeoLocation` is embedded as an optional `"geo"` key inside the existing
public-message JSON payload. No changes to `Envelope` or `Header` are
required; the location data rides inside the payload that is already signed.

```jsonc
// Example: public post payload with city-level geo
{
  "body": "Farmers market is amazing today!",
  "geo": {
    "precision": "city",
    "country_code": "US",
    "region": "California",
    "city": "San Francisco"
  }
}
```

```jsonc
// Example: neighborhood precision
{
  "body": "Rain starting now on the east side.",
  "geo": {
    "precision": "neighborhood",
    "geohash": "9q8yy"
  }
}
```

### Validation rules

- `country_code` MUST be a 2-character ISO 3166-1 alpha-2 string if present.
- `geohash` characters MUST be in the base-32 alphabet `[0-9b-hj-np-z]`.
- For `neighborhood` precision, `geohash` MUST be ≥ 5 characters.
- For `exact` precision, `geohash` MUST be ≥ 9 characters, and `lat`/`lon`
  MUST be present and in range (lat ∈ [−90, 90], lon ∈ [−180, 180)).
- If `precision` is `none`, all other fields MUST be absent.
- Receiver implementations MUST silently drop posts with invalid geo payloads
  rather than failing the entire message.

---

## Configuration

### Identity default (`config.toml`)

A new `[geo]` section is added to each identity's `config.toml`:

```toml
[geo]
# Default geographic precision for public posts.
# Options: none, country, region, city, neighborhood, exact
# Default: "city" if omitted.
default_precision = "city"
```

The identity loader reads this section and falls back to `city` if absent.
Writing the identity config does not require a restart — the web client reads
the configured precision at send time.

### Per-post override

The REST API `POST /api/messages/public` accepts an optional `geo` object:

```jsonc
{
  "body": "Hello from Portland",
  "geo": {
    "precision": "city",
    "country_code": "US",
    "region": "Oregon",
    "city": "Portland"
  }
}
```

If `geo` is absent, the server uses the identity's `default_precision`. If
`default_precision` is `none` and no per-post `geo` is supplied, no geo
metadata is attached.

If `"geo": null` is explicitly sent, no geo is attached regardless of the
identity default (explicit opt-out for a single post).

### Settings API

```
GET  /api/settings/geo
POST /api/settings/geo
```

**GET response:**

```json
{
  "default_precision": "city"
}
```

**POST body:**

```json
{
  "default_precision": "neighborhood"
}
```

Updates `config.toml` on disk and returns the new settings.

---

## Storage Schema

### Migration: `messages` table

Add geo columns to the `messages` table (new migration version):

```sql
ALTER TABLE messages ADD COLUMN geo_precision  TEXT;
ALTER TABLE messages ADD COLUMN geo_country    TEXT;
ALTER TABLE messages ADD COLUMN geo_region     TEXT;
ALTER TABLE messages ADD COLUMN geo_city       TEXT;
ALTER TABLE messages ADD COLUMN geo_geohash    TEXT;
ALTER TABLE messages ADD COLUMN geo_lat        REAL;
ALTER TABLE messages ADD COLUMN geo_lon        REAL;
```

All columns are nullable. Existing rows retain `NULL` in all geo columns
(treated as `precision = none`).

An index on `geo_geohash` supports efficient prefix-scan queries:

```sql
CREATE INDEX IF NOT EXISTS idx_messages_geo_geohash
    ON messages (geo_geohash)
    WHERE geo_geohash IS NOT NULL;
```

### New queries in `storage.rs`

```rust
/// List public message IDs since `since` whose geohash starts with
/// `geohash_prefix`. Used for geographic mesh responses.
pub fn list_public_message_ids_since_in_region(
    &self,
    since: u64,
    geohash_prefix: &str,
    limit: u32,
) -> Result<Vec<String>, StorageError>;

/// List public message IDs since `since` for a country/region/city.
/// Used when the requester specified text-based precision (no geohash).
pub fn list_public_message_ids_since_in_named_region(
    &self,
    since: u64,
    country_code: Option<&str>,
    region: Option<&str>,
    city: Option<&str>,
    limit: u32,
) -> Result<Vec<String>, StorageError>;
```

The existing `list_public_message_ids_since` (no geo filter) is unchanged and
is used for global mesh requests.

---

## Protocol Changes

### New `MetaMessage` variants

Two new variants extend the mesh catch-up protocol for geographic scoping:

```rust
/// Geographic variant of MessageRequest.
/// A asks B for public messages within a geographic region since a timestamp.
GeoMessageRequest {
    peer_id: String,
    since_timestamp: u64,
    /// Geohash prefix for neighborhood/exact precision.
    #[serde(skip_serializing_if = "Option::is_none")]
    geohash_prefix: Option<String>,
    /// ISO 3166-1 alpha-2. Used when precision is country/region/city.
    #[serde(skip_serializing_if = "Option::is_none")]
    country_code: Option<String>,
    /// Region name. Used when precision is region/city.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    /// City name. Used when precision is city.
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<String>,
},

/// Response to GeoMessageRequest: IDs of available public messages in the
/// requested region. The responder uses the same region filter as the
/// request, echoes the filter back so the requester can validate.
GeoMeshAvailable {
    peer_id: String,
    message_ids: Vec<String>,
    since_timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    geohash_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<String>,
},
```

After `GeoMeshAvailable`, the existing `MeshRequest` / `MeshDelivery` phases
(phases 3 and 4) are reused unchanged. This keeps the new variants minimal.

### Extended four-phase flow

```
A                                  B
|                                  |
|-- GeoMessageRequest (region) -->|   Phase 1-geo: Geographic query
|<-- GeoMeshAvailable (ids) ------|   Phase 2-geo: Announce matching IDs
|                                  |
| (dedup against local DB)         |
|                                  |
|-- MeshRequest (ids) ----------->|   Phase 3: Request unknowns (unchanged)
|<-- MeshDelivery (envs) ---------|   Phase 4: Deliver batches (unchanged)
```

`GeoMessageRequest` and `GeoMeshAvailable` are parallel to the existing
`MessageRequest` / `MeshAvailable` pair. A peer may issue both kinds of request
to the same peer in the same sync cycle:

- A global `MessageRequest` to catch up on all public messages.
- One or more `GeoMessageRequest` targeting specific regions of interest.

A peer that issued a `GeoMessageRequest` MUST NOT request the same IDs again
in a concurrent global `MeshRequest` (deduplication applies across both).

### Responder behaviour

When a peer receives `GeoMessageRequest`:

1. Apply the same timestamp cap as for `MessageRequest`:
   `since = max(since_timestamp, now - MAX_MESH_WINDOW_SECS)`.
2. Query `list_public_message_ids_since_in_region` (geohash prefix) or
   `list_public_message_ids_since_in_named_region` (country/region/city text).
3. Respond with `GeoMeshAvailable` containing up to `MAX_MESH_IDS` IDs,
   echoing back the same region filter fields.
4. A peer with no messages matching the region responds with an empty
   `message_ids` list rather than not responding.

### Mesh query scoping

A peer's `send_mesh_queries` function (called after each sync) sends:

- One global `MessageRequest` per peer (existing, unchanged).
- One `GeoMessageRequest` per peer for each region of interest configured
  for this identity (see configuration above), if `default_precision != none`.

The region sent in `GeoMessageRequest` is derived from the identity's
`default_precision`:

| `default_precision` | Fields sent |
|---------------------|-------------|
| `country` | `country_code` |
| `region` | `country_code`, `region` |
| `city` | `country_code`, `region`, `city` |
| `neighborhood` | `geohash_prefix` (5 chars) |
| `exact` | `geohash_prefix` (5 chars, truncated for privacy) |

When subscribing by geo, a peer only requests content that is *at least* as
precise as the region filter. A message stored with `precision = country` will
not appear in a `GeoMessageRequest` with `geohash_prefix` filter and vice
versa.

---

## `message_handler.rs` Changes

### New `on_meta` arms

```rust
MetaMessage::GeoMessageRequest { peer_id, since_timestamp, geohash_prefix,
                                 country_code, region, city } => {
    // 1. Cap since_timestamp to MAX_MESH_WINDOW_SECS.
    // 2. Query matching message IDs (geohash OR named region branch).
    // 3. Wrap in GeoMeshAvailable and return as MetaMessage envelope to sender.
}

MetaMessage::GeoMeshAvailable { peer_id, message_ids, since_timestamp,
                                geohash_prefix, country_code, region, city } => {
    // 1. Dedup message_ids against local DB (has_message).
    // 2. Send MeshRequest for unknowns (reuses existing path).
}
```

`MeshRequest` and `MeshDelivery` handling requires no changes.

### `insert_message` update

Extract `geo` from the payload JSON (if present) during `on_message` for
`MessageKind::Public` and populate the new `geo_*` columns.

```rust
if let Some(geo) = payload_json.get("geo") {
    let loc: GeoLocation = serde_json::from_value(geo.clone())?;
    // populate geo_precision, geo_country, geo_region, geo_city,
    // geo_geohash, geo_lat, geo_lon
}
```

---

## Web Client Changes

### `POST /api/messages/public`

Extended request body:

```jsonc
{
  "body": "Hello world",
  "attachments": [...],     // unchanged
  "geo": {                  // optional; null = explicit no-location
    "precision": "city",
    "country_code": "US",
    "region": "California",
    "city": "San Francisco"
  }
}
```

Handler logic:

1. If `geo` field is missing → use identity `default_precision` to build a
   `GeoLocation` (populated from the identity's stored location fields, see below).
2. If `geo` is `null` → omit geo from payload entirely.
3. If `geo` is present → validate and use as-is.
4. Attach `GeoLocation` as the `"geo"` key in the plaintext payload JSON.

### `GET /api/messages`

Add optional `geo_within` query parameter:

```
GET /api/messages?kind=public&geo_within=9q8y
```

`geo_within` is a geohash prefix. Messages are included if their `geo_geohash`
starts with the prefix. Country/region/city text filtering is not exposed
directly; callers use the geohash equivalent computed client-side.

### Identity location store

To support auto-filling geo for posts, each identity stores its current
location in `config.toml`:

```toml
[geo]
default_precision = "city"
country_code = "US"
region = "California"
city = "San Francisco"
# geohash is computed automatically when needed
```

These fields are populated via `POST /api/settings/geo`:

```json
{
  "default_precision": "city",
  "country_code": "US",
  "region": "California",
  "city": "San Francisco"
}
```

The server does not resolve addresses from coordinates. The client (web UI or
mobile app) is responsible for geocoding (reverse-lookup lat/lon → place name)
before calling the API.

---

## Constants

Add to `src/web_client/config.rs`:

```rust
/// Maximum geohash prefix length accepted in GeoMessageRequest.
pub const MAX_GEO_GEOHASH_PREFIX_LEN: usize = 9;

/// Geohash prefix length used when subscribing at "neighborhood" precision.
pub const NEIGHBORHOOD_GEOHASH_PREFIX_LEN: usize = 5;

/// Geohash prefix length used when subscribing for "exact" precision
/// (truncated to neighborhood granularity for query privacy).
pub const EXACT_QUERY_GEOHASH_PREFIX_LEN: usize = 5;
```

---

## File Change Summary

| File | Change |
|------|--------|
| `src/protocol.rs` | `GeoLocation` struct, `GeoPrecision` enum, `GeoMessageRequest` + `GeoMeshAvailable` MetaMessage variants |
| `src/storage.rs` | 7 new geo columns + index; `list_public_message_ids_since_in_region`; `list_public_message_ids_since_in_named_region`; geo extraction in `insert_message` |
| `src/message_handler.rs` | `GeoMessageRequest` + `GeoMeshAvailable` arms in `on_meta`; geo extraction in `on_message` for public kind |
| `src/web_client/handlers/messages.rs` | Accept `geo` in POST; `geo_within` query param in GET |
| `src/web_client/handlers/settings.rs` | New `GET/POST /api/settings/geo` handlers |
| `src/web_client/config.rs` | 3 new geo constants |
| `src/web_client/sync.rs` | Send `GeoMessageRequest` per peer when `default_precision != none` |
| `config.toml` (per-identity) | `[geo]` section with `default_precision` + location fields |

---

## Out of Scope (for now)

- **Geocoding / reverse geocoding**: The server does not call external services
  to convert coordinates to place names. This is a client responsibility.
- **Region subscription management**: Only a single home region is supported
  per identity. Multiple subscriptions (e.g., home city + work city) are future
  work.
- **Relay-side geo filtering**: The relay is untrusted and geo filtering happens
  client-side. Relay-side indexing is out of scope.
- **Simulation harness**: Geo-tagged messages and geographic mesh queries are
  not wired into the simulation scenarios.
- **Android FFI**: `GeoLocation` type will need to be exposed via UniFFI in
  `android/tenet-ffi/src/types.rs`. Tracked as separate work.
- **UI map view**: A geographic map view of nearby posts in the web client is a
  UX feature, not a protocol feature. Tracked separately.
