use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use druid::{
    im::Vector,
    image::{self, ImageFormat},
    Data, ImageBuf,
};

use itertools::Itertools;
use log::info;
use parking_lot::Mutex;
use psst_core::{
    oauth::{self, WebApiToken},
    session::{
        client_token::{ClientTokenProvider, ClientTokenProviderHandle},
        login5::Login5,
        SessionService,
    },
    system_info::{OS, SPOTIFY_SEMANTIC_VERSION},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use std::sync::OnceLock;
use time::{Date, Month};
use ureq::{
    http::{Response, StatusCode},
    Agent, Body,
};

use crate::{
    data::{
        self, utils::sanitize_html_string, Album, AlbumType, Artist, ArtistAlbums, ArtistInfo,
        ArtistLink, ArtistOverview, ArtistStats, AudioAnalysis, Cached, DatePrecision, Episode,
        EpisodeId, EpisodeLink, Image, MixedView, Nav, Page, Playlist, PublicUser, SearchResults,
        SearchTopic, Show, SpotifyUrl, Track, TrackLines, UserProfile,
    },
    error::Error,
    ui::credits::TrackCredits,
};

use super::{cache::WebApiCache, local_library::LocalLibrary, pathfinder};
use sanitize_html::{rules::predefined::DEFAULT, sanitize_str};

pub struct WebApi {
    agent: Agent,
    cache: WebApiCache,
    paginated_limit: usize,
    webapi_token: Mutex<Option<WebApiToken>>,
    webapi_client_id: Mutex<Option<String>>,
    // First-party session credentials, used for `api-partner.spotify.com`
    // (pathfinder GraphQL) calls, which reject the Web API OAuth token.
    session: Mutex<Option<SessionService>>,
    login5: Login5,
    client_token_provider: ClientTokenProviderHandle,
    /// Cached responses, as `bucket/key`, to fetch again on their next load.
    stale: Mutex<HashSet<String>>,
}

impl WebApi {
    pub fn new(
        proxy_url: Option<&str>,
        cache_base: Option<PathBuf>,
        paginated_limit: usize,
    ) -> Self {
        // Status codes are checked in `with_retry`, which needs to see 429
        // responses (and their `Retry-After` header) instead of an opaque error.
        let mut agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false);
        if let Some(proxy_url) = proxy_url {
            let proxy = ureq::Proxy::new(proxy_url).ok();
            agent = agent.proxy(proxy);
        }
        let client_token_provider = ClientTokenProvider::new_shared(proxy_url);
        Self {
            agent: agent.build().into(),
            cache: WebApiCache::new(cache_base),
            paginated_limit,
            webapi_token: Mutex::new(None),
            webapi_client_id: Mutex::new(None),
            session: Mutex::new(None),
            login5: Login5::new(Some(Arc::clone(&client_token_provider)), proxy_url),
            client_token_provider,
            stale: Mutex::default(),
        }
    }

    // Similar to how librespot does this https://github.com/librespot-org/librespot/blob/dev/core/src/version.rs
    fn user_agent() -> String {
        let platform = match OS {
            "macos" => "OSX",
            "windows" => "Win32",
            _ => "Linux",
        };
        format!(
            "Spotify/{} {}/0 (psst/{})",
            SPOTIFY_SEMANTIC_VERSION,
            platform,
            env!("CARGO_PKG_VERSION")
        )
    }

    /// Update the cached Web API token and client ID (called from GUI on config changes).
    pub fn set_webapi_credentials(&self, client_id: Option<String>, token: Option<WebApiToken>) {
        *self.webapi_client_id.lock() = client_id;
        *self.webapi_token.lock() = token;
    }

    /// The current Web API token, refreshed in memory as it expires.
    pub fn webapi_token(&self) -> Option<WebApiToken> {
        self.webapi_token.lock().clone()
    }

    /// Install the authenticated core session, used to mint the first-party
    /// tokens that `api-partner.spotify.com` requires.
    pub fn set_session(&self, session: SessionService) {
        *self.session.lock() = Some(session);
    }

    /// Mint the `(bearer, client-token)` pair accepted by `api-partner`.  Uses
    /// the Login5 access token from the core session plus a protobuf client
    /// token — the same credentials the web player and `Cdn` use.
    fn partner_token(&self) -> Result<(String, String), Error> {
        let session =
            self.session.lock().clone().ok_or_else(|| {
                Error::WebApiError("No active session for api-partner".to_string())
            })?;
        let access_token = self
            .login5
            .get_access_token(&session)
            .map_err(|err| Error::WebApiError(err.to_string()))?;
        let client_token = self
            .client_token_provider
            .get()
            .map_err(|err| Error::WebApiError(err.to_string()))?;
        Ok((access_token.access_token, client_token))
    }

    fn access_token(&self) -> Result<String, Error> {
        let mut token_guard = self.webapi_token.lock();
        if let Some(ref token) = *token_guard {
            if !token.is_expired() {
                return Ok(token.access_token.clone());
            }
            // Try to refresh
            let client_id_guard = self.webapi_client_id.lock();
            if let (Some(ref client_id), Some(ref refresh_token)) =
                (&*client_id_guard, &token.refresh_token)
            {
                log::info!("Web API token expired, attempting refresh...");
                match oauth::refresh_webapi_token(client_id, refresh_token) {
                    Ok(new_token) => {
                        let access_token = new_token.access_token.clone();
                        // Only in memory: the main window copies it into
                        // config.json when it closes (`webapi_token()`), since
                        // Spotify may rotate the refresh token.
                        *token_guard = Some(new_token);
                        return Ok(access_token);
                    }
                    Err(e) => {
                        log::error!("Failed to refresh Web API token: {e}");
                    }
                }
            }
        }
        Err(Error::WebApiError(
            "No valid Web API token available. Did you enable Web API for the Spotify Developer Client?".to_string(),
        ))
    }

    fn request(&self, request: &RequestBuilder) -> Result<Response<Body>, Error> {
        // `api-partner.spotify.com` rejects the Web API OAuth token, so those
        // requests carry the first-party Login5 bearer + client-token instead.
        let (token, client_token) = if request.partner_auth {
            let (bearer, client_token) = self.partner_token()?;
            (bearer, Some(client_token))
        } else {
            (self.access_token()?, None)
        };
        let url = request.build();

        fn configure_request<B>(
            req_builder: ureq::RequestBuilder<B>,
            token: &str,
            client_token: Option<&str>,
            headers: &HashMap<String, String>,
        ) -> ureq::RequestBuilder<B> {
            let mut req = req_builder.header("Authorization", &format!("Bearer {token}"));
            if let Some(client_token) = client_token {
                req = req.header("client-token", client_token);
            }
            headers
                .iter()
                .fold(req, |current_req, (k, v)| current_req.header(k, v))
        }

        let ct = client_token.as_deref();
        let headers = request.get_headers();
        match request.get_method() {
            Method::Get => configure_request(self.agent.get(&url), &token, ct, headers)
                .call()
                .map_err(|err| Error::WebApiError(err.to_string())),
            Method::Post => configure_request(self.agent.post(&url), &token, ct, headers)
                .send_json(request.get_body())
                .map_err(|err| Error::WebApiError(err.to_string())),
            Method::Put => configure_request(self.agent.put(&url), &token, ct, headers)
                .send_json(request.get_body())
                .map_err(|err| Error::WebApiError(err.to_string())),
            Method::Delete => configure_request(self.agent.delete(&url), &token, ct, headers)
                .force_send_body()
                .send_json(request.get_body())
                .map_err(|err| Error::WebApiError(err.to_string())),
        }
    }

    fn with_retry(f: impl Fn() -> Result<Response<Body>, Error>) -> Result<Response<Body>, Error> {
        const MAX_RETRIES: u32 = 4;
        // Spotify can answer with a `Retry-After` of hours when it throttles hard;
        // sleeping that long would hang the worker thread, so give up instead.
        const MAX_RETRY_AFTER_SECS: u64 = 30;

        let mut attempt = 0;
        let mut transport_attempts = 0;
        loop {
            let response = match f() {
                Ok(response) => response,
                // Flaky connections get reset or time out now and then.  After a
                // network drop every pooled socket is dead, and ureq keeps up to 3
                // per host, so retry past them to reach a fresh one, right away:
                // waiting between tries only left pages loading for seconds.
                Err(err) if is_transport_error(&err) && transport_attempts < 3 => {
                    transport_attempts += 1;
                    log::info!("request failed ({err}), retrying");
                    continue;
                }
                Err(err) => return Err(err),
            };
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RETRIES {
                let retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|secs| secs.to_str().ok())
                    .and_then(|secs| secs.trim().parse::<u64>().ok());
                // Exponential backoff when the server gives no hint: 1, 2, 4, 8s.
                let secs = retry_after.unwrap_or(1 << attempt);
                if secs <= MAX_RETRY_AFTER_SECS {
                    log::warn!("rate limited by Spotify, retrying in {secs}s");
                    thread::sleep(Duration::from_secs(secs));
                    attempt += 1;
                    continue;
                }
                break Err(Error::WebApiError(format!(
                    "Spotify is rate limiting requests, try again in {} min",
                    secs.div_ceil(60)
                )));
            }
            if status.is_client_error() || status.is_server_error() {
                break Err(Error::WebApiError(format!(
                    "http status: {}",
                    status.as_u16()
                )));
            }
            break Ok(response);
        }
    }

    /// Send a request with an empty JSON object, throw away the response body.
    /// Use for POST/PUT/DELETE requests.
    fn send_empty_json(&self, request: &RequestBuilder) -> Result<(), Error> {
        Self::with_retry(|| self.request(request)).map(|_| ())
    }

    /// Send a request and return the deserialized JSON body.  Use for GET
    /// requests.
    fn load<T: DeserializeOwned>(&self, request: &RequestBuilder) -> Result<T, Error> {
        let mut response = Self::with_retry(|| self.request(request))?;
        response
            .body_mut()
            .read_json()
            .map_err(|err| Error::WebApiError(err.to_string()))
    }

    /// GET a public JSON endpoint that needs no Spotify credentials (LRCLIB).
    /// Retries a couple of times when the server is busy, `Ok(None)` on 404.
    pub(super) fn get_public_json<T: DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Option<T>, Error> {
        const ATTEMPTS: u32 = 3;

        let user_agent = format!(
            "psst/{} (https://github.com/jpochyla/psst)",
            env!("CARGO_PKG_VERSION")
        );
        let mut attempt = 0;
        loop {
            let mut response = self
                .agent
                .get(url)
                .header("User-Agent", &user_agent)
                .call()
                .map_err(|err| Error::WebApiError(err.to_string()))?;
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let busy = status == StatusCode::SERVICE_UNAVAILABLE
                || status == StatusCode::TOO_MANY_REQUESTS;
            if busy && attempt + 1 < ATTEMPTS {
                attempt += 1;
                log::info!("{url} is busy, retrying");
                thread::sleep(Duration::from_secs(1 << attempt));
                continue;
            }
            if busy {
                return Err(Error::WebApiError(
                    "The lyrics server is busy, try again in a moment".to_string(),
                ));
            }
            if status.is_client_error() || status.is_server_error() {
                return Err(Error::WebApiError(format!(
                    "http status: {}",
                    status.as_u16()
                )));
            }
            return response
                .body_mut()
                .read_json()
                .map(Some)
                .map_err(|err| Error::WebApiError(err.to_string()));
        }
    }

    pub(super) fn cache_get_json<T: DeserializeOwned>(&self, bucket: &str, key: &str) -> Option<T> {
        let file = self.cache.get(bucket, key)?;
        serde_json::from_reader(io::BufReader::new(file)).ok()
    }

    pub(super) fn cache_set_json(&self, bucket: &str, key: &str, value: &impl Serialize) {
        match serde_json::to_vec(value) {
            Ok(bytes) => self.cache.set(bucket, key, &bytes),
            Err(err) => log::warn!("failed to serialize {bucket} cache entry: {err}"),
        }
    }

    /// The next load of this cached response fetches it again, still falling
    /// back to the copy when that fails.
    pub fn refresh_cached(&self, bucket: &str, key: &str) {
        self.stale.lock().insert(format!("{bucket}/{key}"));
    }

    /// Send a request, unless a recent copy of the response is in the cache.
    /// Older copies are refreshed (new releases, new episodes) but still used when
    /// that fails, e.g. offline.  Unreadable copies count as missing.
    fn load_cached<T: Data + DeserializeOwned>(
        &self,
        request: &RequestBuilder,
        bucket: &str,
        key: &str,
    ) -> Result<Cached<T>, Error> {
        const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

        let cached = self.cache.get(bucket, key).and_then(|file| {
            let cached_at = file.metadata().ok()?.modified().ok()?;
            let value = serde_json::from_reader(io::BufReader::new(file)).ok()?;
            Some(Cached::new(value, cached_at))
        });
        let is_recent = |cached: &Cached<T>| {
            cached
                .cached_at
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age < MAX_AGE)
        };
        let refresh = self.stale.lock().remove(&format!("{bucket}/{key}"));
        match cached {
            Some(cached) if is_recent(&cached) && !refresh => Ok(cached),
            stale => {
                let fetched: Result<Cached<T>, Error> = (|| {
                    let response = Self::with_retry(|| self.request(request))?;
                    let mut body = Vec::new();
                    response.into_body().into_reader().read_to_end(&mut body)?;
                    let value = serde_json::from_slice(&body)?;
                    self.cache.set(bucket, key, &body);
                    Ok(Cached::fresh(value))
                })();
                fetched.or_else(|err| stale.ok_or(err))
            }
        }
    }

    /// Iterate a paginated result set by sending `request` with added
    /// pagination parameters.  Mostly used through `load_all_pages`.
    fn for_all_pages<T: DeserializeOwned + Clone>(
        &self,
        request: &RequestBuilder,
        mut func: impl FnMut(Page<T>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // TODO: Some result sets, like very long playlists and saved tracks/albums can
        // be very big.  Implement virtualized scrolling and lazy-loading of results.
        let mut limit = 50;
        let mut offset = 0;
        loop {
            let req = request
                .clone()
                .query("limit".to_string(), limit.to_string())
                .query("offset".to_string(), offset.to_string());
            let page: Page<T> = self.load(&req)?;

            let page_total = page.total;
            let page_offset = page.offset;
            let page_limit = page.limit;
            func(page)?;

            if page_total > offset && offset < self.paginated_limit {
                limit = page_limit;
                offset = page_offset + page_limit;
            } else {
                break Ok(());
            }
        }
    }

    /// Very similar to `for_all_pages`, but only returns a certain number of results
    fn for_some_pages<T: DeserializeOwned + Clone>(
        &self,
        request: &RequestBuilder,
        lim: usize,
        mut func: impl FnMut(Page<T>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut limit = 50;
        let mut offset = 0;
        if lim < limit {
            limit = lim;
            let req = request
                .clone()
                .query("limit".to_string(), limit.to_string())
                .query("offset".to_string(), offset.to_string());

            let page: Page<T> = self.load(&req)?;

            func(page)?;
        } else {
            loop {
                let req = request
                    .clone()
                    .query("limit".to_string(), limit.to_string())
                    .query("offset".to_string(), offset.to_string());

                let page: Page<T> = self.load(&req)?;

                let page_total = limit / lim;
                let page_offset = page.offset;
                let page_limit = page.limit;
                func(page)?;

                if page_total > offset && offset < self.paginated_limit {
                    limit = page_limit;
                    offset = page_offset + page_limit;
                } else {
                    break;
                }
            }
        }
        Ok(())
    }
    /// Load a paginated result set by sending `request` with added pagination
    /// parameters and return the aggregated results.  Use with GET requests.
    fn load_all_pages<T: DeserializeOwned + Clone>(
        &self,
        request: &RequestBuilder,
    ) -> Result<Vector<T>, Error> {
        let mut results = Vector::new();

        self.for_all_pages(request, |page| {
            results.append(page.items);
            Ok(())
        })?;

        Ok(results)
    }

    /// Every page of a result set, ignoring the paginated limit.  Pages after the
    /// first are fetched by a few threads at once, a page at a time is slow.
    fn load_every_page<T: DeserializeOwned + Clone + Send + Sync>(
        &self,
        request: &RequestBuilder,
    ) -> Result<Vector<T>, Error> {
        const PAGE_SIZE: usize = 50;

        let page = |offset: usize| -> Result<Page<T>, Error> {
            self.load(
                &request
                    .clone()
                    .query("limit", PAGE_SIZE)
                    .query("offset", offset),
            )
        };
        let first = page(0)?;
        let offsets: Vec<_> = (PAGE_SIZE..first.total).step_by(PAGE_SIZE).collect();
        let mut items = first.items;
        for page in fetch_pages(&offsets, page)? {
            items.append(page.items);
        }
        Ok(items)
    }

    /// Does a similar thing as `load_all_pages`, but limiting the number of results
    fn load_some_pages<T: DeserializeOwned + Clone>(
        &self,
        request: &RequestBuilder,
        number: usize,
    ) -> Result<Vector<T>, Error> {
        let mut results = Vector::new();

        self.for_some_pages(request, number, |page| {
            results.append(page.items);
            Ok(())
        })?;

        Ok(results)
    }

    fn load_and_return_home_section(&self, request: &RequestBuilder) -> Result<MixedView, Error> {
        #[derive(Deserialize)]
        pub struct Welcome {
            data: WelcomeData,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct WelcomeData {
            home_sections: HomeSections,
        }

        #[derive(Deserialize)]
        pub struct HomeSections {
            sections: Vec<Section>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct Section {
            data: SectionData,
            section_items: SectionItems,
        }

        #[derive(Deserialize)]
        pub struct SectionData {
            title: Title,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct Title {
            #[serde(alias = "transformedLabel")]
            text: String,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct SectionItems {
            items: Vec<Item>,
        }

        #[derive(Deserialize)]
        pub struct Item {
            content: Content,
        }

        #[derive(Deserialize)]
        pub struct Content {
            // Unknown item kinds come without `data`; they're skipped (no `uri`).
            #[serde(default)]
            data: ContentData,
        }

        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase")]
        pub struct ContentData {
            #[serde(rename = "__typename")]
            typename: DataTypename,
            name: Option<String>,
            uri: Option<String>,

            // Playlist-specific fields
            attributes: Option<Vec<Attribute>>,
            description: Option<String>,
            images: Option<Images>,
            owner_v2: Option<OwnerV2>,

            // Artist-specific fields
            artists: Option<Artists>,
            profile: Option<Profile>,
            visuals: Option<Visuals>,

            // Show-specific fields
            cover_art: Option<CoverArt>,
            publisher: Option<Publisher>,
            total_episodes: Option<usize>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct Visuals {
            avatar_image: CoverArt,
        }

        #[derive(Deserialize)]
        pub struct Artists {
            items: Vec<ArtistsItem>,
        }

        #[derive(Deserialize)]
        pub struct ArtistsItem {
            profile: Profile,
            uri: String,
        }

        #[derive(Deserialize)]
        pub struct Profile {
            name: String,
        }

        #[derive(Deserialize)]
        pub struct Attribute {
            key: String,
            value: String,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct CoverArt {
            sources: Vec<Source>,
        }

        #[derive(Deserialize)]
        pub struct Source {
            url: String,
        }

        #[derive(Deserialize)]
        #[allow(dead_code)]
        pub enum MediaType {
            #[serde(rename = "AUDIO")]
            Audio,
            #[serde(rename = "MIXED")]
            Mixed,
        }

        #[derive(Deserialize)]
        pub struct Publisher {
            name: String,
        }

        #[derive(Deserialize, Default)]
        pub enum DataTypename {
            Podcast,
            Playlist,
            Artist,
            Album,
            // Episodes, audiobooks and new kinds of items are skipped.
            #[default]
            #[serde(other)]
            NotFound,
        }

        #[derive(Deserialize)]
        pub struct Images {
            items: Vec<ImagesItem>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct ImagesItem {
            sources: Vec<Source>,
        }

        #[derive(Deserialize)]
        pub struct OwnerV2 {
            data: OwnerV2Data,
        }

        #[derive(Deserialize)]
        pub struct OwnerV2Data {
            #[serde(rename = "__typename")]
            name: String,
        }

        // Extract the playlists
        let result: Welcome = match self.load(request) {
            Ok(res) => res,
            Err(e) => {
                info!("Error loading home section: {e}");
                return Err(e);
            }
        };

        let mut title: Arc<str> = Arc::from("");
        let mut playlist: Vector<Playlist> = Vector::new();
        let mut album: Vector<Arc<Album>> = Vector::new();
        let mut artist: Vector<Artist> = Vector::new();
        let mut show: Vector<Arc<Show>> = Vector::new();

        result
            .data
            .home_sections
            .sections
            .iter()
            .for_each(|section| {
                title = section.data.title.text.clone().into();

                section.section_items.items.iter().for_each(|item| {
                    let Some(uri) = &item.content.data.uri else {
                        return;
                    };
                    // Removed or region-locked items can come without a name.
                    let name = match item.content.data.typename {
                        DataTypename::Artist => item.content.data.profile.as_ref().map(|p| &p.name),
                        _ => item.content.data.name.as_ref(),
                    };
                    let Some(name) = name.map(|name| Arc::from(name.as_str())) else {
                        return;
                    };
                    let id = uri.split(':').next_back().unwrap_or("").to_string();

                    match item.content.data.typename {
                        DataTypename::Playlist => {
                            playlist.push_back(Playlist {
                                id: id.into(),
                                name,
                                images: Some(item.content.data.images.as_ref().map_or_else(
                                    Vector::new,
                                    |images| {
                                        images
                                            .items
                                            .iter()
                                            .map(|img| data::utils::Image {
                                                url: Arc::from(
                                                    img.sources
                                                        .first()
                                                        .map(|s| s.url.as_str())
                                                        .unwrap_or_default(),
                                                ),
                                                width: None,
                                                height: None,
                                            })
                                            .collect()
                                    },
                                )),
                                description: {
                                    let desc = sanitize_html_string(
                                        item.content
                                            .data
                                            .description
                                            .as_deref()
                                            .unwrap_or_default(),
                                    );

                                    // This is roughly 3 lines of description, truncated if too long
                                    if desc.chars().count() > 55 {
                                        Arc::from(desc.chars().take(52).collect::<String>() + "...")
                                    } else {
                                        desc
                                    }
                                },
                                track_count: item.content.data.attributes.as_ref().and_then(
                                    |attrs| {
                                        attrs
                                            .iter()
                                            .find(|attr| attr.key == "track_count")
                                            .and_then(|attr| attr.value.parse().ok())
                                    },
                                ),
                                owner: PublicUser {
                                    id: Arc::from(""),
                                    display_name: Arc::from(
                                        item.content
                                            .data
                                            .owner_v2
                                            .as_ref()
                                            .map(|owner| owner.data.name.as_str())
                                            .unwrap_or_default(),
                                    ),
                                },
                                collaborative: false,
                                public: None,
                                pinned: false,
                            });
                        }
                        DataTypename::Artist => artist.push_back(Artist {
                            id: id.into(),
                            name,
                            images: item.content.data.visuals.as_ref().map_or_else(
                                Vector::new,
                                |images| {
                                    images
                                        .avatar_image
                                        .sources
                                        .iter()
                                        .map(|img| data::utils::Image {
                                            url: Arc::from(img.url.as_str()),
                                            width: None,
                                            height: None,
                                        })
                                        .collect()
                                },
                            ),
                        }),
                        DataTypename::Album => album.push_back(Arc::new(Album {
                            id: id.into(),
                            name,
                            album_type: AlbumType::Album,
                            images: item.content.data.cover_art.as_ref().map_or_else(
                                Vector::new,
                                |images| {
                                    images
                                        .sources
                                        .iter()
                                        .map(|src| data::utils::Image {
                                            url: Arc::from(src.url.clone()),
                                            width: None,
                                            height: None,
                                        })
                                        .collect()
                                },
                            ),
                            artists: item.content.data.artists.as_ref().map_or_else(
                                Vector::new,
                                |artists| {
                                    artists
                                        .items
                                        .iter()
                                        .map(|artist| ArtistLink {
                                            id: Arc::from(
                                                artist
                                                    .uri
                                                    .split(':')
                                                    .next_back()
                                                    .unwrap_or("")
                                                    .to_string(),
                                            ),
                                            name: Arc::from(artist.profile.name.clone()),
                                        })
                                        .collect()
                                },
                            ),
                            copyrights: Vector::new(),
                            label: "".into(),
                            tracks: Vector::new(),
                            release_date: None,
                            release_date_precision: None,
                        })),
                        DataTypename::Podcast => show.push_back(Arc::new(Show {
                            id: id.into(),
                            name,
                            images: item.content.data.cover_art.as_ref().map_or_else(
                                Vector::new,
                                |images| {
                                    images
                                        .sources
                                        .iter()
                                        .map(|src| data::utils::Image {
                                            url: Arc::from(src.url.clone()),
                                            width: None,
                                            height: None,
                                        })
                                        .collect()
                                },
                            ),
                            publisher: Arc::from(
                                item.content
                                    .data
                                    .publisher
                                    .as_ref()
                                    .map(|p| p.name.as_str())
                                    .unwrap_or(""),
                            ),
                            description: Arc::from(
                                item.content.data.description.as_deref().unwrap_or(""),
                            ),
                            total_episodes: item.content.data.total_episodes,
                        })),
                        // For section items we don't cover yet
                        DataTypename::NotFound => {}
                    }
                });
            });

        Ok(MixedView {
            title,
            playlists: playlist,
            artists: artist,
            albums: album,
            shows: show,
        })
    }
}

/// A Web API playlist.  Since 2026 its tracks (and their count) come as `items`
/// instead of `tracks`, which left the count out.
fn playlist_from_json(mut value: serde_json::Value) -> Result<Playlist, Error> {
    if let Some(playlist) = value.as_object_mut() {
        if !playlist.contains_key("tracks") {
            if let Some(items) = playlist.remove("items") {
                playlist.insert("tracks".to_string(), items);
            }
        }
    }
    Ok(serde_json::from_value(value)?)
}

/// Positions `tracks` in their order, which a copy from the cache doesn't keep.
fn numbered(mut tracks: Vector<Arc<Track>>) -> Vector<Arc<Track>> {
    for (position, track) in tracks.iter_mut().enumerate() {
        if track.track_pos != position {
            Arc::make_mut(track).track_pos = position;
        }
    }
    tracks
}

/// The pages at `offsets`, in order, fetched by a couple of threads: a page at a
/// time is slow for thousands of tracks, more threads made Spotify answer "too
/// many requests".
fn fetch_pages<P: Send>(
    offsets: &[usize],
    page: impl Fn(usize) -> Result<P, Error> + Sync,
) -> Result<Vec<P>, Error> {
    const THREADS: usize = 2;
    let per_thread = offsets.len().div_ceil(THREADS).max(1);
    thread::scope(|scope| {
        let threads: Vec<_> = offsets
            .chunks(per_thread)
            .map(|chunk| scope.spawn(|| chunk.iter().map(|&o| page(o)).collect::<Vec<_>>()))
            .collect();
        threads
            .into_iter()
            .flat_map(|thread| thread.join().expect("page thread panicked"))
            .collect()
    })
}

/// Whether a failed request didn't reach the server or got cut off, as
/// opposed to a missing token or session, which a retry won't fix.
fn is_transport_error(err: &Error) -> bool {
    let Error::WebApiError(message) = err;
    let message = message.to_lowercase();
    message.starts_with("io:")
        || message.contains("timeout")
        || message.contains("timed out")
        || message.contains("connection")
}

/// Saves (`Put`) or removes (`Delete`) any kind of item from the library.  The
/// endpoint takes the URI as a query parameter, a JSON body is a 400.
fn library_request(method: Method, uri: &str) -> RequestBuilder {
    RequestBuilder::new("v1/me/library", method, None).query("uris", urlencoding::encode(uri))
}

static GLOBAL_WEBAPI: OnceLock<Arc<WebApi>> = OnceLock::new();

/// Global instance.
impl WebApi {
    pub fn install_as_global(self) {
        GLOBAL_WEBAPI
            .set(Arc::new(self))
            .map_err(|_| "Cannot install more than once")
            .unwrap()
    }

    pub fn global() -> Arc<Self> {
        GLOBAL_WEBAPI.get().unwrap().clone()
    }
}

/// User endpoints.
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-users-profile
    pub fn get_user_profile(&self) -> Result<UserProfile, Error> {
        let result = self.load(&RequestBuilder::new("v1/me".to_string(), Method::Get, None))?;
        Ok(result)
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-users-top-artists-and-tracks
    pub fn get_user_top_tracks(&self) -> Result<Vector<Arc<Track>>, Error> {
        let request = &RequestBuilder::new("v1/me/top/tracks".to_string(), Method::Get, None)
            .query("market", "from_token");
        let result: Vector<Arc<Track>> = self.load_some_pages(request, 30)?;

        Ok(result)
    }

    pub fn get_user_top_artist(&self) -> Result<Vector<Artist>, Error> {
        #[derive(Clone, Data, Deserialize)]
        #[allow(dead_code)]
        struct Artists {
            artists: Artist,
        }
        let request = &RequestBuilder::new("v1/me/top/artists", Method::Get, None);

        Ok(self
            .load_some_pages(request, 10)?
            .into_iter()
            .map(|item: Artist| item)
            .collect())
    }
}

/// Persisted-query hashes for the pathfinder discography operations, extracted
/// from the web player bundle.  Albums, singles and compilations all share one
/// document (selected by `operationName`); appears-on is a separate one.
const DISCOGRAPHY_HASH: &str = "5e07d323febb57b4a56a42abbf781490e58764aa45feb6e3dc0591564fc56599";
const APPEARS_ON_HASH: &str = "9a4bb7a20d6720fe52d7b47bc001cfa91940ddf5e7113761460b4a288d18a4c1";

// Shape of the `queryArtistDiscography*` / `queryArtistAppearsOn` responses.
// Each operation populates a different field, so all are optional and the
// caller selects the one it wants.
#[derive(Deserialize)]
struct DiscographyResponse {
    data: DiscographyData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscographyData {
    artist_union: DiscographyUnion,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscographyUnion {
    #[serde(default)]
    discography: Discography,
    #[serde(default)]
    related_content: RelatedContent,
}

#[derive(Default, Deserialize)]
struct Discography {
    #[serde(default)]
    albums: Option<DiscographySection>,
    #[serde(default)]
    singles: Option<DiscographySection>,
    #[serde(default)]
    compilations: Option<DiscographySection>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelatedContent {
    #[serde(default)]
    appears_on: Option<DiscographySection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscographySection {
    #[serde(default)]
    total_count: i64,
    #[serde(default)]
    items: Vec<ReleaseHolder>,
}

#[derive(Deserialize)]
struct ReleaseHolder {
    releases: Releases,
}

#[derive(Deserialize)]
struct Releases {
    #[serde(default)]
    items: Vec<Release>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    id: Arc<str>,
    name: Arc<str>,
    #[serde(default)]
    cover_art: CoverArt,
    #[serde(default)]
    date: Option<ReleaseDate>,
    #[serde(rename = "type", default)]
    release_type: Option<String>,
    #[serde(default)]
    artists: ReleaseArtists,
}

#[derive(Default, Deserialize)]
struct CoverArt {
    #[serde(default)]
    sources: Vector<Image>,
}

#[derive(Deserialize)]
struct ReleaseDate {
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    month: Option<u8>,
    #[serde(default)]
    day: Option<u8>,
    #[serde(default)]
    precision: Option<String>,
}

#[derive(Default, Deserialize)]
struct ReleaseArtists {
    #[serde(default)]
    items: Vec<ReleaseArtist>,
}

#[derive(Deserialize)]
struct ReleaseArtist {
    uri: String,
    profile: ReleaseArtistProfile,
}

#[derive(Deserialize)]
struct ReleaseArtistProfile {
    name: Arc<str>,
}

impl Release {
    fn to_album(&self, default_type: AlbumType) -> Album {
        let album_type = match self.release_type.as_deref() {
            Some("ALBUM") => AlbumType::Album,
            Some("SINGLE") => AlbumType::Single,
            Some("COMPILATION") => AlbumType::Compilation,
            _ => default_type,
        };
        let (release_date, release_date_precision) = self
            .date
            .as_ref()
            .map(ReleaseDate::to_date)
            .unwrap_or((None, None));

        Album {
            id: self.id.clone(),
            name: self.name.clone(),
            album_type,
            images: self.cover_art.sources.clone(),
            artists: self
                .artists
                .items
                .iter()
                .map(|artist| ArtistLink {
                    id: artist.uri.rsplit(':').next().unwrap_or_default().into(),
                    name: artist.profile.name.clone(),
                })
                .collect(),
            copyrights: Vector::new(),
            label: "".into(),
            tracks: Vector::new(),
            release_date,
            release_date_precision,
        }
    }
}

impl ReleaseDate {
    fn to_date(&self) -> (Option<Date>, Option<DatePrecision>) {
        let precision = match self.precision.as_deref() {
            Some("DAY") => Some(DatePrecision::Day),
            Some("MONTH") => Some(DatePrecision::Month),
            Some("YEAR") => Some(DatePrecision::Year),
            _ => None,
        };
        // Only the year is required; month/day default to 1 so we can still form
        // a valid `Date` even when precision is coarser than a full day.
        let date = self.year.and_then(|year| {
            let month = self
                .month
                .and_then(|month| Month::try_from(month).ok())
                .unwrap_or(Month::January);
            let day = self.day.filter(|day| *day >= 1).unwrap_or(1);
            Date::from_calendar_date(year, month, day).ok()
        });
        (date, precision)
    }
}

/// Artist endpoints.
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-artist/
    pub fn get_artist(&self, id: &str) -> Result<Artist, Error> {
        let request = &RequestBuilder::new(format!("v1/artists/{id}"), Method::Get, None);
        let result = self.load_cached(request, "artist", id)?;
        Ok(result.data)
    }

    // Albums, singles, compilations and appears-on each come from a separate
    // pathfinder discography operation on `api-partner.spotify.com`, paginated
    // independently.
    pub fn get_artist_albums(&self, id: &str) -> Result<ArtistAlbums, Error> {
        Ok(ArtistAlbums {
            albums: self.artist_releases(
                id,
                "queryArtistDiscographyAlbums",
                DISCOGRAPHY_HASH,
                true,
                AlbumType::Album,
                |union| union.discography.albums.as_ref(),
            )?,
            singles: self.artist_releases(
                id,
                "queryArtistDiscographySingles",
                DISCOGRAPHY_HASH,
                true,
                AlbumType::Single,
                |union| union.discography.singles.as_ref(),
            )?,
            compilations: self.artist_releases(
                id,
                "queryArtistDiscographyCompilations",
                DISCOGRAPHY_HASH,
                true,
                AlbumType::Compilation,
                |union| union.discography.compilations.as_ref(),
            )?,
            appears_on: self.artist_releases(
                id,
                "queryArtistAppearsOn",
                APPEARS_ON_HASH,
                false,
                AlbumType::Album,
                |union| union.related_content.appears_on.as_ref(),
            )?,
        })
    }

    /// Fetch one discography section, paging until `totalCount` is exhausted.
    /// `select` picks the section out of the response, `default_type` fills in
    /// releases that omit their `type`, and `order` sends `DATE_DESC` (which the
    /// appears-on operation rejects).
    fn artist_releases(
        &self,
        id: &str,
        operation: &str,
        hash: &str,
        order: bool,
        default_type: AlbumType,
        select: impl Fn(&DiscographyUnion) -> Option<&DiscographySection>,
    ) -> Result<Vector<Arc<Album>>, Error> {
        const PAGE: usize = 50;

        let mut releases = Vector::new();
        let mut offset = 0;
        loop {
            let mut variables = json!({
                "uri": format!("spotify:artist:{id}"),
                "offset": offset,
                "limit": PAGE,
            });
            if order {
                variables["order"] = json!("DATE_DESC");
            }
            let json = json!({
                "operationName": operation,
                "variables": variables,
                "extensions": {
                    "persistedQuery": { "version": 1, "sha256Hash": hash }
                },
            });
            let request =
                &RequestBuilder::new("pathfinder/v2/query".to_string(), Method::Post, Some(json))
                    .set_base_uri("api-partner.spotify.com")
                    .header("User-Agent", Self::user_agent())
                    .partner_auth();

            let response: DiscographyResponse = self.load(request)?;
            let union = response.data.artist_union;
            let Some(section) = select(&union) else {
                break;
            };

            for holder in &section.items {
                if let Some(release) = holder.releases.items.first() {
                    releases.push_back(Arc::new(release.to_album(default_type.clone())));
                }
            }

            offset += PAGE;
            if section.items.is_empty() || offset >= section.total_count.max(0) as usize {
                break;
            }
        }
        Ok(releases)
    }

    // Artist bio, stats, image, external links and related artists, from the
    // pathfinder GraphQL `queryArtistOverview` operation.  One response feeds
    // the whole artist page, so it is fetched, cached and parsed as one.
    pub fn get_artist_overview(&self, id: &str) -> Result<Cached<ArtistOverview>, Error> {
        #[derive(Clone, Data, Deserialize)]
        struct Welcome {
            data: WelcomeData,
        }
        #[derive(Clone, Data, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WelcomeData {
            artist_union: ArtistUnion,
        }
        // Sparse artists (no monthly listeners yet) omit `profile`, `stats`,
        // `visuals` and `relatedContent` entirely, so every branch must
        // tolerate their absence.
        #[derive(Clone, Data, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ArtistUnion {
            profile: Option<Profile>,
            stats: Option<Stats>,
            visuals: Option<Visuals>,
            related_content: Option<RelatedContent>,
        }
        #[derive(Clone, Data, Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Profile {
            biography: Option<Biography>,
            #[serde(default)]
            external_links: ExternalLinks,
        }
        #[derive(Clone, Data, Deserialize)]
        struct Biography {
            text: Option<String>,
        }
        #[derive(Clone, Data, Default, Deserialize)]
        struct ExternalLinks {
            #[serde(default)]
            items: Vector<ExternalLinksItem>,
        }
        #[derive(Clone, Data, Deserialize)]
        struct ExternalLinksItem {
            url: String,
        }
        // Individual counters can also be null even when `stats` is present.
        #[derive(Clone, Data, Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Stats {
            followers: Option<i64>,
            monthly_listeners: Option<i64>,
            world_rank: Option<i64>,
        }
        #[derive(Clone, Data, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Visuals {
            avatar_image: Option<AvatarImage>,
        }
        #[derive(Clone, Data, Deserialize)]
        struct AvatarImage {
            #[serde(default)]
            sources: Vector<Image>,
        }
        #[derive(Clone, Data, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RelatedContent {
            #[serde(default)]
            related_artists: RelatedArtists,
        }
        #[derive(Clone, Data, Default, Deserialize)]
        struct RelatedArtists {
            #[serde(default)]
            items: Vector<RelatedArtist>,
        }
        // Related artists carry the same `visuals` shape as the artist itself.
        #[derive(Clone, Data, Deserialize)]
        struct RelatedArtist {
            id: Arc<str>,
            profile: RelatedProfile,
            visuals: Option<Visuals>,
        }
        #[derive(Clone, Data, Deserialize)]
        struct RelatedProfile {
            name: Arc<str>,
        }

        let json = json!({
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": "1ac33ddab5d39a3a9c27802774e6d78b9405cc188c6f75aed007df2a32737c72"
                }
            },
            "operationName": "queryArtistOverview",
            "variables": {
                "locale": "",
                "uri": format!("spotify:artist:{id}"),
            },
        });
        let request =
            &RequestBuilder::new("pathfinder/v2/query".to_string(), Method::Post, Some(json))
                .set_base_uri("api-partner.spotify.com")
                .header("User-Agent", Self::user_agent())
                // `api-partner` rejects the Web API OAuth token with a 403; it
                // needs the first-party Login5 bearer + client-token.
                .partner_auth();
        // The cache has no versioning, so the bucket name is effectively the
        // schema version: changing the shape parsed here needs a new one.
        let result: Cached<Welcome> = self.load_cached(request, "artist-overview", id)?;
        Ok(result.map(|welcome| {
            let union = welcome.data.artist_union;

            let main_image = union
                .visuals
                .and_then(|visuals| visuals.avatar_image)
                .and_then(|image| image.sources.into_iter().next())
                .map(|source| source.url)
                .unwrap_or_else(|| Arc::from(""));

            let profile = union.profile.unwrap_or_default();
            let stats = union.stats.unwrap_or_default();

            let bio = profile
                .biography
                .and_then(|biography| biography.text)
                .map(|text| {
                    let sanitized = sanitize_str(&DEFAULT, &text).unwrap_or_default();
                    sanitized.replace("&amp;", "&")
                })
                .unwrap_or_default();

            let artist_links = profile
                .external_links
                .items
                .into_iter()
                .map(|link| link.url)
                .collect();

            let related = union
                .related_content
                .map(|content| content.related_artists.items)
                .unwrap_or_default()
                .into_iter()
                .map(|artist| Artist {
                    id: artist.id,
                    name: artist.profile.name,
                    images: artist
                        .visuals
                        .and_then(|visuals| visuals.avatar_image)
                        .map(|image| image.sources)
                        .unwrap_or_default(),
                })
                .collect();

            ArtistOverview {
                info: ArtistInfo {
                    main_image,
                    stats: ArtistStats {
                        followers: stats.followers.unwrap_or(0),
                        monthly_listeners: stats.monthly_listeners.unwrap_or(0),
                        world_rank: stats.world_rank.unwrap_or(0),
                    },
                    bio,
                    artist_links,
                },
                related,
            }
        }))
    }
}

/// Album endpoints.
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-an-album/
    pub fn get_album(&self, id: &str) -> Result<Cached<Arc<Album>>, Error> {
        let request = &RequestBuilder::new(format!("v1/albums/{id}"), Method::Get, None)
            .query("market", "from_token");
        match self.load_cached(request, "album", id) {
            Ok(result) => Ok(result),
            // Like playlists, fall back to pathfinder when the Web API fails.
            Err(err) => {
                log::warn!("Web API failed for album {id} ({err}), trying pathfinder");
                self.get_pathfinder_album(id)
                    .map(|album| Cached::fresh(Arc::new(album)))
                    .map_err(|fallback_err| {
                        log::warn!("pathfinder failed for album {id}: {fallback_err}");
                        err
                    })
            }
        }
    }

    #[cfg(test)]
    pub fn get_pathfinder_album_for_tests(&self, id: &str) -> Result<Album, Error> {
        self.get_pathfinder_album(id)
    }

    fn get_pathfinder_album(&self, id: &str) -> Result<Album, Error> {
        const PAGE_SIZE: usize = 50;

        let page = |offset: usize| -> Result<pathfinder::AlbumResponse, Error> {
            self.pathfinder_query(
                pathfinder::GET_ALBUM,
                json!({
                    "uri": format!("spotify:album:{id}"),
                    "locale": "",
                    "offset": offset,
                    "limit": PAGE_SIZE,
                }),
            )
        };
        let first = page(0)?;
        let total = first.total_tracks();
        let mut offset = first.page_len();
        let mut album = first
            .into_album()
            .ok_or_else(|| Error::WebApiError("Album not found".to_string()))?;
        while offset < total {
            let next = page(offset)?;
            let len = next.page_len();
            if len == 0 {
                break;
            }
            album.tracks.extend(next.into_tracks());
            offset += len;
        }
        Ok(album)
    }
}

/// Show endpoints. (Podcasts)
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-a-show/Add commentMore actions
    pub fn get_show(&self, id: &str) -> Result<Cached<Arc<Show>>, Error> {
        let request = &RequestBuilder::new(format!("v1/shows/{id}"), Method::Get, None)
            .query("market", "from_token");

        let result = self.load_cached(request, "show", id)?;

        Ok(result)
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-multiple-episodes
    pub fn get_episodes(
        &self,
        ids: impl IntoIterator<Item = EpisodeId>,
    ) -> Result<Vector<Arc<Episode>>, Error> {
        #[derive(Deserialize)]
        struct Episodes {
            episodes: Vector<Arc<Episode>>,
        }

        let request = &RequestBuilder::new("v1/episodes", Method::Get, None)
            .query("ids", ids.into_iter().map(|id| id.0.to_base62()).join(","))
            .query("market", "from_token");
        let result: Episodes = self.load(request)?;
        Ok(result.episodes)
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-a-shows-episodes
    pub fn get_show_episodes(&self, id: &str) -> Result<Vector<Arc<Episode>>, Error> {
        let request = &RequestBuilder::new(format!("v1/shows/{id}/episodes"), Method::Get, None)
            .query("market", "from_token");

        let mut results = Vector::new();
        self.for_all_pages(request, |page: Page<Option<EpisodeLink>>| {
            if !page.items.is_empty() {
                let ids = page
                    .items
                    .into_iter()
                    .filter_map(|link| link.map(|link| link.id));
                let episodes = self.get_episodes(ids)?;
                results.append(episodes);
            }
            Ok(())
        })?;

        Ok(results)
    }
}

/// Track endpoints.
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-track
    pub fn get_track(&self, id: &str) -> Result<Arc<Track>, Error> {
        let request = &RequestBuilder::new(format!("v1/tracks/{id}"), Method::Get, None)
            .query("market", "from_token");
        self.load(request)
    }

    pub fn get_track_credits(&self, track_id: &str) -> Result<TrackCredits, Error> {
        let request = &RequestBuilder::new(
            format!("track-credits-view/v0/experimental/{track_id}/credits"),
            Method::Get,
            None,
        )
        .set_base_uri("spclient.wg.spotify.com")
        // `spclient` answers 403 to the Web API OAuth token.
        .partner_auth();
        let result: TrackCredits = self.load(request)?;
        Ok(result)
    }

    pub fn get_lyrics(&self, track_id: String) -> Result<Vector<TrackLines>, Error> {
        #[derive(Default, Debug, Clone, PartialEq, Deserialize, Data)]
        #[serde(rename_all = "camelCase")]
        pub struct Root {
            pub lyrics: Lyrics,
        }

        #[derive(Default, Debug, Clone, PartialEq, Deserialize, Data)]
        #[serde(rename_all = "camelCase")]
        pub struct Lyrics {
            pub lines: Vector<TrackLines>,
            pub provider: String,
            pub provider_lyrics_id: String,
        }

        let request = &RequestBuilder::new(
            format!("color-lyrics/v2/track/{track_id}"),
            Method::Get,
            None,
        )
        .set_base_uri("spclient.wg.spotify.com")
        .query("format", "json")
        .query("vocalRemoval", "false")
        .query("market", "from_token")
        .header("app-platform", "WebPlayer")
        // `spclient` answers 403 to the Web API OAuth token.
        .partner_auth();

        let lyrics: Cached<Root> = self.load_cached(request, "lyrics", &track_id)?;
        Ok(lyrics.data.lyrics.lines)
    }
}

/// Library endpoints.
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-users-saved-albums/
    pub fn get_saved_albums(&self) -> Result<Vector<Arc<Album>>, Error> {
        #[derive(Clone, Deserialize)]
        struct SavedAlbum {
            album: Arc<Album>,
        }

        let request =
            &RequestBuilder::new("v1/me/albums", Method::Get, None).query("market", "from_token");

        Ok(self
            .load_all_pages(request)?
            .into_iter()
            .map(|item: SavedAlbum| item.album)
            .collect())
    }

    // https://developer.spotify.com/documentation/web-api/reference/save-to-library/
    pub fn save_album(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Put, &format!("spotify:album:{id}"));
        self.send_empty_json(request)
    }

    // https://developer.spotify.com/documentation/web-api/reference/remove-from-library/
    pub fn unsave_album(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Delete, &format!("spotify:album:{id}"));
        self.send_empty_json(request)
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-users-saved-tracks/
    /// Every liked song, newest first, whatever the paginated limit: a liked track
    /// missing here shows as not liked (one account has 4000 of them).
    pub fn get_saved_tracks(&self) -> Result<Vector<Arc<Track>>, Error> {
        self.with_offline_copy("offline", "saved-tracks", || self.fetch_saved_tracks())
            .map(numbered)
    }

    fn fetch_saved_tracks(&self) -> Result<Vector<Arc<Track>>, Error> {
        // The web player reads them as this playlist, through pathfinder.  The Web
        // API needs 81 requests for 4000 of them, and after a day of that Spotify
        // blocked the library endpoints of the user's API app for 18 hours.
        const LIKED_SONGS: &str = "37i9dQZF1F5p3rmiWPIYgZ";
        match self.get_pathfinder_playlist_tracks(LIKED_SONGS, true) {
            Ok(tracks) => return Ok(tracks),
            Err(err) => log::warn!("pathfinder refused liked songs ({err}), using Web API"),
        }

        #[derive(Clone, Deserialize)]
        struct SavedTrack {
            track: Arc<Track>,
        }
        // No market: tracks keep their own IDs, as pathfinder and playlists give
        // them, so the liked marks match.
        let request = &RequestBuilder::new("v1/me/tracks", Method::Get, None);
        Ok(self
            .load_every_page(request)?
            .into_iter()
            .enumerate()
            .map(|(index, item): (usize, SavedTrack)| {
                // Newest first, which is the "date added" order.
                let mut track = item.track;
                Arc::make_mut(&mut track).track_pos = index;
                track
            })
            .collect())
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-users-saved-shows
    pub fn get_saved_shows(&self) -> Result<Vector<Arc<Show>>, Error> {
        #[derive(Clone, Deserialize)]
        struct SavedShow {
            show: Arc<Show>,
        }

        let request =
            &RequestBuilder::new("v1/me/shows", Method::Get, None).query("market", "from_token");

        Ok(self
            .load_all_pages(request)?
            .into_iter()
            .map(|item: SavedShow| item.show)
            .collect())
    }

    // https://developer.spotify.com/documentation/web-api/reference/save-to-library/
    pub fn save_track(&self, id: &str) -> Result<(), Error> {
        // `PUT /v1/me/tracks` answers 403 now.
        let request = &library_request(Method::Put, &format!("spotify:track:{id}"));
        self.send_empty_json(request)
    }

    /// Adds tracks to Liked Songs, 40 per request (the endpoint's limit).
    pub fn save_tracks(&self, tracks: &[Arc<Track>]) -> Result<(), Error> {
        for chunk in tracks.chunks(40) {
            let uris: Vec<_> = chunk
                .iter()
                .map(|track| {
                    urlencoding::encode(&format!("spotify:track:{}", track.id.0.to_base62()))
                        .into_owned()
                })
                .collect();
            let request = &RequestBuilder::new("v1/me/library", Method::Put, None)
                .query("uris", uris.join(","));
            self.send_empty_json(request)?;
        }
        Ok(())
    }

    // https://developer.spotify.com/documentation/web-api/reference/remove-from-library/
    pub fn unsave_track(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Delete, &format!("spotify:track:{id}"));
        self.send_empty_json(request)
    }

    // https://developer.spotify.com/documentation/web-api/reference/save-to-library/
    pub fn save_show(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Put, &format!("spotify:show:{id}"));
        self.send_empty_json(request)
    }

    // https://developer.spotify.com/documentation/web-api/reference/remove-from-library/
    pub fn unsave_show(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Delete, &format!("spotify:show:{id}"));
        self.send_empty_json(request)
    }
}

