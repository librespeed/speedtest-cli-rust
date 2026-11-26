//! Core data types shared by the speed test.

pub mod bytes_counter;
pub mod server;
pub mod telemetry;

pub use server::Server;
pub use telemetry::{TelemetryExtra, TelemetryLog, TelemetryServer};

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::output::sanitize;

/// The program name, as reported by `--version`.
pub const PROG_NAME: &str = "librespeed-cli";

/// The name reported in the User-Agent header.
///
/// The Go client sends `librespeed-cli`, and this binary installs under that
/// same name, so sharing its User-Agent would leave a server's telemetry unable
/// to tell the two implementations apart. This matches the repository and
/// package name instead.
pub const UA_NAME: &str = "librespeed-cli-rust";

/// The program version.
pub const PROG_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// The build date, filled in by `build.rs`.
pub const BUILD_DATE: &str = env!("LIBRESPEED_BUILD_DATE");

/// The source revision, filled in by `build.rs`.
pub const REVISION: &str = env!("LIBRESPEED_REVISION");

/// The User-Agent sent with every request.
///
/// The platform is named as well as the program, the way a browser does. A
/// server's telemetry stores this header already, so reporting the operating
/// system and architecture is what lets an operator tell which kinds of machine
/// are measuring against them without changing what the client sends.
///
/// It stays coarse on purpose: the kernel version or the hostname would
/// identify the machine rather than describe it.
pub fn user_agent() -> String {
    format!(
        "{UA_NAME}/{PROG_VERSION} ({}; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// The JSON returned by a backend server's `getIP.php` endpoint.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GetIPResult {
    #[serde(rename = "processedString", default)]
    pub processed_string: String,
    // Kept verbatim rather than typed: a backend with ISP detection disabled
    // answers with an empty string here rather than an object, so a typed
    // field would reject the whole document and lose processedString with it.
    // Telemetry also sends this document back, and it should report what the
    // backend originally said, not this client's re-encoding of it. The typed
    // view is available through ip_info().
    #[serde(rename = "rawIspInfo", default)]
    pub raw_isp_info: serde_json::Value,
    /// Optional approximate IP-derived location supplied by newer backends.
    #[serde(
        rename = "location",
        default,
        deserialize_with = "deserialize_optional_location",
        skip_serializing_if = "Option::is_none"
    )]
    pub location: Option<Location>,
}

impl GetIPResult {
    /// The typed view of rawIspInfo. Anything that is not an object -- an
    /// empty string, null, or an absent field -- reads as no ISP information,
    /// which is also what the Go client settles on.
    pub fn ip_info(&self) -> IPInfoResponse {
        // as_name is an input alias for org, not a report field: it is read
        // here and never serialized back out.
        #[derive(Deserialize, Default)]
        struct Raw {
            #[serde(flatten)]
            info: IPInfoResponse,
            #[serde(rename = "as_name", default)]
            as_name: String,
        }

        let raw: Raw = serde_json::from_value(self.raw_isp_info.clone()).unwrap_or_default();
        let mut info = raw.info;

        // Current backends use as_name, while older ones use org.
        if info.organization.is_empty() {
            info.organization = raw.as_name;
        }

        info
    }

    /// The client address this measurement was made from.
    ///
    /// Taken from rawIspInfo when present, but a backend with ISP detection
    /// disabled puts the address only in processedString -- bare, or as the
    /// leading token of `<address> - <ISP>` -- so the first token stands in
    /// when it parses as an address. The report carries this field, and it is
    /// what lets a consumer tell an IPv4 measurement from an IPv6 one.
    pub fn ip(&self) -> String {
        let from_raw = self.ip_info().ip;
        if !from_raw.is_empty() {
            return from_raw;
        }

        match self.processed_string.split_whitespace().next() {
            Some(tok) if tok.parse::<std::net::IpAddr>().is_ok() => tok.to_string(),
            _ => String::new(),
        }
    }

