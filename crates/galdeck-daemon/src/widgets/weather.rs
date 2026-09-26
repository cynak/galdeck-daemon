//! Weather, from Open-Meteo.
//!
//! Chosen because it needs no account and no key, which means nothing secret
//! has to live in a config file that people paste into bug reports. It is
//! only ever asked about the coordinates the config gives: nothing here looks
//! up where the machine is.
//!
//! The one other thing asked of Open-Meteo is its place search, so that the
//! settings page can offer "Springfield, Illinois, US" instead of asking for
//! two decimal numbers. It sends only the name someone typed there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use galdeck_model::Units;
use serde::Deserialize;

const ENDPOINT: &str = "https://api.open-meteo.com/v1/forecast";
const GEOCODING_ENDPOINT: &str = "https://geocoding-api.open-meteo.com/v1/search";
/// Sent with every request, so Open-Meteo can tell who to contact about a
/// misbehaving client instead of blocking an address.
const USER_AGENT: &str = concat!("galdeck-daemon/", env!("CARGO_PKG_VERSION"));
/// How long a request may take, start to finish.
///
/// The worker does nothing else, so this only bounds how stale a failed
/// refresh can leave the previous reading.
const TIMEOUT: Duration = Duration::from_secs(15);
/// Today plus the next three, which is what fits across the screen.
const DAYS: usize = 4;
/// How long a place search may take, start to finish.
///
/// Much shorter than a forecast's: someone is watching a search box, and a
/// search that has not answered by now is better retried than waited on.
const GEOCODE_TIMEOUT: Duration = Duration::from_secs(4);
/// Largest answer read from the place search. Five places come to about 2K;
/// anything near this is not an answer to the question asked.
const GEOCODE_MAX_BYTES: u64 = 64 * 1024;
/// Places offered per search: enough to pick the right Springfield, few
/// enough to read at a glance.
const MAX_PLACES: usize = 5;
/// Longest a place's name, region or country is kept, in characters. Real
/// ones are far shorter; this only stops a strange answer from flooding the
/// settings page or the card.
const MAX_PLACE_TEXT: usize = 80;
/// Shortest search, in characters. Open-Meteo answers a single character
/// with nothing, so asking would only spend a request.
const MIN_QUERY_CHARS: usize = 2;
/// Longest search, in characters. Place names run far shorter; this bounds
/// what a stuck key or a stray paste can send.
const MAX_QUERY_CHARS: usize = 100;
/// Searches remembered, so retyping a name does not ask again.
const CACHE_ENTRIES: usize = 32;
/// Least time between two requests to the place search. It is a free service
/// shared by everyone; a settings page firing a search per keystroke must not
/// turn into a burst.
const GEOCODE_SPACING: Duration = Duration::from_secs(1);

/// What the sky is doing, coarsely enough to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Clear,
    PartlyCloudy,
    Cloudy,
    Fog,
    Drizzle,
    Rain,
    Snow,
    Storm,
}