/// View endpoints.
impl WebApi {
    pub fn get_user_info(&self) -> Result<(String, String), Error> {
        #[derive(Deserialize, Serialize)]
        struct User {
            region: String,
            timezone: String,
        }
        // A third party over plain HTTP: never send it the Spotify token.
        let user = match self.cache_get_json::<User>("user-info", "usrinfo") {
            Some(user) => user,
            None => {
                let user: User = self
                    .get_public_json("http://ip-api.com/json?fields=260")?
                    .ok_or_else(|| Error::WebApiError("ip-api.com: not found".to_string()))?;
                self.cache_set_json("user-info", "usrinfo", &user);
                user
            }
        };
        Ok((user.region, user.timezone))
    }

    pub fn get_section(&self, section_uri: &str) -> Result<MixedView, Error> {
        let (country, time_zone) = self.get_user_info()?;
        let access_token = self.access_token()?;

        let json = json!({
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": "5fb7da7a03ee2776c0670856609cb85a189a3cecc36cc160b91cde79063b9cf1"
                }
            },
            "operationName": "homeSection",
            "variables":  {
                "sectionItemsLimit": 20,
                "sectionItemsOffset": 0,
                "sp_t": access_token,
                "timeZone": time_zone,
                "country": country,
                "uri": section_uri,
                "homeEndUserIntegration": "INTEGRATION_WEB_PLAYER"
            },
        });

        let request =
            &RequestBuilder::new("pathfinder/v2/query".to_string(), Method::Post, Some(json))
                .set_base_uri("api-partner.spotify.com")
                .header("User-Agent", Self::user_agent())
                .partner_auth();

        // Extract the playlists
        self.load_and_return_home_section(request)
    }

    pub fn get_made_for_you(&self) -> Result<MixedView, Error> {
        // 0JQ5DAUnp4wcj0bCb3wh3S -> Made for you
        self.get_section("spotify:section:0JQ5DAUnp4wcj0bCb3wh3S")
    }

    pub fn get_top_mixes(&self) -> Result<MixedView, Error> {
        // 0JQ5DAnM3wGh0gz1MXnu89 -> Top mixes
        self.get_section("spotify:section:0JQ5DAnM3wGh0gz1MXnu89")
    }

    pub fn recommended_stations(&self) -> Result<MixedView, Error> {
        // 0JQ5DAnM3wGh0gz1MXnu3R -> Recommended stations
        self.get_section("spotify:section:0JQ5DAnM3wGh0gz1MXnu3R")
    }

    pub fn uniquely_yours(&self) -> Result<MixedView, Error> {
        // 0JQ5DAUnp4wcj0bCb3wh3S -> Uniquely yours
        self.get_section("spotify:section:0JQ5DAUnp4wcj0bCb3wh3S")
    }

    pub fn best_of_artists(&self) -> Result<MixedView, Error> {
        // 0JQ5DAnM3wGh0gz1MXnu3n -> Best of artists
        self.get_section("spotify:section:0JQ5DAnM3wGh0gz1MXnu3n")
    }

    // Need to make a mix of it!
    pub fn jump_back_in(&self) -> Result<MixedView, Error> {
        // 0JQ5DAIiKWzVFULQfUm85X -> Jump back in
        self.get_section("spotify:section:0JQ5DAIiKWzVFULQfUm85X")
    }

    // Shows
    pub fn your_shows(&self) -> Result<MixedView, Error> {
        // 0JQ5DAnM3wGh0gz1MXnu3N -> Your shows
        self.get_section("spotify:section:0JQ5DAnM3wGh0gz1MXnu3N")
    }

    pub fn shows_that_you_might_like(&self) -> Result<MixedView, Error> {
        // 0JQ5DAnM3wGh0gz1MXnu3P -> Shows that you might like
        self.get_section("spotify:section:0JQ5DAnM3wGh0gz1MXnu3P")
    }
}

