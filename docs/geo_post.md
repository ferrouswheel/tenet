# Geographic Posts

## Status

- [x] Plan written
- [ ] `Cargo.toml` — add `geohash`, `country-boundaries`, `reverse_geocoder`, `iso3166-1` dependencies
- [x] `src/geo.rs` — new module: `GeoLocation`, `GeoPrecision`, country seed table, `resolve_geo()`
- [x] `src/protocol.rs` — `GeoMessageRequest` + `GeoMeshAvailable` MetaMessage variants
- [ ] `src/storage.rs` — schema migration + geo queries (geo queries implemented; schema migration deferred)
- [x] `src/message_handler.rs` — geo mesh protocol arms; geo extraction on ingest
- [x] `src/identity.rs` — `[geo]` config section
- [x] `src/web_client/handlers/messages.rs` — `geo` field in POST, `geo_within` in GET
- [x] `src/web_client/handlers/settings.rs` — `GET/POST /api/settings/geo`
- [x] `src/web_client/config.rs` — geo constants
- [x] `src/web_client/sync.rs` — send `GeoMessageRequest` after sync
- [x] `src/bin/tenet-crypto.rs` — `geo set` / `geo show` subcommands
- [ ] `src/simulation/config.rs` — `geo` field in peer/cohort config
- [ ] `src/client.rs` (SimulationClient) — handle `GeoMessageRequest`/`GeoMeshAvailable`
- [ ] `src/bin/debugger.rs` — `geo-query` and `set-geo` REPL commands
- [ ] `android/tenet-ffi/src/types.rs` — `FfiGeoLocation`
- [ ] `android/tenet-ffi/src/lib.rs` — `resolve_geo()`, `FfiMessage.geo`
- [ ] `android/tenet-ffi/src/tenet_ffi.udl` — UniFFI interface update
- [ ] Tests passing (core + bin tests pass; `tests/attachment_v2_tests.rs` fails in sandbox due relay bind permission)

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

## Dependency Analysis

Three new Rust crates are needed. All are offline — no external HTTP calls at
runtime.

### `geohash` (encode/decode)

```toml
geohash = "0.13"
```

Encodes `(lat, lon)` to a base-32 geohash string and decodes back to a
bounding box. No dependencies. Used throughout `src/geo.rs` for all
coordinate-to-geohash conversions.

```rust
use geohash::{encode, decode, Coord};
let hash = encode(Coord { x: -122.419, y: 37.774 }, 5)?; // "9q8yy"
let (coord, _, _) = decode("9q8yy")?;
```

### `country-boundaries` (offline lat/lon → country code)

```toml
country-boundaries = "0.7"
```

Given a `(lat, lon)`, returns the ISO 3166-1 alpha-2 country code and ISO
3166-2 subdivision code. Data is derived from OpenStreetMap and embedded in the
binary (~2 MB). Licensed ODbL — attribution required in any distribution that
includes the dataset. Used in `src/geo.rs:resolve_geo()` to populate
`geo_country` when a geohash-precision post is submitted.

```rust
use country_boundaries::{CountryBoundaries, LatLon};
let boundaries = CountryBoundaries::from_reader(BOUNDARIES_DATA)?;
let ids = boundaries.ids(LatLon::new(37.774, -122.419)?);
// ids → ["US-CA", "US"]
```

The subdivision code (`"US-CA"`) maps to the region/state. Country code is
the two-letter part before the hyphen.

### `reverse_geocoder` (offline lat/lon → city + country)

```toml
reverse_geocoder = "3"
```

Finds the nearest populated place to a `(lat, lon)` using an embedded GeoNames
dataset (~1 MB). Returns city name, country code, and admin region. Used in
`src/geo.rs:resolve_geo()` to populate `geo_city` and `geo_region` for
geohash-precision posts. Licensed MIT.

