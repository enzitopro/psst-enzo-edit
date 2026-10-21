use crate::error::Error;
use crate::oauth::listen_for_callback_parameter;
use crate::util::default_ureq_agent_builder;
use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";

/// An authenticated Last.fm session.
#[derive(Clone)]
pub struct Scrobbler {
    api_key: String,
    api_secret: String,
    session_key: String,
}

pub struct LastFmClient;

impl LastFmClient {
    /// Report a track as "now playing" to Last.fm.
    pub fn now_playing_song(
        scrobbler: &Scrobbler,
        artist: &str,
        title: &str,
        album: Option<&str>,
    ) -> Result<(), Error> {
        scrobbler.call("track.updateNowPlaying", artist, title, album, None)
    }

    /// Scrobble a track that has been playing for a while to Last.fm.
    pub fn scrobble_song(
        scrobbler: &Scrobbler,
        artist: &str,
        title: &str,
        album: Option<&str>,
    ) -> Result<(), Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        scrobbler.call("track.scrobble", artist, title, album, Some(&now))
    }

    /// Creates an authenticated Last.fm Scrobbler instance with provided credentials.
    /// Note: This assumes the session_key is valid. Validity is checked on first API call.
    pub fn create_scrobbler(
        api_key: Option<&str>,
        api_secret: Option<&str>,
        session_key: Option<&str>,
    ) -> Result<Scrobbler, Error> {
        let (Some(api_key), Some(api_secret), Some(session_key)) =
            (api_key, api_secret, session_key)
        else {
            log::warn!("missing Last.fm API key, secret, or session key for scrobbler creation.");
            return Err(Error::ConfigError(
                "Missing Last.fm API key, secret, or session key.".to_string(),
            ));
        };
        log::info!("scrobbler instance created with session key (validity checked on first use).");
        Ok(Scrobbler {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            session_key: session_key.to_string(),
        })
    }
}

impl Scrobbler {
    fn call(
        &self,
        method: &str,
        artist: &str,
        title: &str,
        album: Option<&str>,
        timestamp: Option<&str>,
    ) -> Result<(), Error> {
        let mut params = vec![
            ("method", method),
            ("api_key", &self.api_key),
            ("sk", &self.session_key),
            ("artist", artist),
            ("track", title),
        ];
        params.extend(
            album
                .filter(|album| !album.is_empty())
                .map(|album| ("album", album)),
        );
        params.extend(timestamp.map(|timestamp| ("timestamp", timestamp)));
        call(&self.api_secret, params).map(|_| ())
    }
}

/// Sends a signed Last.fm API call and returns the JSON response.
fn call(api_secret: &str, mut params: Vec<(&str, &str)>) -> Result<serde_json::Value, Error> {
    let api_sig = signature(api_secret, &mut params);
    let mut params: Vec<(&str, &str)> = params;
    params.push(("api_sig", &api_sig));
    params.push(("format", "json"));

    // A fresh agent per call: these are minutes apart, and pooled sockets go stale
    // when the connection drops.  The builder's timeouts keep a dead network from
    // hanging the call.
    let agent: ureq::Agent = default_ureq_agent_builder(None)
        .http_status_as_error(false) // Last.fm explains errors in the JSON body.
        .build()
        .into();
    let json: serde_json::Value = agent
        .post(API_URL)
        .send_form(params)
        .and_then(|mut response| response.body_mut().read_json())
        .map_err(|err| Error::ScrobblerError(err.to_string()))?;
    match json.get("message").and_then(|message| message.as_str()) {
        Some(message) if json.get("error").is_some() => {
            Err(Error::ScrobblerError(message.to_string()))
        }
        _ => Ok(json),
    }
}

/// Last.fm signs calls with the MD5 of every parameter as `namevalue`, sorted by
/// name, followed by the API secret.  Sorts `params` in place.
fn signature(api_secret: &str, params: &mut [(&str, &str)]) -> String {
    params.sort_unstable();
    let mut text: String = params
        .iter()
        .map(|(name, value)| format!("{name}{value}"))
        .collect();
    text.push_str(api_secret);
    format!("{:x}", md5::compute(text))
}

/// Generate a Last.fm authentication URL
pub fn generate_lastfm_auth_url(
    api_key: &str,
    callback_url: &str,
) -> Result<String, url::ParseError> {
    let base = "http://www.last.fm/api/auth/";
    let url = Url::parse_with_params(base, &[("api_key", api_key), ("cb", callback_url)])?;
    Ok(url.to_string())
}

/// Exchange a token for a Last.fm session key
pub fn exchange_token_for_session(
    api_key: &str,
    api_secret: &str,
    token: &str,
) -> Result<String, Error> {
    let params = vec![
        ("method", "auth.getSession"),
        ("api_key", api_key),
        ("token", token),
    ];
    call(api_secret, params)?["session"]["key"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| Error::ScrobblerError("no session key in the response".to_string()))
}

/// Listen for a Last.fm token from the callback
pub fn get_lastfm_token_listener(
    socket_address: SocketAddr,
    timeout: Duration,
) -> Result<String, Error> {
    // Use the shared listener function, specifying "token" as the parameter
    listen_for_callback_parameter(socket_address, timeout, "token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_sorted_params() {
        let mut params = [
            ("token", "t"),
            ("api_key", "k"),
            ("method", "auth.getSession"),
        ];
        // md5("api_keykmethodauth.getSessiontokentsecret")
        assert_eq!(
            signature("secret", &mut params),
            "47656600a9b4352066d3092b68e9036b"
        );
        assert_eq!(params[0].0, "api_key");
    }
}