/// Playlist endpoints.
impl WebApi {
    /// All playlists in the library, pinned ones first, as the official client
    /// shows them.  The public Web API leaves out Spotify-generated playlists
    /// (Daily Mixes and other mixes) and knows nothing about pins, so this asks
    /// pathfinder first and only falls back to the Web API.
    /// `order` is one of the library's sort orders, like "Alphabetical".
    pub fn get_playlists(&self, order: &str) -> Result<Vector<Playlist>, Error> {
        self.with_offline_copy("offline", "playlists", || {
            match self.get_library_playlists(order) {
                Ok(playlists) => Ok(playlists),
                Err(err) => {
                    log::warn!("failed to load library from pathfinder, using Web API: {err}");
                    self.get_web_api_playlists()
                }
            }
        })
    }

    /// `fetch`, keeping a copy to show when it fails, e.g. offline: the playlists
    /// downloaded for offline listening need their pages too.
    fn with_offline_copy<T: Serialize + DeserializeOwned>(
        &self,
        bucket: &str,
        key: &str,
        fetch: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        match fetch() {
            Ok(value) => {
                self.cache_set_json(bucket, key, &value);
                Ok(value)
            }
            Err(err) => {
                let copy = self.cache_get_json(bucket, key);
                if copy.is_some() {
                    log::warn!("showing the saved copy of {bucket}/{key}: {err}");
                }
                copy.ok_or(err)
            }
        }
    }