```rust
use reverse_geocoder::{ReverseGeocoder, Locations};
let locations = Locations::from_memory();
let geocoder = ReverseGeocoder::new(&locations);
let result = geocoder.search((37.774, -122.419));
// result.record: city="San Francisco", admin1="California", cc="US"
```

### `iso3166-1` (country code validation + seeded list)

```toml
iso3166-1 = "0.1"
```

Provides a complete static list of ISO 3166-1 alpha-2 country codes with full
country names. Used for (1) validating `country_code` in incoming `GeoLocation`
payloads, and (2) powering the country picker in all clients.

```rust
use iso3166_1::all;
let countries = all(); // Vec<Country> — sorted, complete list
let valid = iso3166_1::alpha2("US").is_some();
```

### Binary size impact

| Crate | Embedded data | Approx size added |
|-------|--------------|-------------------|
| `geohash` | none | < 50 KB |
| `country-boundaries` | OSM boundaries | ~2 MB |
| `reverse_geocoder` | GeoNames cities | ~1 MB |
| `iso3166-1` | country list | < 20 KB |

The ~3 MB increase is acceptable for the relay and web-client binaries. For the
Android FFI crate, both datasets are compiled in; the APK increase is similar.

---

## Privacy Model

Location precision is controlled at two levels:

| Level | Scope | Overrides |
|-------|-------|-----------|
| **Identity default** | Applies to all posts from this identity | Per-post override |
| **Per-post** | Applies to a single post | — |

A per-post setting always wins over the identity default.

### Precision Levels

| Precision | Stored fields | Typical radius | Geohash length |
|-----------|---------------|----------------|----------------|
| `none` | — | — | — |
| `country` | `country_code` | ~continent | — |
| `region` | `country_code`, `region` | ~500 km | — |
| `city` | `country_code`, `region`, `city` | ~20 km | — |
| `neighborhood` | all text + `geohash` (5 chars) | ~5 km | 5 |
| `exact` | all text + `geohash` (9 chars) + `lat`/`lon` | ~5 m | 9 |

`none` emits no location fields. Any post with `precision = none` is treated as
global and excluded from geographic mesh responses.

**Recommended default:** `city`. It enables local discovery without exposing
neighbourhood or street-level information.

**`exact` is discouraged** for routine social posts and SHOULD trigger a UI
warning. It should only be used when precise location is the content itself
(e.g., a lost-and-found notice, a meet-up pin).

### Hierarchical containment

This is the core rule that makes cross-precision discovery work: **every
geo-tagged post stores the complete place hierarchy regardless of precision.**

When a post is submitted with neighborhood or exact precision, the server
reverse-geocodes the coordinates to fill in `geo_country`, `geo_region`, and
`geo_city` automatically. A peer querying for country `"US"` will therefore
find posts at *any* precision level that fall within the US — country, region,
city, neighborhood, and exact — because all of them carry `geo_country = "US"`.

```
Post precision   → stored fields
─────────────────────────────────────────────────────────────
country          → geo_country
region           → geo_country, geo_region
city             → geo_country, geo_region, geo_city
neighborhood     → geo_country, geo_region, geo_city, geo_geohash
exact            → geo_country, geo_region, geo_city, geo_geohash, geo_lat, geo_lon
```

Query matching (in `GeoMessageRequest`):

| Query filter | SQL condition | Matches |
|---|---|---|
| `country_code = "US"` | `geo_country = 'US'` | all 5 precisions |
| `country_code + region` | `geo_country = 'US' AND geo_region = 'California'` | region, city, neighborhood, exact |
| `country_code + region + city` | `geo_country = 'US' AND geo_region = 'California' AND geo_city = 'San Francisco'` | city, neighborhood, exact |
| `geohash_prefix = "9q8y"` | `geo_geohash LIKE '9q8y%'` | neighborhood, exact only |

Geohash prefix queries and text-based queries are separate dimensions.
A city-precision post has no geohash and will not be returned for a geohash
query; a neighborhood-precision post has all text fields and *will* be returned
for a city-level text query.

