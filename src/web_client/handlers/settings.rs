//! Geo settings endpoints.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::geo::{country_list, validate_geo_location, GeoConfig, GeoLocation, GeoPrecision};
use crate::identity::{load_identity_geo_config, save_identity_geo_config};
use crate::web_client::state::SharedState;
use crate::web_client::utils::api_error;

pub async fn get_geo_settings_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    let Some(identity_dir) = st.db_path.parent() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid identity directory",
        );
    };
    match load_identity_geo_config(identity_dir) {
        Ok(cfg) => (StatusCode::OK, axum::Json(serde_json::json!(cfg))).into_response(),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load settings: {e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateGeoSettingsRequest {
    default_precision: GeoPrecision,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    geohash: Option<String>,
}

pub async fn update_geo_settings_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<UpdateGeoSettingsRequest>,
) -> Response {
    let cfg = GeoConfig {
        default_precision: req.default_precision.clone(),
        country_code: req.country_code.clone(),
        region: req.region.clone(),
        city: req.city.clone(),
        geohash: req.geohash.clone(),
    };
    if let Some(loc) = cfg.to_location() {
        if let Err(e) = validate_geo_location(&loc) {
            return api_error(StatusCode::BAD_REQUEST, e);
        }
    }

    let st = state.lock().await;
    let Some(identity_dir) = st.db_path.parent() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid identity directory",
        );
    };
    if let Err(e) = save_identity_geo_config(identity_dir, &cfg) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to save settings: {e}"),
        );
    }
    (StatusCode::OK, axum::Json(serde_json::json!(cfg))).into_response()
}

pub async fn list_countries_handler() -> Response {
    let countries: Vec<serde_json::Value> = country_list()
        .iter()
        .map(|(alpha2, name)| serde_json::json!({ "alpha2": alpha2, "name": name }))
        .collect();
    (StatusCode::OK, axum::Json(serde_json::json!(countries))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ResolveGeoRequest {
    lat: f64,
    lon: f64,
    precision: GeoPrecision,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    geohash: Option<String>,
}

pub async fn resolve_geo_handler(axum::Json(req): axum::Json<ResolveGeoRequest>) -> Response {
    let loc = GeoLocation {
        precision: req.precision,
        country_code: req.country_code,
        region: req.region,
        city: req.city,
        geohash: req.geohash,
        lat: Some(req.lat),
        lon: Some(req.lon),
    };
    if let Err(e) = validate_geo_location(&loc) {
        return api_error(StatusCode::BAD_REQUEST, e);
    }
    (StatusCode::OK, axum::Json(serde_json::json!(loc))).into_response()
}