    /// The URIs of the user's rootlist, the library's own order ("Custom
    /// Order"), and its revision.
    pub(super) fn rootlist(&self, user: &str) -> Result<(Vec<String>, Vec<u8>), Error> {
        use librespot_protocol::playlist4_external::SelectedListContent;
        use protobuf::Message;

        let request = &RequestBuilder::new(
            format!("playlist/v2/user/{user}/rootlist"),
            Method::Get,
            None,
        )
        .set_base_uri("spclient.wg.spotify.com")
        .query("decorate", "revision")
        .query("from", 0)
        .query("length", 10_000)
        .partner_auth();
        let response = Self::with_retry(|| self.request(request))?;
        let mut body = Vec::new();
        response.into_body().into_reader().read_to_end(&mut body)?;
        let list = SelectedListContent::parse_from_bytes(&body)
            .map_err(|err| Error::WebApiError(format!("rootlist: {err}")))?;
        let uris = list
            .contents
            .items
            .iter()
            .map(|item| item.uri().to_string());
        Ok((uris.collect(), list.revision().to_vec()))
    }

    /// Moves `uri` in the library's own order, right before or after `anchor`.
    pub fn move_in_rootlist(
        &self,
        user: &str,
        uri: &str,
        anchor: &str,
        before: bool,
    ) -> Result<(), Error> {
        use librespot_protocol::playlist4_external::{op, Delta, ListChanges, Mov, Op};
        use protobuf::{Message, MessageField};

        let (uris, revision) = self.rootlist(user)?;
        let index = |uri: &str| {
            uris.iter()
                .position(|item| item == uri)
                .ok_or_else(|| Error::WebApiError(format!("{uri} isn't in the library")))
        };
        let mut mov = Mov::new();
        mov.set_from_index(index(uri)? as i32);
        mov.set_length(1);
        // The index to insert at, counted before the move.
        let to = index(anchor)? + usize::from(!before);
        mov.set_to_index(to as i32);
        let mut op = Op::new();
        op.set_kind(op::Kind::MOV);
        op.mov = MessageField::some(mov);
        let mut delta = Delta::new();
        delta.ops.push(op);
        let mut changes = ListChanges::new();
        changes.set_base_revision(revision);
        changes.deltas.push(delta);
        let body = changes
            .write_to_bytes()
            .map_err(|err| Error::WebApiError(format!("rootlist change: {err}")))?;

        let (bearer, client_token) = self.partner_token()?;
        let url =
            format!("https://spclient.wg.spotify.com/playlist/v2/user/{user}/rootlist/changes");
        Self::with_retry(|| {
            self.agent
                .post(&url)
                .header("Authorization", &format!("Bearer {bearer}"))
                .header("client-token", &client_token)
                .header("Content-Type", "application/x-protobuf")
                .send(&body[..])
                .map_err(|err| Error::WebApiError(err.to_string()))
        })?;
        Ok(())
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-a-list-of-current-users-playlists
    fn get_web_api_playlists(&self) -> Result<Vector<Playlist>, Error> {
        let request = &RequestBuilder::new("v1/me/playlists", Method::Get, None);
        let result: Vector<Playlist> = self.load_all_pages(request)?;
        Ok(result)
    }

    fn get_library_playlists(&self, order: &str) -> Result<Vector<Playlist>, Error> {
        // Include the playlists inside of folders.
        let playlists =
            self.library_v3_items("Playlists", true, Some(order), |page| page.into_playlists())?;
        // Pinned playlists come first already, but don't rely on it.
        let (pinned, rest): (Vector<_>, Vector<_>) = playlists.into_iter().partition(|p| p.pinned);
        let mut playlists = pinned;
        playlists.append(rest);
        Ok(playlists)
    }

    /// The artists the user follows, in library order.
    pub fn get_followed_artists(&self) -> Result<Vector<Artist>, Error> {
        self.library_v3_items("Artists", false, None, |page| page.into_artists())
    }

    /// Every item of the library filtered with `filter`, page by page.
    fn library_v3_items<T: Clone>(
        &self,
        filter: &str,
        flatten: bool,
        order: Option<&str>,
        items: fn(pathfinder::LibraryResponse) -> Result<Vec<T>, String>,
    ) -> Result<Vector<T>, Error> {
        const PAGE_SIZE: usize = 50;

        let mut all = Vector::new();
        let mut offset = 0;
        loop {
            let response: pathfinder::LibraryResponse = self.pathfinder_query(
                pathfinder::LIBRARY_V3,
                json!({
                    "filters": [filter],
                    "order": order,
                    "textFilter": null,
                    "features": [],
                    "limit": PAGE_SIZE,
                    "offset": offset,
                    "flatten": flatten,
                    "folderUri": null,
                    "includeFoldersWhenFlattening": false,
                }),
            )?;
            let total = response.total_count();
            let page_len = response.item_count();
            all.extend(items(response).map_err(Error::WebApiError)?);
            offset += page_len;
            if page_len == 0 || offset >= total || offset >= self.paginated_limit {
                break;
            }
        }
        Ok(all)
    }

    fn get_pathfinder_playlist_page(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<pathfinder::PlaylistResponse, Error> {
        self.pathfinder_query(
            pathfinder::FETCH_PLAYLIST,
            json!({
                "uri": format!("spotify:playlist:{id}"),
                "offset": offset,
                "limit": limit,
                "enableWatchFeedEntrypoint": false,
                "includeEpisodeContentRatingsV2": false,
            }),
        )
    }

    fn get_pathfinder_playlist_tracks(
        &self,
        id: &str,
        all: bool,
    ) -> Result<Vector<Arc<Track>>, Error> {
        const PAGE_SIZE: usize = 100;
        let limit = if all {
            usize::MAX
        } else {
            self.paginated_limit
        };

        let local = Some(LocalLibrary::global());
        let page = |offset| self.get_pathfinder_playlist_page(id, offset, PAGE_SIZE);
        let first = page(0)?;
        let total = first.total_count().min(limit);
        let offsets: Vec<_> = (PAGE_SIZE..total).step_by(PAGE_SIZE).collect();
        // Each page becomes tracks as it comes: holding the parsed responses of
        // 4000 liked songs until the last one took a few hundred MB.
        let rest = fetch_pages(&offsets, |offset| Ok(page(offset)?.into_tracks(local)))?;
        let mut tracks = Vector::new();
        for page_tracks in std::iter::once(first.into_tracks(local)).chain(rest) {
            for mut track in page_tracks {
                // Keeps the playlist order for sorting by "date added".
                Arc::make_mut(&mut track).track_pos = tracks.len();
                tracks.push_back(track);
            }
        }
        Ok(tracks)
    }

    /// Send a persisted pathfinder (GraphQL) query to `api-partner`.
    fn pathfinder_query<T: DeserializeOwned>(
        &self,
        (operation, hash): (&str, &str),
        variables: serde_json::Value,
    ) -> Result<T, Error> {
        let json = json!({
            "operationName": operation,
            "variables": variables,
            "extensions": {
                "persistedQuery": { "version": 1, "sha256Hash": hash }
            },
        });
        let request =
            &RequestBuilder::new("pathfinder/v2/query".to_string(), Method::Post, Some(json))
                .set_base_uri("api-partner.spotify.com")
                .header("User-Agent", Self::user_agent())
                .partner_auth();
        self.load(request)
    }

    pub fn follow_playlist(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Put, &format!("spotify:playlist:{id}"));
        self.send_empty_json(request)?;
        Ok(())
    }

    pub fn unfollow_playlist(&self, id: &str) -> Result<(), Error> {
        let request = &library_request(Method::Delete, &format!("spotify:playlist:{id}"));
        self.send_empty_json(request)?;
        Ok(())
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-playlist
    pub fn get_playlist(&self, id: &str) -> Result<Playlist, Error> {
        let request = &RequestBuilder::new(format!("v1/playlists/{id}"), Method::Get, None);
        match self.load(request).and_then(playlist_from_json) {
            Ok(playlist) => Ok(playlist),
            // Spotify-generated mixes are a 404 on the Web API for third-party
            // apps, but pathfinder serves them.
            Err(err) => self
                .get_pathfinder_playlist_page(id, 0, 0)
                .ok()
                .and_then(|response| response.playlist())
                .ok_or(err),
        }
    }

    /// The playlist's tracks, up to the paginated limit, for showing it.
    pub fn get_playlist_tracks(&self, id: &str) -> Result<Vector<Arc<Track>>, Error> {
        let tracks =
            self.with_offline_copy("offline-playlist", id, || self.playlist_tracks(id, false));
        tracks.map(numbered)
    }

    /// Every track of the playlist, for downloading or liking all of it.
    pub fn get_all_playlist_tracks(&self, id: &str) -> Result<Vector<Arc<Track>>, Error> {
        self.playlist_tracks(id, true)
    }

    fn playlist_tracks(&self, id: &str, all: bool) -> Result<Vector<Arc<Track>>, Error> {
        match self.get_web_api_playlist_tracks(id, all) {
            Ok(tracks) => Ok(tracks),
            Err(err) => {
                log::info!("Web API refused playlist {id} ({err}), trying pathfinder");
                self.get_pathfinder_playlist_tracks(id, all)
                    .map_err(|_| err)
            }
        }
    }

    // https://developer.spotify.com/documentation/web-api/reference/get-playlist-items
    fn get_web_api_playlist_tracks(
        &self,
        id: &str,
        all: bool,
    ) -> Result<Vector<Arc<Track>>, Error> {
        #[derive(Clone, Deserialize)]
        struct PlaylistItem {
            #[serde(default)]
            item: Option<OptionalTrack>,
            #[serde(default)]
            track: Option<OptionalTrack>,
        }

        // Spotify API likes to return _really_ bogus data for local tracks. Much better
        // would be to ignore parsing this completely if `is_local` is true, but this
        // will do as well.
        #[derive(Clone, Deserialize)]
        #[serde(untagged)]
        enum OptionalTrack {
            Track(Arc<Track>),
            Json(serde_json::Value),
        }

        // No market (it was misspelled "marker" before, so never sent): tracks keep
        // their own IDs, as the liked songs have them, so liked marks match.
        let request = &RequestBuilder::new(format!("v1/playlists/{id}/items"), Method::Get, None)
            .query("additional_types", "track");

        let result: Vector<PlaylistItem> = if all {
            self.load_every_page(request)?
        } else {
            self.load_all_pages(request)?
        };

        let local_library = LocalLibrary::global();

        Ok(result
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let track_source = item.item.or(item.track);
                let mut track = match track_source {
                    // Local items usually fail to parse as regular tracks because
                    // of their `null` IDs, but in case one doesn't, re-route it.
                    Some(OptionalTrack::Track(track)) if track.is_local => {
                        local_library.track_from_local_track(&track)
                    }
                    Some(OptionalTrack::Track(track)) => track,
                    Some(OptionalTrack::Json(json)) => {
                        local_library.track_from_playlist_item(&json)?
                    }
                    None => return None,
                };
                Arc::make_mut(&mut track).track_pos = index;
                Some(track)
            })
            .collect())
    }