    /// The structured approximate location to display or report.
    ///
    /// A top-level location object takes precedence. Older backends only expose
    /// city, region, country and `loc` inside rawIspInfo, so those fields remain
    /// usable as a legacy fallback instead of being discarded.
    pub fn approximate_location(&self) -> Option<Location> {
        if let Some(location) = &self.location {
            let location = location.normalized();
            if !location.is_empty() {
                return Some(location);
            }
        }

        let location = if let serde_json::Value::Object(raw) = &self.raw_isp_info {
            Location::from_value(&serde_json::Value::Object(raw.clone()))
        } else {
            Location::from_ip_info(&self.ip_info())
        };

        if location.is_empty() {
            None
        } else {
            Some(location)
        }
    }
}

/// Approximate IP-derived location metadata.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Location {
    pub city: String,
    pub region: String,
    pub country: String,
    pub country_name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

impl Location {
    fn from_value(value: &serde_json::Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::default();
        };

        let nested_location = object
            .get("location")
            .and_then(serde_json::Value::as_object);
        let string_from = |source: &serde_json::Map<String, serde_json::Value>, key: &str| {
            sanitize(
                source
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            )
            .trim()
            .to_string()
        };
        let string = |key: &str| {
            let top = string_from(object, key);
            if top.is_empty() {
                nested_location
                    .map(|source| string_from(source, key))
                    .unwrap_or_default()
            } else {
                top
            }
        };

        let raw_country = string("country");
        let country_name_explicit = ["countryName", "country_name"]
            .into_iter()
            .filter_map(|key| {
                let top = string_from(object, key);
                if top.is_empty() {
                    nested_location
                        .map(|source| string_from(source, key))
                        .filter(|value| !value.is_empty())
                } else {
                    Some(top)
                }
            })
            .find(|value| !value.is_empty())
            .unwrap_or_default();
        let country_code_alias = [
            "countryCode",
            "country_code",
            "iso_code",
            "country_iso_code",
        ]
        .into_iter()
        .filter_map(|key| {
            let top = string_from(object, key);
            if top.is_empty() {
                nested_location
                    .map(|source| string_from(source, key))
                    .filter(|value| !value.is_empty())
            } else {
                Some(top)
            }
        })
        .find(|value| !value.is_empty())
        .unwrap_or_default();
        let country = if !country_code_alias.is_empty() {
            country_code_alias
        } else if looks_like_country_code(&raw_country) {
            raw_country.clone()
        } else {
            String::new()
        };
        let country_name = if !country_name_explicit.is_empty() {
            country_name_explicit
        } else if !raw_country.is_empty() && !looks_like_country_code(&raw_country) {
            raw_country
        } else {
            String::new()
        };
        let (latitude, longitude) = parse_location_coordinates(object);

