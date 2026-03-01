//! Geographic metadata helpers for public posts and mesh queries.

use serde::{Deserialize, Serialize};

/// Geographic precision for location sharing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoPrecision {
    None,
    Country,
    Region,
    City,
    Neighborhood,
    Exact,
}

impl std::fmt::Display for GeoPrecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            GeoPrecision::None => "none",
            GeoPrecision::Country => "country",
            GeoPrecision::Region => "region",
            GeoPrecision::City => "city",
            GeoPrecision::Neighborhood => "neighborhood",
            GeoPrecision::Exact => "exact",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for GeoPrecision {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "none" => Ok(GeoPrecision::None),
            "country" => Ok(GeoPrecision::Country),
            "region" => Ok(GeoPrecision::Region),
            "city" => Ok(GeoPrecision::City),
            "neighborhood" => Ok(GeoPrecision::Neighborhood),
            "exact" => Ok(GeoPrecision::Exact),
            _ => Err(format!("invalid geo precision: {s}")),
        }
    }
}

/// Optional geo metadata attached to a public post.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    pub precision: GeoPrecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geohash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
}

/// Identity-level default geo settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoConfig {
    pub default_precision: GeoPrecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geohash: Option<String>,
}

impl Default for GeoConfig {
    fn default() -> Self {
        Self {
            default_precision: GeoPrecision::None,
            country_code: None,
            region: None,
            city: None,
            geohash: None,
        }
    }
}

impl GeoConfig {
    /// Build a default per-post geo payload from identity settings.
    pub fn to_location(&self) -> Option<GeoLocation> {
        if self.default_precision == GeoPrecision::None {
            return None;
        }
        Some(GeoLocation {
            precision: self.default_precision.clone(),
            country_code: self.country_code.clone(),
            region: self.region.clone(),
            city: self.city.clone(),
            geohash: self.geohash.clone(),
            lat: None,
            lon: None,
        })
    }
}

/// JSON body for public post payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicMessageBody {
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geo: Option<GeoLocation>,
}

impl PublicMessageBody {
    pub fn to_payload_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

pub fn parse_public_message_body(payload_body: &str) -> Result<PublicMessageBody, String> {
    match serde_json::from_str(payload_body) {
        Ok(v) => Ok(v),
        Err(_) => Ok(PublicMessageBody {
            body: payload_body.to_string(),
            geo: None,
        }),
    }
}

pub fn validate_geo_location(geo: &GeoLocation) -> Result<(), String> {
    if let Some(code) = &geo.country_code {
        if !is_valid_country_code(code) {
            return Err("country_code must be a 2-letter uppercase code".to_string());
        }
    }
    if let Some(hash) = &geo.geohash {
        if !is_valid_geohash(hash) {
            return Err("geohash has invalid characters".to_string());
        }
    }
    if let Some(lat) = geo.lat {
        if !(-90.0..=90.0).contains(&lat) {
            return Err("lat out of range".to_string());
        }
    }
    if let Some(lon) = geo.lon {
        if !(-180.0..=180.0).contains(&lon) {
            return Err("lon out of range".to_string());
        }
    }

    match geo.precision {
        GeoPrecision::None => {
            if geo.country_code.is_some()
                || geo.region.is_some()
                || geo.city.is_some()
                || geo.geohash.is_some()
                || geo.lat.is_some()
                || geo.lon.is_some()
            {
                return Err("precision none must not include geo fields".to_string());
            }
        }
        GeoPrecision::Country => {
            if geo.country_code.is_none() {
                return Err("country precision requires country_code".to_string());
            }
        }
        GeoPrecision::Region => {
            if geo.country_code.is_none() || geo.region.is_none() {
                return Err("region precision requires country_code and region".to_string());
            }
        }
        GeoPrecision::City => {
            if geo.country_code.is_none() || geo.region.is_none() || geo.city.is_none() {
                return Err("city precision requires country_code, region, and city".to_string());
            }
        }
        GeoPrecision::Neighborhood => {
            if geo.country_code.is_none() || geo.region.is_none() || geo.city.is_none() {
                return Err(
                    "neighborhood precision requires country_code, region, and city".to_string(),
                );
            }
            let hash = geo
                .geohash
                .as_deref()
                .ok_or_else(|| "neighborhood precision requires geohash".to_string())?;
            if hash.len() != 5 {
                return Err("neighborhood geohash must be 5 characters".to_string());
            }
        }
        GeoPrecision::Exact => {
            if geo.country_code.is_none() || geo.region.is_none() || geo.city.is_none() {
                return Err("exact precision requires country_code, region, and city".to_string());
            }
            let hash = geo
                .geohash
                .as_deref()
                .ok_or_else(|| "exact precision requires geohash".to_string())?;
            if hash.len() != 9 {
                return Err("exact geohash must be 9 characters".to_string());
            }
            if geo.lat.is_none() || geo.lon.is_none() {
                return Err("exact precision requires lat and lon".to_string());
            }
        }
    }

    Ok(())
}

pub fn is_valid_country_code(code: &str) -> bool {
    code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase())
}