    // https://developer.spotify.com/documentation/web-api/reference/change-playlist-details
    pub fn change_playlist_details(&self, id: &str, name: &str) -> Result<(), Error> {
        let request = &RequestBuilder::new(format!("v1/playlists/{id}"), Method::Put, None)
            .set_body(Some(json!({ "name": name })));
        self.send_empty_json(request)?;
        Ok(())
    }

    // https://developer.spotify.com/documentation/web-api/reference/add-items-to-playlist
    pub fn add_track_to_playlist(&self, playlist_id: &str, track_uri: &str) -> Result<(), Error> {
        let request = &RequestBuilder::new(
            format!("v1/playlists/{playlist_id}/items"),
            Method::Post,
            Some(json!({"uris": [track_uri]})),
        );
        self.send_empty_json(request)
    }

    // https://developer.spotify.com/documentation/web-api/reference/remove-playlist-items
    pub fn remove_track_from_playlist(
        &self,
        playlist_id: &str,
        track_uri: &str,
    ) -> Result<(), Error> {
        let request = &RequestBuilder::new(
            format!("v1/playlists/{playlist_id}/items"),
            Method::Delete,
            Some(json!({ "items": [{ "uri": track_uri }] })),
        );
        self.send_empty_json(request)
    }
}

/// Search endpoints.
impl WebApi {
    /// Diagnostics only: the distinct `album_type` values of an album search.
    #[cfg(test)]
    pub fn raw_search_album_types(&self, query: &str) -> Result<Vec<String>, Error> {
        let request = &RequestBuilder::new("v1/search", Method::Get, None)
            .query("q", urlencoding::encode(query))
            .query("type", "album")
            .query("limit", "10");
        let value: serde_json::Value = self.load(request)?;
        let mut types: Vec<String> = value["albums"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|album| album["album_type"].as_str().map(String::from))
            .collect();
        types.sort();
        types.dedup();
        Ok(types)
    }