        Self {
            city: string("city"),
            region: string("region"),
            country,
            country_name,
            latitude,
            longitude,
        }
    }

    pub(crate) fn from_ip_info(info: &IPInfoResponse) -> Self {
        let (latitude, longitude) = parse_location_string(&info.location);
        Self {
            city: info.city.clone(),
            region: info.region.clone(),
            country: info.country.clone(),
            country_name: String::new(),
            latitude,
            longitude,
        }
    }

    pub fn is_empty(&self) -> bool {
        sanitize(&self.city).trim().is_empty()
            && sanitize(&self.region).trim().is_empty()
            && sanitize(&self.country).trim().is_empty()
            && sanitize(&self.country_name).trim().is_empty()
            && self.latitude.is_none()
            && self.longitude.is_none()
    }

    fn normalized(&self) -> Self {
        Self {
            city: sanitize(&self.city).trim().to_string(),
            region: sanitize(&self.region).trim().to_string(),
            country: sanitize(&self.country).trim().to_string(),
            country_name: sanitize(&self.country_name).trim().to_string(),
            latitude: self.valid_latitude(),
            longitude: self.valid_longitude(),
        }
    }

    fn valid_latitude(&self) -> Option<f64> {
        self.latitude
            .filter(|value| value.is_finite() && (-90.0..=90.0).contains(value))
    }

    fn valid_longitude(&self) -> Option<f64> {
        self.longitude
            .filter(|value| value.is_finite() && (-180.0..=180.0).contains(value))
    }

    /// Formats the approximate location for terminal/report text.
    pub fn human_readable(&self) -> Option<String> {
        let mut place = Vec::new();
        let country_name = sanitize(&self.country_name).trim().to_string();
        let country_code = sanitize(&self.country).trim().to_string();
        let country = if country_name.is_empty() {
            country_code.as_str()
        } else {
            country_name.as_str()
        };

        for value in [self.city.as_str(), self.region.as_str(), country] {
            let value = sanitize(value).trim().to_string();
            if !value.is_empty() && !place.iter().any(|existing: &String| existing == &value) {
                place.push(value);
            }
        }

        let coordinates = match (self.valid_latitude(), self.valid_longitude()) {
            (Some(latitude), Some(longitude)) => Some(format!(
                "latitude {}, longitude {}",
                format_coordinate(latitude),
                format_coordinate(longitude)
            )),
            (Some(latitude), None) => Some(format!("latitude {}", format_coordinate(latitude))),
            (None, Some(longitude)) => Some(format!("longitude {}", format_coordinate(longitude))),
            _ => None,
        };

        match (place.join(", "), coordinates) {
            (place, Some(coordinates)) if !place.is_empty() => {
                Some(format!("{place} ({coordinates})"))
            }
            (place, None) if !place.is_empty() => Some(place),
            (place, Some(coordinates)) if place.is_empty() => Some(coordinates),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Location {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::from_value(&value))
    }
}

impl Serialize for Location {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let city = sanitize(&self.city);
        let region = sanitize(&self.region);
        let country = sanitize(&self.country);
        let country_name = sanitize(&self.country_name);
        let latitude = self.valid_latitude();
        let longitude = self.valid_longitude();

        let mut fields = 0usize;
        for value in [&city, &region, &country, &country_name] {
            if !value.is_empty() {
                fields += 1;
            }
        }
        if latitude.is_some() {
            fields += 1;
        }
        if longitude.is_some() {
            fields += 1;
        }

        let mut map = serializer.serialize_map(Some(fields))?;
        if !city.is_empty() {
            map.serialize_entry("city", &city)?;
        }
        if !region.is_empty() {
            map.serialize_entry("region", &region)?;
        }
        if !country.is_empty() {
            map.serialize_entry("country", &country)?;
        }
        if !country_name.is_empty() {
            map.serialize_entry("countryName", &country_name)?;
        }
        if let Some(latitude) = latitude {
            map.serialize_entry("latitude", &latitude)?;
        }
        if let Some(longitude) = longitude {
            map.serialize_entry("longitude", &longitude)?;
        }
        map.end()
    }
}

fn deserialize_optional_location<'de, D>(deserializer: D) -> Result<Option<Location>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(value) if value.is_object() => {
            let location = Location::from_value(&value);
            Ok((!location.is_empty()).then_some(location))
        }
        _ => Ok(None),
    }
}

fn parse_valid_coordinate(
    value: Option<&serde_json::Value>,
    minimum: f64,
    maximum: f64,
) -> Option<f64> {
    let parsed = match value {
        Some(serde_json::Value::Number(number)) => number.as_f64(),
        Some(serde_json::Value::String(value)) => value.trim().parse::<f64>().ok(),
        _ => None,
    }?;

    (parsed.is_finite() && (minimum..=maximum).contains(&parsed)).then_some(parsed)
}

fn looks_like_country_code(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic())
}

fn first_valid_coordinate(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
    minimum: f64,
    maximum: f64,
) -> Option<f64> {
    keys.iter()
        .find_map(|key| parse_valid_coordinate(object.get(*key), minimum, maximum))
}