pub fn is_valid_geohash(hash: &str) -> bool {
    !hash.is_empty()
        && hash
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'b'..='h' | 'j'..='k' | 'm'..='n' | 'p'..='z'))
}

/// Very small built-in list used by settings pickers in phase 1.
pub fn country_list() -> &'static [(&'static str, &'static str)] {
    &[
        ("AU", "Australia"),
        ("CA", "Canada"),
        ("DE", "Germany"),
        ("FR", "France"),
        ("GB", "United Kingdom"),
        ("IN", "India"),
        ("JP", "Japan"),
        ("US", "United States of America"),
    ]
}

#[derive(Debug, Clone, Default)]
pub struct GeoQuery {
    pub geohash_prefix: Option<String>,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

pub fn matches_geo_query(geo: Option<&GeoLocation>, query: &GeoQuery) -> bool {
    let Some(geo) = geo else {
        return false;
    };
    if let Some(prefix) = query.geohash_prefix.as_deref() {
        return geo
            .geohash
            .as_deref()
            .map(|g| g.starts_with(prefix))
            .unwrap_or(false);
    }
    if let Some(code) = query.country_code.as_deref() {
        if geo.country_code.as_deref() != Some(code) {
            return false;
        }
    }
    if let Some(region) = query.region.as_deref() {
        if geo.region.as_deref() != Some(region) {
            return false;
        }
    }
    if let Some(city) = query.city.as_deref() {
        if geo.city.as_deref() != Some(city) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_payload_roundtrip() {
        let payload = PublicMessageBody {
            body: "hello".to_string(),
            geo: Some(GeoLocation {
                precision: GeoPrecision::City,
                country_code: Some("US".to_string()),
                region: Some("California".to_string()),
                city: Some("San Francisco".to_string()),
                geohash: None,
                lat: None,
                lon: None,
            }),
        };
        let json = payload.to_payload_json().unwrap();
        let parsed = parse_public_message_body(&json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn validates_exact_geo() {
        let ok = GeoLocation {
            precision: GeoPrecision::Exact,
            country_code: Some("US".to_string()),
            region: Some("California".to_string()),
            city: Some("San Francisco".to_string()),
            geohash: Some("9q8yyk8yt".to_string()),
            lat: Some(37.7749),
            lon: Some(-122.4194),
        };
        assert!(validate_geo_location(&ok).is_ok());
    }

    #[test]
    fn rejects_bad_geo() {
        let bad = GeoLocation {
            precision: GeoPrecision::Neighborhood,
            country_code: Some("US".to_string()),
            region: Some("California".to_string()),
            city: Some("San Francisco".to_string()),
            geohash: Some("9q8y".to_string()),
            lat: None,
            lon: None,
        };
        assert!(validate_geo_location(&bad).is_err());
    }
}