    // https://developer.spotify.com/documentation/web-api/reference/search/
    pub fn search(
        &self,
        query: &str,
        topics: &[SearchTopic],
        limit: usize,
    ) -> Result<SearchResults, Error> {
        #[derive(Deserialize)]
        struct ApiSearchResults {
            artists: Option<Page<Artist>>,
            albums: Option<Page<Arc<Album>>>,
            tracks: Option<Page<Arc<Track>>>,
            // Spotify returns `null` items for region-blocked/removed playlists.
            playlists: Option<Page<Option<Playlist>>>,
            shows: Option<Page<Arc<Show>>>,
        }

        let encoded_query = urlencoding::encode(query);
        let type_query_param = topics.iter().map(SearchTopic::as_str).join(",");
        let request = &RequestBuilder::new("v1/search", Method::Get, None)
            .query("q", encoded_query)
            .query("type", &type_query_param)
            .query("limit", limit.to_string())
            .query("market", "from_token");

        let result: ApiSearchResults = self.load(request)?;

        let artists = result.artists.map_or_else(Vector::new, |page| page.items);
        let albums = result.albums.map_or_else(Vector::new, |page| page.items);
        let tracks = result.tracks.map_or_else(Vector::new, |page| page.items);
        let playlists = result
            .playlists
            .map_or_else(Vector::new, |p| p.items.into_iter().flatten().collect());
        let shows = result.shows.map_or_else(Vector::new, |page| page.items);
        let topic = (topics.len() == 1).then_some(topics[0]);

        Ok(SearchResults {
            query: query.into(),
            topic,
            artists,
            albums,
            tracks,
            playlists,
            shows,
        })
    }