fn parse_location_coordinates(
    object: &serde_json::Map<String, serde_json::Value>,
) -> (Option<f64>, Option<f64>) {
    let mut latitude = first_valid_coordinate(object, &["latitude", "lat"], -90.0, 90.0);
    let mut longitude = first_valid_coordinate(object, &["longitude", "lon"], -180.0, 180.0);

    for container in ["coordinates", "location"] {
        if let Some(nested) = object.get(container).and_then(serde_json::Value::as_object) {
            if latitude.is_none() {
                latitude = first_valid_coordinate(nested, &["latitude", "lat"], -90.0, 90.0);
            }
            if longitude.is_none() {
                longitude = first_valid_coordinate(nested, &["longitude", "lon"], -180.0, 180.0);
            }
            if latitude.is_none() || longitude.is_none() {
                if let Some(loc) = nested.get("loc").and_then(serde_json::Value::as_str) {
                    let (loc_latitude, loc_longitude) = parse_location_string(loc);
                    if latitude.is_none() {
                        latitude = loc_latitude;
                    }
                    if longitude.is_none() {
                        longitude = loc_longitude;
                    }
                }
            }
        }
    }

    for key in ["loc", "location"] {
        if let Some(loc) = object.get(key).and_then(serde_json::Value::as_str) {
            let (loc_latitude, loc_longitude) = parse_location_string(loc);
            if latitude.is_none() {
                latitude = loc_latitude;
            }
            if longitude.is_none() {
                longitude = loc_longitude;
            }
        }
    }

    (latitude, longitude)
}

fn parse_location_string(value: &str) -> (Option<f64>, Option<f64>) {
    let mut coordinates = value.split(',');
    let latitude = coordinates
        .next()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && (-90.0..=90.0).contains(value));
    let longitude = coordinates
        .next()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && (-180.0..=180.0).contains(value));
    (latitude, longitude)
}

fn format_coordinate(value: f64) -> String {
    if value == 0.0 {
        "0".to_string()
    } else {
        value.to_string()
    }
}