---

## `src/geo.rs` — New Module

All geo logic lives in a new top-level module `src/geo.rs`, exported via
`lib.rs`. This avoids scattering coordinate-handling code across modules.

```rust
pub mod geo {
    pub struct GeoLocation { ... }
    pub enum GeoPrecision { ... }

    /// Resolve a lat/lon to a fully-populated GeoLocation at the given precision.
    /// Uses embedded country-boundaries and reverse_geocoder datasets.
    /// Returns an error if the coordinates are out of range or no match found.
    pub fn resolve_geo(lat: f64, lon: f64, precision: GeoPrecision)
        -> Result<GeoLocation, GeoError>;

    /// Encode lat/lon to a geohash string at the given character length.
    pub fn encode_geohash(lat: f64, lon: f64, len: usize)
        -> Result<String, GeoError>;

    /// Decode a geohash string to a bounding-box centre (lat, lon).
    pub fn decode_geohash(hash: &str) -> Result<(f64, f64), GeoError>;

    /// Validate that a country_code is a known ISO 3166-1 alpha-2 code.
    pub fn is_valid_country_code(code: &str) -> bool;

    /// Return the full seeded list of (alpha2, name) pairs, sorted by name.
    pub fn country_list() -> &'static [(& 'static str, &'static str)];
}
```

`resolve_geo()` does the heavy lifting:

1. Validate lat ∈ [−90, 90], lon ∈ [−180, 180).
2. Call `country-boundaries` to get country + subdivision codes.
3. Call `reverse_geocoder` to get city and admin-region name.
4. Call `geohash::encode` at the appropriate length for the requested precision.
5. Assemble and return `GeoLocation` with all fields populated.

### `GeoLocation` struct

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    pub precision: GeoPrecision,

    /// ISO 3166-1 alpha-2 country code. Present for all precisions except none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,

    /// Region/state name (human-readable). Present for region, city,
    /// neighborhood, exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// City name (human-readable). Present for city, neighborhood, exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,

    /// Base-32 geohash string. Present for neighborhood (5 chars) and
    /// exact (9 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geohash: Option<String>,

    /// Latitude in decimal degrees. Present only for exact precision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,

    /// Longitude in decimal degrees. Present only for exact precision.
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
public-message JSON payload. No changes to `Envelope` or `Header` are required;
the location rides inside the signed payload.

```jsonc
// City precision
{
  "body": "Farmers market is packed today!",
  "geo": {
    "precision": "city",
    "country_code": "US",
    "region": "California",
    "city": "San Francisco"
  }
}

// Neighborhood precision (server filled country/region/city via resolve_geo)
{
  "body": "Rain starting on the east side.",
  "geo": {
    "precision": "neighborhood",
    "country_code": "US",
    "region": "California",
    "city": "San Francisco",
    "geohash": "9q8yy"
  }
}
```

### Validation rules

- `country_code` MUST be a 2-character ISO 3166-1 alpha-2 code (validated
  against the embedded country list).
- `geohash` characters MUST be in the base-32 alphabet `[0-9b-hj-np-z]`.
- For `neighborhood` precision, `geohash` MUST be exactly 5 characters.
- For `exact` precision, `geohash` MUST be exactly 9 characters, and `lat`/`lon`
  MUST be present and in range.
- If `precision` is `none`, all other fields MUST be absent.
- Receivers MUST silently skip posts with invalid geo payloads rather than
  failing the entire message.

---

## Configuration

### Identity default (`config.toml`)

A new `[geo]` section is added to each identity's `config.toml`:

```toml
[geo]
# Default precision for public posts.
# Options: none, country, region, city, neighborhood, exact
# Default when section is absent: "city"
default_precision = "city"

# Home location — used to auto-fill geo when posting.
# Leave empty for none/country precision.
country_code = "US"
region = "California"
city = "San Francisco"
# geohash and lat/lon derived server-side via resolve_geo() when needed
```