    /// The top artists and tracks of a genre by Last.fm's tags, found on Spotify:
    /// Spotify's own `genre:` filter misses the niche ones.
    pub fn search_genre(&self, tag: &str, lastfm_api_key: &str) -> Result<SearchResults, Error> {
        let (artists, tracks) = psst_core::lastfm::tag_top(lastfm_api_key, tag, 10, 20)
            .map_err(|err| Error::WebApiError(format!("Last.fm: {err}")))?;
        let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
        // A search per name, about 30, so only matching names are kept.
        let artists = fetch_pages(&(0..artists.len()).collect_vec(), |i| {
            let found = self.search(&artists[i], &[SearchTopic::Artist], 5)?;
            Ok(found
                .artists
                .into_iter()
                .find(|a| same(&a.name, &artists[i])))
        })?;
        let tracks = fetch_pages(&(0..tracks.len()).collect_vec(), |i| {
            let (artist, title) = &tracks[i];
            let query = format!(
                "track:\"{}\" artist:\"{}\"",
                title.replace('"', ""),
                artist.replace('"', "")
            );
            let found = self.search(&query, &[SearchTopic::Track], 5)?;
            Ok(found
                .tracks
                .into_iter()
                .find(|track| track.artists.iter().any(|a| same(&a.name, artist))))
        })?;
        Ok(SearchResults {
            query: tag.into(),
            topic: Some(SearchTopic::Genre),
            artists: artists.into_iter().flatten().collect(),
            albums: Vector::new(),
            tracks: tracks
                .into_iter()
                .flatten()
                .unique_by(|track| track.id)
                .collect(),
            playlists: Vector::new(),
            shows: Vector::new(),
        })
    }