impl Condition {
    /// From a WMO weather interpretation code, as Open-Meteo reports them.
    pub fn from_wmo(code: u8) -> Self {
        match code {
            0 => Condition::Clear,
            1 | 2 => Condition::PartlyCloudy,
            3 => Condition::Cloudy,
            45 | 48 => Condition::Fog,
            51..=57 => Condition::Drizzle,
            61..=67 | 80..=82 => Condition::Rain,
            71..=77 | 85 | 86 => Condition::Snow,
            95..=99 => Condition::Storm,
            // Codes outside the table are not something to guess at; cloud is
            // the least wrong picture of "something is happening".
            _ => Condition::Cloudy,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Day {
    /// Short weekday name, `Tue`.
    pub label: String,
    pub high: f64,
    pub low: f64,
    pub condition: Condition,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Weather {
    pub temperature: f64,
    pub condition: Condition,
    pub is_day: bool,
    pub units: Units,
    /// Today first.
    pub days: Vec<Day>,
}

impl Weather {
    /// `21°`, rounded, which is all a glance needs.
    pub fn degrees(value: f64) -> String {
        format!("{:.0}°", value)
    }
}

/// Holds the HTTP agent, so connections are reused between refreshes.
pub struct WeatherClient {
    agent: ureq::Agent,
}

impl Default for WeatherClient {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .into();
        Self { agent }
    }
}

impl WeatherClient {
    /// Fetch the current conditions. Blocking.
    pub fn fetch(&self, latitude: f64, longitude: f64, units: Units) -> Option<Weather> {
        let unit = match units {
            Units::Celsius => "celsius",
            Units::Fahrenheit => "fahrenheit",
        };
        let url = format!(
            "{ENDPOINT}?latitude={latitude:.4}&longitude={longitude:.4}\
             &current=temperature_2m,weather_code,is_day\
             &daily=weather_code,temperature_2m_max,temperature_2m_min\
             &timezone=auto&forecast_days={DAYS}&temperature_unit={unit}"
        );
        let body = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| log::warn!("fetching the weather: {e}"))
            .ok()?
            .body_mut()
            .read_to_string()
            .map_err(|e| log::warn!("reading the weather: {e}"))
            .ok()?;
        parse(&body, units)
    }
}

#[derive(Deserialize)]
struct Response {
    current: Current,
    daily: Daily,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f64,
    weather_code: u8,
    is_day: u8,
}

#[derive(Deserialize)]
struct Daily {
    time: Vec<String>,
    weather_code: Vec<Option<u8>>,
    temperature_2m_max: Vec<Option<f64>>,
    temperature_2m_min: Vec<Option<f64>>,
}

fn parse(body: &str, units: Units) -> Option<Weather> {
    let response: Response = serde_json::from_str(body)
        .map_err(|e| log::warn!("the weather service answered something unexpected: {e}"))
        .ok()?;
    let daily = &response.daily;
    let days = daily
        .time
        .iter()
        .enumerate()
        .filter_map(|(i, date)| {
            let label = date
                .parse::<jiff::civil::Date>()
                .ok()?
                .strftime("%a")
                .to_string();
            Some(Day {
                label,
                high: (*daily.temperature_2m_max.get(i)?)?,
                low: (*daily.temperature_2m_min.get(i)?)?,
                condition: Condition::from_wmo((*daily.weather_code.get(i)?)?),
            })
        })
        .collect();
    Some(Weather {
        temperature: response.current.temperature_2m,
        condition: Condition::from_wmo(response.current.weather_code),
        is_day: response.current.is_day != 0,
        units,
        days,
    })
}

/// A place the search found, offered on the settings page to fill in a
/// weather widget's coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub name: String,
    /// The state, province or region: `Illinois`.
    pub admin1: Option<String>,
    /// ISO 3166-1 alpha-2: `US`.
    pub country_code: Option<String>,
    pub country: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
}

impl Place {
    /// `Springfield, Illinois, US`: enough to tell the Springfields apart,
    /// short enough for one line. Missing parts are left out; the country's
    /// name stands in when its code is missing.
    pub fn label(&self) -> String {
        let country = self
            .country_code
            .as_deref()
            .filter(|code| !code.is_empty())
            .or(self.country.as_deref());
        [Some(self.name.as_str()), self.admin1.as_deref(), country]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Check a place name before it is sent anywhere, and tidy it.
///
/// Returns it trimmed. Control characters are refused rather than stripped:
/// nobody types them, so they mean a paste went wrong or something other
/// than a person is asking, and neither deserves a request.
pub fn validate_query(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.chars().any(char::is_control) {
        return Err("a place name cannot contain control characters".into());
    }
    let chars = name.chars().count();
    if chars < MIN_QUERY_CHARS {
        return Err(format!(
            "type at least {MIN_QUERY_CHARS} characters of the place's name"
        ));
    }
    if chars > MAX_QUERY_CHARS {
        return Err(format!(
            "a place name is at most {MAX_QUERY_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

#[derive(Deserialize)]
struct Search {
    /// Absent, not empty, when nothing matched. Kept as raw values so that
    /// an entry of the wrong shape (a missing field, a word where a number
    /// belongs) is dropped on its own instead of failing the lot. A number
    /// too large for an `f64` is not covered: see [`parse_places`].
    results: Option<Vec<serde_json::Value>>,
    /// Set, alongside a `reason`, when Open-Meteo refused the request.
    #[serde(default)]
    error: bool,
}

#[derive(Deserialize)]
struct Found {
    name: String,
    latitude: f64,
    longitude: f64,
    admin1: Option<String>,
    country_code: Option<String>,
    country: Option<String>,
}

impl Found {
    /// `None` for coordinates that are not somewhere on Earth, since the
    /// forecast would be asked about them next, and for a place with no
    /// name left once cleaned, since there would be nothing to offer.
    ///
    /// A coordinate gets here finite, if off the map (a latitude of 91): JSON
    /// has no way to write a non-finite number, and one that overflows an
    /// `f64` fails the whole answer before this is reached.
    fn into_place(self) -> Option<Place> {
        // `contains` is false for NaN, so this would hold even if a
        // non-finite value ever did get this far.
        let on_earth =
            (-90.0..=90.0).contains(&self.latitude) && (-180.0..=180.0).contains(&self.longitude);
        if !on_earth {
            return None;
        }
        Some(Place {
            name: clean(&self.name)?,
            admin1: self.admin1.as_deref().and_then(clean),
            country_code: self.country_code.as_deref().and_then(clean),
            country: self.country.as_deref().and_then(clean),
            latitude: self.latitude,
            longitude: self.longitude,
        })
    }
}

/// A string from the network made fit to show: no control characters, no
/// surrounding space, at most [`MAX_PLACE_TEXT`] characters (cut between
/// characters, never inside one), and `None` if nothing is left.
fn clean(text: &str) -> Option<String> {
    let visible: String = text.chars().filter(|c| !c.is_control()).collect();
    let kept: String = visible.trim().chars().take(MAX_PLACE_TEXT).collect();
    let kept = kept.trim_end();
    (!kept.is_empty()).then(|| kept.to_string())
}

/// Read the place search's answer: at most [`MAX_PLACES`], best match first.
///
/// An answer without `results` is how Open-Meteo says nothing matched, so it
/// is an empty list rather than an error. An entry of the wrong shape, or
/// with coordinates off the map, is dropped and the rest kept.
///
/// A number too large for an `f64` anywhere in the answer (`1e400`) makes
/// the whole of it an error instead. serde_json refuses such a number while
/// reading the document, before any entry is looked at on its own, and with
/// its default features there is no way to read past one. No real answer
/// carries one, so it is left an error rather than worked around.
pub fn parse_places(body: &str) -> Result<Vec<Place>, String> {
    let search: Search = serde_json::from_str(body)
        .map_err(|e| format!("the place search answered something unexpected: {e}"))?;
    if search.error {
        return Err("the place search refused the request".into());
    }
    Ok(search
        .results
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<Found>(entry).ok())
        .filter_map(Found::into_place)
        .take(MAX_PLACES)
        .collect())
}

/// Looks places up by name for the settings page.
///
/// One is shared by every request handler, so everything in it works through
/// `&self`.
pub struct Geocoder {
    agent: ureq::Agent,
    /// Lowercased query and what it found, oldest first.
    cache: Mutex<Vec<(String, Vec<Place>)>>,
    /// Set while a request is out.
    in_flight: AtomicBool,
    /// When the last request went out, to keep them [`GEOCODE_SPACING`] apart.
    last_request: Mutex<Option<Instant>>,
}

impl Default for Geocoder {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(GEOCODE_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .into();
        Self {
            agent,
            cache: Mutex::new(Vec::new()),
            in_flight: AtomicBool::new(false),
            last_request: Mutex::new(None),
        }
    }
}

impl Geocoder {
    /// Find places called `name`, best match first. Blocking, for at most
    /// about five seconds: one waiting its turn, four for the answer.
    ///
    /// Only one search goes out at a time. Another arriving meanwhile gets
    /// `Err("busy")` at once rather than queueing: searches come from someone
    /// typing, and by the time a queued one ran it would answer a name they
    /// had already typed past. Remembered answers are served even then.
    pub fn search(&self, name: &str) -> Result<Vec<Place>, String> {
        let query = validate_query(name)?;
        let key = query.to_lowercase();
        if let Some(places) = self.cached(&key) {
            return Ok(places);
        }
        let _flight = Flight::take(&self.in_flight).ok_or_else(|| "busy".to_string())?;
        self.wait_turn();
        let places = self.fetch(&query)?;
        self.remember(key, places.clone());
        Ok(places)
    }

    fn cached(&self, key: &str) -> Option<Vec<Place>> {
        let cache = self.cache.lock().expect("place cache poisoned");
        cache
            .iter()
            .find(|(cached, _)| cached == key)
            .map(|(_, places)| places.clone())
    }

    /// Keep an answer, forgetting the oldest once [`CACHE_ENTRIES`] are held.
    /// Empty answers are kept too: "nowhere is called that" does not change
    /// between keystrokes either.
    fn remember(&self, key: String, places: Vec<Place>) {
        let mut cache = self.cache.lock().expect("place cache poisoned");
        cache.retain(|(cached, _)| *cached != key);
        if cache.len() >= CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push((key, places));
    }

    /// Sleep until the next request may go out, and note that it is going.
    ///
    /// Only the holder of the flight gets here, so holding the lock through
    /// the sleep keeps nobody waiting.
    fn wait_turn(&self) {
        let mut last = self.last_request.lock().expect("place clock poisoned");
        std::thread::sleep(spacing_left(*last, Instant::now()));
        *last = Some(Instant::now());
    }

    fn fetch(&self, query: &str) -> Result<Vec<Place>, String> {
        // `query` percent-encodes; the name never becomes part of the URL
        // text any other way.
        let body = self
            .agent
            .get(GEOCODING_ENDPOINT)
            .query("name", query)
            .query("count", MAX_PLACES.to_string())
            .query("language", "en")
            .query("format", "json")
            .call()
            .map_err(|e| failed("searching for a place", e))?
            .body_mut()
            .with_config()
            .limit(GEOCODE_MAX_BYTES)
            .read_to_string()
            .map_err(|e| failed("reading the place search", e))?;
        parse_places(&body)
    }
}

/// Log a failed request and word it for whoever is waiting on the search.
/// The query is left out of the log: it may well be where someone lives.
fn failed(what: &str, e: ureq::Error) -> String {
    log::warn!("{what}: {e}");
    format!("{what} failed: {e}")
}

/// How much longer to wait before a request, given when the last one went.
fn spacing_left(last: Option<Instant>, now: Instant) -> Duration {
    last.map_or(Duration::ZERO, |at| {
        GEOCODE_SPACING.saturating_sub(now.saturating_duration_since(at))
    })
}

/// The geocoder's one request slot, freed when dropped: on success, on an
/// early return and on a panic alike, so a failed search never wedges the
/// next one.
struct Flight<'a>(&'a AtomicBool);

impl<'a> Flight<'a> {
    /// `None` if a request is already out.
    fn take(slot: &'a AtomicBool) -> Option<Self> {
        slot.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Flight(slot))
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
impl Geocoder {
    /// Remember an answer as a search for `name` would have.
    fn prime(&self, name: &str, places: Vec<Place>) {
        let query = validate_query(name).expect("a valid test query");
        self.remember(query.to_lowercase(), places);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "latitude": 51.5, "longitude": -0.12,
        "current": {"time": "2026-09-25T10:45", "interval": 900,
                    "temperature_2m": 18.4, "weather_code": 61, "is_day": 1},
        "daily": {"time": ["2026-09-25", "2026-09-26", "2026-09-27"],
                  "weather_code": [61, 0, null],
                  "temperature_2m_max": [19.1, 22.0, 20.0],
                  "temperature_2m_min": [11.0, 12.5, 10.0]}
    }"#;

    #[test]
    fn a_forecast_is_parsed() {
        let weather = parse(SAMPLE, Units::Celsius).unwrap();
        assert_eq!(weather.temperature, 18.4);
        assert_eq!(weather.condition, Condition::Rain);
        assert!(weather.is_day);
        // The day with no code is dropped rather than drawn as a guess.
        assert_eq!(weather.days.len(), 2);
        assert_eq!(weather.days[0].label, "Fri");
        assert_eq!(weather.days[1].condition, Condition::Clear);
        assert_eq!(weather.days[1].high, 22.0);
    }

    #[test]
    fn nonsense_is_refused_rather_than_drawn() {
        assert_eq!(parse("{\"error\": true}", Units::Celsius), None);
    }

    #[test]
    fn wmo_codes_map_to_something_drawable() {
        assert_eq!(Condition::from_wmo(0), Condition::Clear);
        assert_eq!(Condition::from_wmo(2), Condition::PartlyCloudy);
        assert_eq!(Condition::from_wmo(48), Condition::Fog);
        assert_eq!(Condition::from_wmo(81), Condition::Rain);
        assert_eq!(Condition::from_wmo(86), Condition::Snow);
        assert_eq!(Condition::from_wmo(96), Condition::Storm);
        assert_eq!(Condition::from_wmo(200), Condition::Cloudy);
    }

    /// The shape Open-Meteo's geocoding answers in, trimmed to three results.
    const PLACES: &str = r#"{
        "results": [
            {"id": 4250542, "name": "Springfield", "latitude": 39.80172,
             "longitude": -89.64371, "elevation": 182.0, "feature_code": "PPLA",
             "country_code": "US", "admin1_id": 4896861, "admin2_id": 4250548,
             "timezone": "America/Chicago", "population": 116565,
             "postcodes": ["62701", "62702"], "country_id": 6252001,
             "country": "United States", "admin1": "Illinois", "admin2": "Sangamon"},
            {"id": 4409896, "name": "Springfield", "latitude": 37.21533,
             "longitude": -93.29824, "elevation": 398.0, "feature_code": "PPLA2",
             "country_code": "US", "admin1_id": 4398678, "timezone": "America/Chicago",
             "population": 169176, "country_id": 6252001,
             "country": "United States", "admin1": "Missouri", "admin2": "Greene"},
            {"id": 2173911, "name": "Springfield", "latitude": -27.65,
             "longitude": 152.91667, "feature_code": "PPL", "country_code": "AU",
             "timezone": "Australia/Brisbane", "country_id": 2077456,
             "country": "Australia"}
        ],
        "generationtime_ms": 0.8740425
    }"#;

    fn springfield() -> Place {
        Place {
            name: "Springfield".into(),
            admin1: Some("Illinois".into()),
            country_code: Some("US".into()),
            country: Some("United States".into()),
            latitude: 39.80172,
            longitude: -89.64371,
        }
    }

    #[test]
    fn a_query_is_trimmed() {
        assert_eq!(
            validate_query("  Springfield \t\n").as_deref(),
            Ok("Springfield")
        );
        assert_eq!(validate_query("Rí").as_deref(), Ok("Rí"));
    }

    #[test]
    fn a_query_needs_two_characters_after_trimming() {
        assert!(validate_query("").is_err());
        assert!(validate_query("P").is_err());
        assert!(validate_query("   P   ").is_err());
    }

    #[test]
    fn a_query_is_at_most_a_hundred_characters() {
        assert!(validate_query(&"a".repeat(100)).is_ok());
        assert!(validate_query(&"a".repeat(101)).is_err());
        // Counted in characters, not bytes: 100 of these are 200 bytes.
        assert!(validate_query(&"é".repeat(100)).is_ok());
        assert!(validate_query(&"é".repeat(101)).is_err());
    }

    #[test]
    fn a_query_with_control_characters_is_refused() {
        assert!(validate_query("Spring\u{0}field").is_err());
        assert!(validate_query("New\nYork").is_err());
        assert!(validate_query("Paris\u{7f}x").is_err());
        assert!(validate_query("Paris\u{9b}x").is_err());
    }

    #[test]
    fn places_are_parsed() {
        let places = parse_places(PLACES).unwrap();
        assert_eq!(places.len(), 3);
        assert_eq!(places[0], springfield());
        assert_eq!(places[1].admin1.as_deref(), Some("Missouri"));
        assert_eq!(places[2].admin1, None);
        assert_eq!(places[2].latitude, -27.65);
    }

    #[test]
    fn no_results_is_no_places_rather_than_an_error() {
        assert_eq!(parse_places(r#"{"generationtime_ms": 0.53}"#), Ok(vec![]));
        assert_eq!(parse_places(r#"{"results": null}"#), Ok(vec![]));
        assert_eq!(parse_places(r#"{"results": []}"#), Ok(vec![]));
    }

    #[test]
    fn a_refusal_or_nonsense_is_an_error() {
        assert!(parse_places(r#"{"error": true, "reason": "Parameter count"}"#).is_err());
        assert!(parse_places("<html>").is_err());
        assert!(parse_places("[]").is_err());
    }

    #[test]
    fn places_off_the_earth_or_malformed_are_dropped() {
        let body = r#"{"results": [
            {"name": "North of north", "latitude": 90.5, "longitude": 0.0},
            {"name": "South of south", "latitude": -91.0, "longitude": 0.0},
            {"name": "Past the dateline", "latitude": 0.0, "longitude": 180.01},
            {"name": "Before it", "latitude": 0.0, "longitude": -181.0},
            {"name": "No longitude", "latitude": 10.0},
            {"name": "Words", "latitude": "north", "longitude": 0.0},
            {"name": "   ", "latitude": 1.0, "longitude": 1.0},
            {"name": "Null Island", "latitude": 0.0, "longitude": 0.0},
            {"name": "Corner", "latitude": -90.0, "longitude": 180.0}
        ]}"#;
        let names: Vec<_> = parse_places(body)
            .unwrap()
            .into_iter()
            .map(|place| place.name)
            .collect();
        assert_eq!(names, ["Null Island", "Corner"]);
    }

    #[test]
    fn a_coordinate_too_large_for_a_float_fails_the_whole_answer() {
        // Not dropped like the entries above: serde_json refuses the number
        // while reading the document, so the good place after it is lost too.
        // Pinned so that a change here is a decision, not a surprise.
        for huge in ["1e400", "-1e400"] {
            let body = format!(
                r#"{{"results": [
                    {{"name": "Huge", "latitude": {huge}, "longitude": 0.0}},
                    {{"name": "Fine", "latitude": 1.0, "longitude": 2.0}}
                ]}}"#
            );
            assert!(parse_places(&body).is_err(), "{huge} was read");
        }
        // Nor can JSON spell a non-finite number any other way.
        assert!(
            parse_places(r#"{"results": [{"name": "N", "latitude": NaN, "longitude": 0}]}"#)
                .is_err()
        );
    }

    #[test]
    fn at_most_five_places_are_kept() {
        let entry = r#"{"name": "Springfield", "latitude": 1.0, "longitude": 2.0}"#;
        let body = format!(r#"{{"results": [{}]}}"#, [entry; 7].join(","));
        assert_eq!(parse_places(&body).unwrap().len(), 5);
    }

    #[test]
    fn long_strings_are_cut_between_characters() {
        // `ü` is two bytes, so 80 characters run past 80 bytes: a cut by
        // bytes would keep too little, or split an `ü` and panic.
        let long = "Zürich".repeat(20);
        let body = serde_json::json!({"results": [{
            "name": long, "admin1": long, "country": long,
            "latitude": 47.37, "longitude": 8.55,
        }]})
        .to_string();
        let place = parse_places(&body).unwrap().remove(0);
        let expected: String = long.chars().take(80).collect();
        assert_eq!(place.name.chars().count(), 80);
        assert_eq!(place.name, expected);
        assert_eq!(place.admin1.as_deref(), Some(expected.as_str()));
        assert_eq!(place.country.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn strings_are_cleaned_of_control_characters_and_blanks() {
        let body = r#"{"results": [{
            "name": " Spring\u0000field\n", "admin1": "", "country_code": "U\u001bS",
            "latitude": 1.0, "longitude": 2.0
        }]}"#;
        let place = parse_places(body).unwrap().remove(0);
        assert_eq!(place.name, "Springfield");
        assert_eq!(place.admin1, None);
        assert_eq!(place.country_code.as_deref(), Some("US"));
    }

    #[test]
    fn a_label_skips_what_is_missing() {
        assert_eq!(springfield().label(), "Springfield, Illinois, US");
        let monaco = Place {
            name: "Monaco".into(),
            admin1: None,
            country_code: Some("MC".into()),
            country: Some("Monaco".into()),
            latitude: 43.73,
            longitude: 7.42,
        };
        assert_eq!(monaco.label(), "Monaco, MC");
        let codeless = Place {
            country_code: None,
            ..springfield()
        };
        assert_eq!(codeless.label(), "Springfield, Illinois, United States");
        let blank_code = Place {
            country_code: Some(String::new()),
            ..springfield()
        };
        assert_eq!(blank_code.label(), "Springfield, Illinois, United States");
        let bare = Place {
            admin1: Some(String::new()),
            country_code: None,
            country: None,
            ..springfield()
        };
        assert_eq!(bare.label(), "Springfield");
    }

    #[test]
    fn a_remembered_search_is_answered_without_a_request() {
        let geocoder = Geocoder::default();
        geocoder.prime("Springfield", vec![springfield()]);
        // With the slot held, anything that would need the network is turned
        // away as busy; so an answer here can only have come from the cache.
        let _held = Flight::take(&geocoder.in_flight).unwrap();
        assert_eq!(geocoder.search("  SPRINGFIELD "), Ok(vec![springfield()]));
        assert_eq!(geocoder.search("Shelbyville"), Err("busy".to_string()));
        assert_eq!(*geocoder.last_request.lock().unwrap(), None);
    }

    #[test]
    fn a_second_search_while_one_is_out_is_busy() {
        let geocoder = Geocoder::default();
        let held = Flight::take(&geocoder.in_flight).unwrap();
        assert_eq!(geocoder.search("Paris"), Err("busy".to_string()));
        // Being turned away must not free the slot the first search holds.
        assert!(geocoder.in_flight.load(Ordering::Acquire));
        assert!(Flight::take(&geocoder.in_flight).is_none());
        drop(held);
        assert!(!geocoder.in_flight.load(Ordering::Acquire));
        assert!(Flight::take(&geocoder.in_flight).is_some());
    }

    #[test]
    fn a_bad_query_is_refused_before_anything_else() {
        let geocoder = Geocoder::default();
        let _held = Flight::take(&geocoder.in_flight).unwrap();
        let refusal = geocoder.search("x").unwrap_err();
        assert_ne!(refusal, "busy");
    }

    #[test]
    fn the_cache_forgets_its_oldest_answer() {
        let geocoder = Geocoder::default();
        for i in 0..=CACHE_ENTRIES {
            geocoder.prime(&format!("place {i}"), vec![]);
        }
        assert_eq!(geocoder.cached("place 0"), None);
        assert_eq!(geocoder.cached("place 1"), Some(vec![]));
        assert_eq!(
            geocoder.cached(&format!("place {CACHE_ENTRIES}")),
            Some(vec![])
        );
        assert_eq!(geocoder.cache.lock().unwrap().len(), CACHE_ENTRIES);
        // Asking again replaces an answer rather than keeping two.
        geocoder.prime("Place 5", vec![springfield()]);
        assert_eq!(geocoder.cache.lock().unwrap().len(), CACHE_ENTRIES);
        assert_eq!(geocoder.cached("place 5"), Some(vec![springfield()]));
    }

    #[test]
    fn requests_are_spaced_a_second_apart() {
        let sent = Instant::now();
        assert_eq!(spacing_left(None, sent), Duration::ZERO);
        let soon_after = sent + Duration::from_millis(300);
        assert_eq!(
            spacing_left(Some(sent), soon_after),
            Duration::from_millis(700)
        );
        assert_eq!(
            spacing_left(Some(sent), sent + Duration::from_secs(5)),
            Duration::ZERO
        );
    }
}