`src/identity.rs` gains a `GeoConfig` struct (parallel to how other config
sections are handled) and falls back to sensible defaults if the section is
absent.

### Per-post override

`POST /api/messages/public` accepts an optional `geo` object:

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

- If `geo` is **absent** → use identity `default_precision` + stored location.
- If `geo` is **`null`** → attach no location for this post (explicit one-post opt-out).
- If `geo` is **present** → validate and use as provided.

For neighborhood/exact precision, the client may provide either the full
hierarchy or just `lat`/`lon`; the server calls `resolve_geo()` to fill any
missing text fields and compute the geohash.

---

## Storage Schema

### Migration: `messages` table

```sql
ALTER TABLE messages ADD COLUMN geo_precision  TEXT;
ALTER TABLE messages ADD COLUMN geo_country    TEXT;
ALTER TABLE messages ADD COLUMN geo_region     TEXT;
ALTER TABLE messages ADD COLUMN geo_city       TEXT;
ALTER TABLE messages ADD COLUMN geo_geohash    TEXT;
ALTER TABLE messages ADD COLUMN geo_lat        REAL;
ALTER TABLE messages ADD COLUMN geo_lon        REAL;
```

All columns nullable. Existing rows retain `NULL` (treated as `none`).

Indexes:

```sql
-- Prefix-scan for geohash queries
CREATE INDEX IF NOT EXISTS idx_messages_geo_geohash
    ON messages (geo_geohash)
    WHERE geo_geohash IS NOT NULL;

-- Equality filter for text-based queries
CREATE INDEX IF NOT EXISTS idx_messages_geo_country
    ON messages (geo_country, geo_region, geo_city)
    WHERE geo_country IS NOT NULL;
```

### New storage queries

```rust
/// IDs of public messages since `since` whose geohash starts with `prefix`.
pub fn list_public_message_ids_since_in_geohash_region(
    &self,
    since: u64,
    geohash_prefix: &str,
    limit: u32,
) -> Result<Vec<String>, StorageError>;

/// IDs of public messages since `since` matching the text-based region.
/// Each provided field is ANDed; None means "any".
pub fn list_public_message_ids_since_in_named_region(
    &self,
    since: u64,
    country_code: Option<&str>,
    region: Option<&str>,
    city: Option<&str>,
    limit: u32,
) -> Result<Vec<String>, StorageError>;
```

The existing `list_public_message_ids_since` is unchanged and used for global
(non-geo) mesh requests.

---

## Protocol Changes

### New `MetaMessage` variants