    pub fn load_spotify_link(&self, link: &SpotifyUrl) -> Result<Nav, Error> {
        let nav = match link {
            SpotifyUrl::Playlist(id) => Nav::PlaylistDetail(self.get_playlist(id)?.link()),
            SpotifyUrl::Artist(id) => Nav::ArtistDetail(self.get_artist(id)?.link()),
            SpotifyUrl::Album(id) => Nav::AlbumDetail(self.get_album(id)?.data.link(), None),
            SpotifyUrl::Show(id) => Nav::ShowDetail(self.get_show(id)?.data.link()),
            SpotifyUrl::Track(id) => {
                let track = self.get_track(id)?;
                let album = track.album.clone().ok_or_else(|| {
                    Error::WebApiError("Track was found but has no album".to_string())
                })?;
                Nav::AlbumDetail(album, Some(track.id))
            }
        };
        Ok(nav)
    }
}

/// Track endpoints.
impl WebApi {
    // https://developer.spotify.com/documentation/web-api/reference/get-audio-analysis/
    pub fn _get_audio_analysis(&self, track_id: &str) -> Result<AudioAnalysis, Error> {
        let request =
            &RequestBuilder::new(format!("v1/audio-analysis/{track_id}"), Method::Get, None);
        let result = self.load_cached(request, "audio-analysis", track_id)?;
        Ok(result.data)
    }
}

/// Image endpoints.
impl WebApi {
    pub fn get_cached_image(&self, uri: &Arc<str>) -> Option<ImageBuf> {
        self.cache.get_image(uri)
    }

    pub fn get_image(&self, uri: Arc<str>) -> Result<ImageBuf, Error> {
        if let Some(cached_image) = self.cache.get_image(&uri) {
            return Ok(cached_image);
        }

        if let Some(disk_cached_image) = self.cache.get_image_from_disk(&uri) {
            self.cache.set_image(uri.clone(), disk_cached_image.clone());
            return Ok(disk_cached_image);
        }

        // A public CDN: no Spotify token, so covers don't depend on refreshing it.
        let response = Self::with_retry(|| {
            self.agent
                .get(uri.as_ref())
                .call()
                .map_err(|err| Error::WebApiError(err.to_string()))
        })?;
        let mut body = Vec::new();
        response.into_body().into_reader().read_to_end(&mut body)?;

        let format = match infer::get(body.as_slice()) {
            Some(kind) if kind.mime_type() == "image/jpeg" => Some(ImageFormat::Jpeg),
            Some(kind) if kind.mime_type() == "image/png" => Some(ImageFormat::Png),
            Some(kind) if kind.mime_type() == "image/webp" => Some(ImageFormat::WebP),
            _ => None,
        };

        // Save raw image data to disk cache
        self.cache.save_image_to_disk(&uri, &body);

        let image = if let Some(format) = format {
            image::load_from_memory_with_format(&body, format)?
        } else {
            image::load_from_memory(&body)?
        };
        let image_buf = ImageBuf::from_dynamic_image(image);
        self.cache.set_image(uri, image_buf.clone());
        Ok(image_buf)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::WebApiError(err.to_string())
    }
}

impl From<ureq::Error> for Error {
    fn from(err: ureq::Error) -> Self {
        Error::WebApiError(err.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::WebApiError(err.to_string())
    }
}

impl From<image::ImageError> for Error {
    fn from(err: image::ImageError) -> Self {
        Error::WebApiError(err.to_string())
    }
}

#[derive(Debug, Clone)]
enum Method {
    Post,
    Put,
    Delete,
    Get,
}

// Creating a new URI builder so aid in the creation of uris with extendable queries.
#[derive(Debug, Clone)]
struct RequestBuilder {
    base_uri: String,
    path: String,
    queries: HashMap<String, String>,
    headers: HashMap<String, String>,
    method: Method,
    body: Option<serde_json::Value>,
    // When set, authenticate with the first-party Login5 bearer + client-token
    // (for `api-partner.spotify.com`) instead of the Web API OAuth token.
    partner_auth: bool,
}

impl RequestBuilder {
    // By default, we use https and the api.spotify.com
    fn new(path: impl Display, method: Method, body: Option<serde_json::Value>) -> Self {
        Self {
            base_uri: "api.spotify.com".to_string(),
            path: path.to_string(),
            queries: HashMap::new(),
            headers: HashMap::new(),
            method,
            body,
            partner_auth: false,
        }
    }

    /// Authenticate this request with the first-party Login5 bearer +
    /// client-token, as required by `api-partner.spotify.com`.
    fn partner_auth(mut self) -> Self {
        self.partner_auth = true;
        self
    }

    fn query(mut self, key: impl Display, value: impl Display) -> Self {
        self.queries.insert(key.to_string(), value.to_string());
        self
    }

    fn header(mut self, key: impl Display, value: impl Display) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    fn get_headers(&self) -> &HashMap<String, String> {
        &self.headers
    }
    fn get_body(&self) -> Option<&serde_json::Value> {
        self.body.as_ref()
    }
    fn set_body(mut self, body: Option<serde_json::Value>) -> Self {
        self.body = body;
        self
    }
    fn get_method(&self) -> &Method {
        &self.method
    }
    #[allow(dead_code)]
    fn set_method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }
    fn set_base_uri(mut self, url: impl Display) -> Self {
        self.base_uri = url.to_string();
        self
    }
    fn build(&self) -> String {
        let mut url = format!("https://{}/{}", self.base_uri, self.path);
        if !self.queries.is_empty() {
            url.push('?');
            url.push_str(
                &self
                    .queries
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
        url
    }
}

#[cfg(test)]
mod load_cached_tests {
    use super::*;
    use std::{fs, time::SystemTime};

    #[test]
    fn refreshes_old_copies_but_falls_back_to_them() {
        let dir = std::env::temp_dir().join(format!("psst-cache-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // No Web API token, so every request fails right away, without network.
        let api = WebApi::new(None, Some(dir.clone()), 50);
        let request = &RequestBuilder::new("v1/artists/x", Method::Get, None);
        let load = || api.load_cached::<String>(request, "test", "key");
        let path = dir.join("test").join("key");

        api.cache.set("test", "key", br#""recent""#);
        assert_eq!(load().unwrap().data, "recent");
        // A reload asks once even for a recent copy, and keeps it when that fails.
        api.refresh_cached("test", "key");
        assert_eq!(load().unwrap().data, "recent");
        assert!(api.stale.lock().is_empty());

        // Too old to trust, but better than nothing when the refresh fails.
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(load().unwrap().data, "recent");

        // A copy cut short is ignored instead of failing forever.
        fs::write(&path, br#""rec"#).unwrap();
        assert!(load().is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn playlist_track_count_from_items_or_tracks() {
        let playlist = |page_key: &str| {
            serde_json::json!({
                "id": "p", "name": "P", "description": "", "collaborative": false,
                "owner": { "id": "u", "display_name": "U" }, "public": true,
                page_key: { "total": 214, "items": [] },
            })
        };
        let count = |key| playlist_from_json(playlist(key)).unwrap().track_count;
        assert_eq!(count("items"), Some(214));
        assert_eq!(count("tracks"), Some(214));
    }

    #[test]
    fn offline_copies_stand_in_for_failed_loads() {
        let dir = std::env::temp_dir().join(format!("psst-offline-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let api = WebApi::new(None, Some(dir.clone()), 50);
        let failed = || Err(Error::WebApiError("io: offline".into()));
        assert!(api.with_offline_copy::<Vec<u32>>("o", "k", failed).is_err());
        let fetched = api.with_offline_copy("o", "k", || Ok(vec![1, 2]));
        assert_eq!(fetched.unwrap(), [1, 2]);
        assert_eq!(
            api.with_offline_copy::<Vec<u32>>("o", "k", failed).unwrap(),
            [1, 2]
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod fetch_pages_tests {
    use super::*;

    #[test]
    fn keeps_the_page_order_and_fails_with_any_page() {
        // An odd count, so the threads get uneven shares.
        let offsets: Vec<usize> = (0..11).map(|i| i * 50).collect();
        let pages = fetch_pages(&offsets, |offset| Ok(offset / 50)).unwrap();
        assert_eq!(pages, (0..11).collect::<Vec<_>>());

        let failed = fetch_pages(&offsets, |offset| match offset {
            300 => Err(Error::WebApiError("io: reset".into())),
            _ => Ok(offset),
        });
        assert!(failed.is_err());
        assert!(fetch_pages(&[], Ok).unwrap().is_empty());
    }
}
