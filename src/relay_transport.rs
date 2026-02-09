//! Client-side relay transport helpers.
//!
//! These functions encapsulate the HTTP operations that any tenet client needs
//! for communicating with relay servers: posting envelopes and fetching inboxes.

use crate::protocol::Envelope;

/// Post an envelope to a relay server's `/envelopes` endpoint.
pub fn post_envelope(relay_url: &str, envelope: &Envelope) -> Result<(), String> {
    let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
    let json_val =
        serde_json::to_value(envelope).map_err(|e| format!("failed to serialize envelope: {e}"))?;
    ureq::post(&url)
        .send_json(json_val)
        .map_err(|e| format!("relay POST failed: {e}"))?;
    Ok(())
}

/// Fetch all envelopes from a relay inbox for the given peer.
///
/// The relay typically drains messages on fetch, so callers must process
/// all returned envelopes.
pub fn fetch_inbox(relay_url: &str, peer_id: &str) -> Result<Vec<Envelope>, String> {
    let base = relay_url.trim_end_matches('/');
    let inbox_url = format!("{}/inbox/{}", base, peer_id);
    let envelopes: Vec<Envelope> = ureq::get(&inbox_url)
        .call()
        .map_err(|e| format!("relay fetch failed: {e}"))?
        .into_json()
        .map_err(|e| format!("deserialize inbox: {e}"))?;
    Ok(envelopes)
}