```rust
/// A asks B for public messages within a geographic region since a timestamp.
/// Use either geohash_prefix OR the country/region/city text fields, not both.
GeoMessageRequest {
    peer_id: String,
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

/// Response to GeoMessageRequest: matching public message IDs.
/// Echoes the filter fields from the request.
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

After `GeoMeshAvailable`, the existing `MeshRequest` / `MeshDelivery` phases 3
and 4 are reused unchanged.

### Extended flow

```
A                                  B
|                                  |
|-- GeoMessageRequest (region) -->|   Phase 1-geo: Geographic query
|<-- GeoMeshAvailable (ids) ------|   Phase 2-geo: Matching IDs
|                                  |
| (dedup against local DB)         |
|                                  |
|-- MeshRequest (ids) ----------->|   Phase 3: unchanged
|<-- MeshDelivery (envs) ---------|   Phase 4: unchanged
```

A peer MAY send a global `MessageRequest` and a `GeoMessageRequest` to the same
peer in the same sync cycle. Deduplication of IDs is applied across both before
sending `MeshRequest`.

### Mesh query region derivation

The region sent in `GeoMessageRequest` comes from the identity's `[geo]` config:

| `default_precision` | Fields sent in `GeoMessageRequest` |
|---------------------|-------------------------------------|
| `none` | — (no geo query sent) |
| `country` | `country_code` |
| `region` | `country_code`, `region` |
| `city` | `country_code`, `region`, `city` |
| `neighborhood` | `geohash_prefix` (5 chars from stored geohash) |
| `exact` | `geohash_prefix` (5 chars, truncated for query privacy) |

### Responder behaviour

1. Cap `since_timestamp`: `since = max(since_timestamp, now - MAX_MESH_WINDOW_SECS)`.
2. If `geohash_prefix` is set → call `list_public_message_ids_since_in_geohash_region`.
3. Otherwise → call `list_public_message_ids_since_in_named_region`.
4. Respond with `GeoMeshAvailable` (up to `MAX_MESH_IDS` IDs), echoing filter fields.
5. Respond with empty `message_ids` if no match (do not skip the response).

---

## `src/message_handler.rs` Changes

### New `on_meta` arms

```rust
MetaMessage::GeoMessageRequest { peer_id, since_timestamp,
                                 geohash_prefix, country_code, region, city } => {
    // 1. Cap since_timestamp.
    // 2. Query geohash or named-region storage function based on which is set.
    // 3. Return GeoMeshAvailable envelope addressed to peer_id.
}

MetaMessage::GeoMeshAvailable { peer_id, message_ids, .. } => {
    // 1. Dedup message_ids against local DB.
    // 2. If unknowns exist: send MeshRequest (reuses existing path).
}
```

### Geo extraction on ingest

In `on_message` for `MessageKind::Public`, extract `geo` from the payload JSON
and write to the `geo_*` columns when inserting:

```rust
let geo: Option<GeoLocation> = payload_json
    .get("geo")
    .and_then(|v| serde_json::from_value(v.clone()).ok());
// Validate and write geo_precision, geo_country, geo_region, geo_city,
// geo_geohash, geo_lat, geo_lon into the insert statement.
```

---

## Web Client Changes (`src/web_client/`)

### `POST /api/messages/public`

Extended request body:

```jsonc
{
  "body": "Hello world",
  "attachments": [...],
  // Optional. Absent = use identity default. null = no location.
  "geo": {
    "precision": "city",
    "country_code": "US",
    "region": "California",
    "city": "San Francisco"
    // OR for finer precision: provide lat/lon and server resolves rest
    // "precision": "neighborhood",
    // "lat": 37.7749,
    // "lon": -122.4194
  }
}
```

Handler logic in `handlers/messages.rs`:

1. Parse optional `geo` field.
2. If absent → load identity `GeoConfig`; build `GeoLocation` from stored fields.
3. If `null` → no geo.
4. If present and precision is neighborhood/exact and geohash is absent →
   call `geo::resolve_geo(lat, lon, precision)` to fill all fields.
5. Validate the final `GeoLocation` (country code, geohash length, etc.).
6. Embed as `"geo"` key in plaintext payload JSON before building the envelope.

### `GET /api/messages`

New optional query parameter:

```
GET /api/messages?kind=public&geo_within=9q8y     # geohash prefix
GET /api/messages?kind=public&geo_country=US      # country filter
GET /api/messages?kind=public&geo_country=US&geo_region=California&geo_city=San+Francisco
```

When any `geo_*` query param is present, call the appropriate storage query.
Multiple text params are ANDed. `geo_within` (geohash) and text params are
mutually exclusive; return 400 if both are provided.

### `GET /api/geo/countries`

New endpoint that returns the seeded country list for client-side pickers:

```json
[
  { "alpha2": "AU", "name": "Australia" },
  { "alpha2": "DE", "name": "Germany" },
  { "alpha2": "US", "name": "United States of America" }
]
```

Sourced from `geo::country_list()` (static, no DB query needed).

### `POST /api/geo/resolve`

Accepts coordinates and returns a fully-resolved `GeoLocation`. Lets clients
(web UI, Android) delegate reverse-geocoding to the server:

```jsonc
// Request
{ "lat": 37.7749, "lon": -122.4194, "precision": "neighborhood" }