/// The JSON returned by IPInfo.io's API.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IPInfoResponse {
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub country: String,
    #[serde(rename = "loc", default)]
    pub location: String,
    #[serde(rename = "org", default)]
    pub organization: String,
    #[serde(default)]
    pub postal: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub readme: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_names_the_program_and_the_platform() {
        let ua = user_agent();
        assert!(
            ua.starts_with(&format!("{UA_NAME}/{PROG_VERSION} (")),
            "unexpected prefix: {ua}"
        );
        assert!(ua.ends_with(')'), "unterminated platform: {ua}");
        assert!(
            ua.contains(std::env::consts::OS),
            "no operating system: {ua}"
        );
        assert!(ua.contains(std::env::consts::ARCH), "no architecture: {ua}");
    }

    // The Go client sends `librespeed-cli/...`, and this binary installs under
    // that same name, so a server telling the two implementations apart in its
    // telemetry has only this prefix to go on. That makes it part of the wire
    // contract rather than a cosmetic choice.
    #[test]
    fn user_agent_stays_distinguishable_from_the_go_client() {
        assert!(
            user_agent().starts_with("librespeed-cli-rust/"),
            "got {}",
            user_agent()
        );
    }

    // A backend with ISP detection disabled answers with an empty string here
    // rather than an object, which is a different JSON type than the field
    // declares.
    #[test]
    fn get_ip_result_tolerates_empty_raw_isp_info() {
        let json = r#"{"processedString":"192.0.2.1","rawIspInfo":""}"#;
        let parsed: Result<GetIPResult, _> = serde_json::from_str(json);
        assert!(parsed.is_ok(), "rejected: {:?}", parsed.err());
        assert_eq!(parsed.unwrap().processed_string, "192.0.2.1");
    }

    #[test]
    fn get_ip_result_keeps_processed_string_whatever_raw_isp_info_holds() {
        for raw in ["\"\"", "123", "null", "[]", "\"unavailable\""] {
            let json = format!(r#"{{"processedString":"192.0.2.1","rawIspInfo":{raw}}}"#);
            let parsed: Result<GetIPResult, _> = serde_json::from_str(&json);
            assert!(parsed.is_ok(), "rejected {raw}: {:?}", parsed.err());
            assert_eq!(parsed.unwrap().processed_string, "192.0.2.1", "for {raw}");
        }
    }

    #[test]
    fn get_ip_result_still_reads_a_real_object() {
        let json = r#"{"processedString":"x","rawIspInfo":{"ip":"192.0.2.1","country":"CZ"}}"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.ip_info().ip, "192.0.2.1");
        assert_eq!(parsed.ip_info().country, "CZ");
        assert_eq!(parsed.ip(), "192.0.2.1");
    }

    #[test]
    fn get_ip_result_reads_structured_location() {
        let json = r#"{
            "processedString":"203.0.113.7 - Example ISP",
            "rawIspInfo":{"ip":"203.0.113.7","org":"AS64496 Example ISP"},
            "location":{
                "city":"Boydton",
                "region":"Virginia",
                "country":"US",
                "countryName":"United States",
                "latitude":36.6676,
                "longitude":-78.3875
            }
        }"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        let location = parsed.location.as_ref().expect("top-level location");
        assert_eq!(location.city, "Boydton");
        assert_eq!(location.region, "Virginia");
        assert_eq!(location.country, "US");
        assert_eq!(location.country_name, "United States");
        assert_eq!(location.latitude, Some(36.6676));
        assert_eq!(location.longitude, Some(-78.3875));
        assert_eq!(
            parsed
                .approximate_location()
                .and_then(|value| value.human_readable()),
            Some(
                "Boydton, Virginia, United States (latitude 36.6676, longitude -78.3875)"
                    .to_string()
            )
        );
    }

    #[test]
    fn structured_location_accepts_numeric_strings_but_rejects_bad_coordinates() {
        let json = r#"{
            "location":{
                "city":"Numeric City",
                "latitude":"36.6676",
                "longitude":"-78.3875"
            }
        }"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        let location = parsed.location.expect("string coordinates");
        assert_eq!(location.latitude, Some(36.6676));
        assert_eq!(location.longitude, Some(-78.3875));

        for (latitude, longitude, want_latitude, want_longitude) in [
            ("91", "-78", None, Some(-78.0)),
            ("36", "181", Some(36.0), None),
            ("NaN", "Infinity", None, None),
            ("null", "null", None, None),
        ] {
            let json = format!(
                r#"{{"location":{{"city":"Partial City","latitude":"{latitude}","longitude":"{longitude}"}}}}"#
            );
            let parsed: GetIPResult = serde_json::from_str(&json).expect("should parse");
            let location = parsed.location.expect("valid text should remain");
            assert_eq!(location.latitude, want_latitude, "for latitude {latitude}");
            assert_eq!(
                location.longitude, want_longitude,
                "for longitude {longitude}"
            );
        }
    }

    #[test]
    fn structured_location_accepts_coordinate_aliases_and_fallbacks() {
        for json in [
            r#"{"location":{"city":"Boydton","latitude":91,"longitude":181,"lat":"36.6676","lon":"-78.3875","loc":"37,-79"}}"#,
            r#"{"location":{"city":"Boydton","coordinates":{"latitude":"36.6676","longitude":"-78.3875"}}}"#,
            r#"{"location":{"city":"Boydton","loc":"36.6676,-78.3875"}}"#,
            r#"{"location":{"city":"Boydton","location":{"lat":36.6676,"lon":-78.3875}}}"#,
        ] {
            let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
            let location = parsed.location.expect("coordinate fallback");
            assert_eq!(location.latitude, Some(36.6676), "for {json}");
            assert_eq!(location.longitude, Some(-78.3875), "for {json}");
        }
    }

    #[test]
    fn partial_location_place_and_coordinate_metadata_remains_usable() {
        for (json, want) in [
            (
                r#"{"location":{"city":"Boydton","latitude":36.6676}}"#,
                "Boydton (latitude 36.6676)",
            ),
            (
                r#"{"location":{"country":"US","longitude":-78.3875}}"#,
                "US (longitude -78.3875)",
            ),
            (r#"{"location":{"latitude":36.6676}}"#, "latitude 36.6676"),
            (
                r#"{"location":{"longitude":-78.3875}}"#,
                "longitude -78.3875",
            ),
        ] {
            let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
            let location = parsed.location.expect("partial location");
            assert_eq!(
                location.human_readable(),
                Some(want.to_string()),
                "for {json}"
            );
        }
    }

    #[test]
    fn get_ip_result_tolerates_missing_and_malformed_location() {
        for location in ["null", "123", "\"Boydton\"", "[]", "{}"] {
            let json = format!(r#"{{"processedString":"x","location":{location}}}"#);
            let parsed: GetIPResult = serde_json::from_str(&json).expect("should parse");
            assert!(
                parsed.location.is_none(),
                "malformed location accepted: {location}"
            );
        }

        let parsed: GetIPResult =
            serde_json::from_str(r#"{"processedString":"x"}"#).expect("should parse");
        assert!(parsed.location.is_none());
    }

    #[test]
    fn invisible_only_top_city_falls_back_to_legacy_location() {
        let json = r#"{
            "processedString":"203.0.113.7 - Example ISP",
            "rawIspInfo":{
                "ip":"203.0.113.7",
                "city":"Legacy City",
                "region":"Legacy Region",
                "country":"US",
                "loc":"36.25,-78.25"
            },
            "location":{"city":"\u200b\u2060"}
        }"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        assert!(
            parsed.location.is_none(),
            "invisible-only city was retained"
        );

        let location = parsed.approximate_location().expect("legacy location");
        assert_eq!(location.city, "Legacy City");
        assert_eq!(location.region, "Legacy Region");
        assert_eq!(location.country, "US");
        assert_eq!(
            location.human_readable(),
            Some("Legacy City, Legacy Region, US (latitude 36.25, longitude -78.25)".to_string())
        );
    }

    #[test]
    fn invisible_country_name_falls_back_to_usable_country_code() {
        for country_key in ["country", "country_code", "countryCode"] {
            let json = format!(
                r#"{{"location":{{"city":"Boydton","countryName":"\u200b\u202e","{country_key}":"US"}}}}"#
            );
            let parsed: GetIPResult = serde_json::from_str(&json).expect("should parse");
            let location = parsed.location.expect("usable country code");
            assert_eq!(
                location.human_readable(),
                Some("Boydton, US".to_string()),
                "for {country_key}"
            );

            let serialized = serde_json::to_value(&location).expect("serialize");
            assert_eq!(serialized["country"], "US", "for {country_key}");
            assert!(serialized.get("countryName").is_none(), "for {country_key}");
        }
    }

    #[test]
    fn legacy_raw_isp_info_remains_available_as_location_fallback() {
        let json = r#"{
            "processedString":"203.0.113.7 - Example ISP",
            "rawIspInfo":{
                "ip":"203.0.113.7",
                "city":"Testville",
                "region":"Testshire",
                "country":"XX",
                "countryName":"Test Country",
                "loc":"36.25,-78.25",
                "org":"AS64496 Example ISP"
            }
        }"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        let location = parsed.approximate_location().expect("legacy location");
        assert_eq!(location.city, "Testville");
        assert_eq!(location.region, "Testshire");
        assert_eq!(location.country, "XX");
        assert_eq!(location.country_name, "Test Country");
        assert_eq!(location.latitude, Some(36.25));
        assert_eq!(location.longitude, Some(-78.25));
        assert_eq!(parsed.ip_info().organization, "AS64496 Example ISP");
    }

    #[test]
    fn structured_location_output_is_sanitized() {
        let location = Location {
            city: "C\u{200b}ity\u{202e}".into(),
            region: "R\u{061c}egion".into(),
            country: "X\u{00ad}X".into(),
            country_name: "Evil\u{202e}Country".into(),
            latitude: Some(36.6676),
            longitude: Some(-78.3875),
        };

        let serialized = serde_json::to_string(&location).expect("serialize");
        assert_eq!(
            serialized,
            r#"{"city":"City","region":"Region","country":"XX","countryName":"EvilCountry","latitude":36.6676,"longitude":-78.3875}"#
        );
        assert_eq!(
            location.human_readable(),
            Some("City, Region, EvilCountry (latitude 36.6676, longitude -78.3875)".to_string())
        );
    }

    // The address lives only in processedString whenever rawIspInfo carries
    // none: live backends answer with an empty string, a JSON null, or -- with
    // the current ipinfo schema -- an object that has no ip field at all.
    #[test]
    fn client_address_recovered_from_processed_string() {
        for raw in [
            r#""""#,
            "null",
            r#"{"as_name":"O2 Czech Republic, a.s.","asn":"AS5610"}"#,
        ] {
            for (processed, want) in [
                (
                    "2a00:1028:8388:a84e:8cda:7223:57f8:67a6",
                    "2a00:1028:8388:a84e:8cda:7223:57f8:67a6",
                ),
                ("192.0.2.1 - Example ISP, CZ", "192.0.2.1"),
                ("no address here", ""),
                ("", ""),
            ] {
                let json = format!(r#"{{"processedString":"{processed}","rawIspInfo":{raw}}}"#);
                let parsed: GetIPResult = serde_json::from_str(&json).expect("should parse");
                assert_eq!(parsed.ip(), want, "for {processed:?} with raw {raw}");
            }
        }
    }

    // as_name is an input alias for org, not a report field: accepting it
    // must not start emitting it.
    #[test]
    fn ip_info_does_not_leak_as_name() {
        let json = r#"{"processedString":"x","rawIspInfo":{"as_name":"O2 Czech Republic, a.s."}}"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        let out = serde_json::to_string(&parsed.ip_info()).expect("serialize");
        assert!(!out.contains("as_name"), "leaked: {out}");
        assert!(
            out.contains(r#""org":"O2 Czech Republic, a.s.""#),
            "org missing: {out}"
        );
    }

    // org takes precedence over as_name; a backend sending only the current
    // ipinfo schema still yields a filled organisation.
    #[test]
    fn organisation_recovered_from_as_name() {
        for (raw, want) in [
            (
                r#"{"ip":"192.0.2.1","org":"AS64496 Example"}"#,
                "AS64496 Example",
            ),
            (
                r#"{"as_name":"O2 Czech Republic, a.s.","asn":"AS5610"}"#,
                "O2 Czech Republic, a.s.",
            ),
            (r#"{"org":"Old Name","as_name":"New Name"}"#, "Old Name"),
            (r#"{"ip":"192.0.2.1"}"#, ""),
        ] {
            let json = format!(r#"{{"processedString":"x","rawIspInfo":{raw}}}"#);
            let parsed: GetIPResult = serde_json::from_str(&json).expect("should parse");
            assert_eq!(parsed.ip_info().organization, want, "for {raw}");
        }
    }

    // rawIspInfo.ip wins over whatever processedString starts with: the typed
    // field is the backend's own answer, the token is only a stand-in.
    #[test]
    fn raw_address_takes_precedence_over_processed_string() {
        let json = r#"{"processedString":"198.51.100.7 - X","rawIspInfo":{"ip":"192.0.2.1"}}"#;
        let parsed: GetIPResult = serde_json::from_str(json).expect("should parse");
        assert_eq!(parsed.ip(), "192.0.2.1");
    }

    // Telemetry sends the getIP document back to the server; what the backend
    // said must survive the round trip unchanged, not come back re-typed.
    #[test]
    fn raw_isp_info_round_trips_verbatim() {
        for raw in [r#""""#, "null", r#"{"ip":"192.0.2.1"}"#, r#""unavailable""#] {
            let json = format!(r#"{{"processedString":"x","rawIspInfo":{raw}}}"#);
            let parsed: GetIPResult = serde_json::from_str(&json).expect("should parse");
            let back = serde_json::to_value(&parsed).expect("should serialize");
            let orig: serde_json::Value = serde_json::from_str(&json).expect("valid");
            assert_eq!(back["rawIspInfo"], orig["rawIspInfo"], "for {raw}");
        }
    }
}