// Response
{
  "precision": "neighborhood",
  "country_code": "US",
  "region": "California",
  "city": "San Francisco",
  "geohash": "9q8yy"
}
```

### `GET/POST /api/settings/geo`

New handler in `handlers/settings.rs`:

```
GET /api/settings/geo
→ { "default_precision": "city", "country_code": "US",
    "region": "California", "city": "San Francisco" }

POST /api/settings/geo
← { "default_precision": "city", "country_code": "US",
    "region": "California", "city": "San Francisco",
    "lat": 37.7749, "lon": -122.4194 }   // lat/lon optional; triggers resolve_geo
→ updated settings object
```

Writes the `[geo]` section of `config.toml` and returns the new state.
If `lat`/`lon` are provided, calls `resolve_geo()` at the requested precision
and stores the result; raw lat/lon are not persisted.

### `src/web_client/sync.rs`

After each successful sync, `send_mesh_queries` sends:

- Existing global `MessageRequest` per peer (unchanged).
- One `GeoMessageRequest` per peer if `default_precision != none`, using the
  region derived from the identity's `[geo]` config.

---

## CLI Changes (`src/bin/tenet-crypto.rs`)

Two new subcommands under a `geo` group:

### `tenet geo show`

Prints the current identity's geo settings:

```
Default precision : city
Country           : US
Region            : California
City              : San Francisco
```

### `tenet geo set`

Configure the identity's geo defaults. Accepts either text fields or
coordinates (which are resolved server-side):

```bash
# Text-based (city precision)
tenet geo set --precision city --country US --region California --city "San Francisco"

# Coordinate-based (neighborhood; server resolves name)
tenet geo set --precision neighborhood --lat 37.7749 --lon -122.4194

# Disable geo by default
tenet geo set --precision none
```

### Per-post flag

Existing `tenet post` command gains a `--geo` flag (overrides default):

```bash
tenet post "Hello from Portland" --geo city --country US --region Oregon --city Portland
tenet post "Private message, no location" --no-geo
```

---

## Android FFI Changes

### `android/tenet-ffi/src/types.rs`

New FFI-safe geo type:

```rust
#[derive(Debug, Clone)]
pub struct FfiGeoLocation {
    pub precision: String,        // "none"|"country"|"region"|"city"|"neighborhood"|"exact"
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub geohash: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}
```

`FfiMessage` gains:

```rust
pub struct FfiMessage {
    // ... existing fields ...
    pub geo: Option<FfiGeoLocation>,
}
```

### `android/tenet-ffi/src/lib.rs`

New methods on `TenetClient`:

```rust
/// Resolve lat/lon to a GeoLocation at the given precision.
/// Performs offline reverse geocoding. Safe to call on IO thread.
fn resolve_geo(
    &self,
    lat: f64,
    lon: f64,
    precision: String,
) -> Result<FfiGeoLocation, TenetError>;

/// Get current geo settings for this identity.
fn get_geo_settings(&self) -> Result<FfiGeoSettings, TenetError>;

/// Update geo settings. lat/lon optional; if provided, resolve_geo is called.
fn set_geo_settings(
    &self,
    settings: FfiGeoSettings,
) -> Result<FfiGeoSettings, TenetError>;

/// Return the seeded country list: vec of (alpha2, name) pairs.
fn list_countries(&self) -> Vec<FfiCountry>;
```

`send_public_message()` gains an optional `geo: Option<FfiGeoLocation>` param.

### `android/tenet-ffi/src/tenet_ffi.udl`

```
dictionary FfiGeoLocation {
    string precision;
    string? country_code;
    string? region;
    string? city;
    string? geohash;
    double? lat;
    double? lon;
};

dictionary FfiGeoSettings {
    string default_precision;
    string? country_code;
    string? region;
    string? city;
};

dictionary FfiCountry {
    string alpha2;
    string name;
};
```

Add `FfiGeoLocation? geo` to the existing `FfiMessage` dictionary.

### Android UI (`app/src/main/java/com/example/tenet/`)

- **`ui/compose/`**: Add a location button to the compose screen. Tapping it
  calls the Android Fused Location Provider to get lat/lon, then calls
  `resolve_geo()` via the FFI. Shows a chip displaying the resolved place name
  with a precision selector. A second tap opens a precision picker or removes
  the location.
- **`ui/settings/`**: New `GeoSettingsScreen` with country/region/city pickers
  (backed by `list_countries()`) and a precision selector.
- **`ui/timeline/`**: Geo filter chip on the timeline header. Selecting a
  region calls `list_messages` with geo filter params.

---

## Simulation Changes

### `src/simulation/config.rs`

Add an optional `geo` field to cohort and node configuration:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimGeoConfig {
    /// Geohash of the peer's simulated location (5 chars = neighborhood).
    pub geohash: String,
    /// ISO 3166-1 alpha-2 country code for the simulated location.
    pub country_code: String,
    pub region: Option<String>,
    pub city: Option<String>,
    /// Precision used when posting.
    #[serde(default = "default_precision")]
    pub post_precision: String, // "city" | "neighborhood" | etc.
    /// Precision used when sending GeoMessageRequest.
    #[serde(default = "default_precision")]
    pub query_precision: String,
}
```

`OnlineCohortDefinition` variants gain `geo: Option<SimGeoConfig>`.
Individual node overrides can be added in an `overrides` map.

Example scenario snippet:

```toml
[[cohorts]]
name = "sf_locals"
node_ids = ["alice", "bob", "carol"]

[cohorts.geo]
geohash = "9q8yy"
country_code = "US"
region = "California"
city = "San Francisco"
post_precision = "neighborhood"
query_precision = "city"

[[cohorts]]
name = "london_peers"
node_ids = ["dave", "eve"]

[cohorts.geo]
geohash = "gcpvu"
country_code = "GB"
region = "England"
city = "London"
post_precision = "city"
query_precision = "country"
```

### `src/client.rs` (SimulationClient)

`SimulationClient` already implements a gossip path for `MessageRequest` /
`MeshAvailable` / `MeshRequest` / `MeshDelivery` using its
`public_message_cache`. Extend with:

- `geo_config: Option<SimGeoConfig>` field on `SimulationClient`.
- In `handle_message_request`-style logic, add a branch for `GeoMessageRequest`
  that filters the `public_message_cache` by the stored `geo_*` fields of each
  cached message.
- `GeoMeshAvailable` handling: mirrors `MeshAvailable` but filters to
  the geo-matching subset.
- When building a public message, attach `GeoLocation` from `geo_config` at the
  configured `post_precision`.

This enables simulation scenarios that test whether geo-filtered mesh
distribution correctly confines local messages to local peers.

---

## Debugger Changes (`src/bin/debugger.rs`)

Two new REPL commands, following the pattern of `mesh-query <peer-a> <peer-b>`:

### `geo-query <peer-a> <peer-b> <region>`

Drives the geo-scoped four-phase mesh exchange interactively and prints each
step:

```
> geo-query alice bob city:US/California/San Francisco

Phase 1: alice → bob  GeoMessageRequest { since=1709000000, country="US", region="California", city="San Francisco" }
Phase 2: bob → alice  GeoMeshAvailable  { 12 IDs }
  Dedup: alice has 9, requesting 3
Phase 3: alice → bob  MeshRequest       { 3 IDs }
Phase 4: bob → alice  MeshDelivery      { 3 envelopes }
  Stored 3 new messages for alice.
```

`<region>` accepts:
- `country:<code>` — e.g., `country:US`
- `region:<code>/<region>` — e.g., `region:US/California`
- `city:<code>/<region>/<city>` — e.g., `city:US/California/San Francisco`
- `geohash:<prefix>` — e.g., `geohash:9q8y`

### `set-geo <peer> <precision> [<lat> <lon>]`

Sets a simulated peer's location in the debugger session:

```
> set-geo alice neighborhood 37.7749 -122.4194
alice geo set: neighborhood / US / California / San Francisco / 9q8yy

> set-geo bob city US California London
Error: "London" is not a city in California. Use lat/lon or check spelling.
```

---

## File Change Summary

| File | Change |
|------|--------|
| `Cargo.toml` | Add `geohash`, `country-boundaries`, `reverse_geocoder`, `iso3166-1` |
| `src/geo.rs` | New module: `GeoLocation`, `GeoPrecision`, `resolve_geo()`, `encode_geohash()`, `is_valid_country_code()`, `country_list()` |
| `src/lib.rs` | Export `pub mod geo` |
| `src/protocol.rs` | `GeoMessageRequest` + `GeoMeshAvailable` MetaMessage variants |
| `src/storage.rs` | 7 new geo columns + 2 indexes; `list_public_message_ids_since_in_geohash_region`; `list_public_message_ids_since_in_named_region`; geo fields in `insert_message` |
| `src/message_handler.rs` | `GeoMessageRequest` + `GeoMeshAvailable` arms; geo extraction on ingest |
| `src/identity.rs` | `GeoConfig` struct; read/write `[geo]` section of `config.toml` |
| `src/web_client/handlers/messages.rs` | `geo` field in POST; `geo_within`/`geo_country`/`geo_region`/`geo_city` params in GET |
| `src/web_client/handlers/settings.rs` | `GET/POST /api/settings/geo` |
| `src/web_client/router.rs` | Register `/api/settings/geo`, `/api/geo/countries`, `/api/geo/resolve` routes |
| `src/web_client/config.rs` | Geo constants (`MAX_GEO_GEOHASH_PREFIX_LEN`, etc.) |
| `src/web_client/sync.rs` | Send `GeoMessageRequest` after sync when `default_precision != none` |
| `src/bin/tenet-crypto.rs` | `geo show` + `geo set` subcommands; `--geo`/`--no-geo` flag on `post` |
| `src/simulation/config.rs` | `SimGeoConfig`; `geo` field on cohort config |
| `src/client.rs` | `SimulationClient` geo field; `GeoMessageRequest`/`GeoMeshAvailable` handling; geo attachment in public post builder |
| `src/bin/debugger.rs` | `geo-query` + `set-geo` REPL commands |
| `android/tenet-ffi/src/types.rs` | `FfiGeoLocation`, `FfiGeoSettings`, `FfiCountry`; `FfiMessage.geo` |
| `android/tenet-ffi/src/lib.rs` | `resolve_geo()`, `get_geo_settings()`, `set_geo_settings()`, `list_countries()`; geo param on `send_public_message()` |
| `android/tenet-ffi/src/tenet_ffi.udl` | UDL entries for all new types + methods |

---

## Out of Scope (for now)

- **Forward geocoding** (address/place-name → coordinates): remains a client
  responsibility. The `POST /api/geo/resolve` endpoint only does reverse
  geocoding (coordinates → place name).
- **Multiple region subscriptions**: only a single home region per identity.
  Subscribing to multiple regions (home + work, travel mode) is future work.
- **Relay-side geo filtering**: the relay is untrusted and does no geo
  indexing. All filtering happens on the receiving client.
- **UI map view**: a geographic map of nearby posts in the web SPA is a UX
  feature tracked separately.
- **ODbL attribution UI**: the `country-boundaries` dataset requires ODbL
  attribution in any distributed binary. The "About" screen of each client
  must include the attribution text. Implementation tracked separately from the
  protocol work.
