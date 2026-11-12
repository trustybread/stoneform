use std::fmt;
use std::str::FromStr;

/// A Battle.net play region. China is out of scope for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    Us,
    Eu,
    Kr,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown region {0:?}; expected us, eu or kr")]
pub struct ParseRegionError(String);

impl Region {
    pub const ALL: [Region; 3] = [Region::Us, Region::Eu, Region::Kr];

    /// Lower-case code, as used in Blizzard's host names (`eu.battle.net`).
    pub fn as_str(self) -> &'static str {
        match self {
            Region::Us => "us",
            Region::Eu => "eu",
            Region::Kr => "kr",
        }
    }

    /// The value the game reads from `Launch Options/<codename>/REGION`.
    pub fn launch_option_value(self) -> &'static str {
        match self {
            Region::Us => "US",
            Region::Eu => "EU",
            Region::Kr => "KR",
        }
    }

    /// A sensible default for a Mac set to `country` (ISO 3166-1 alpha-2).
    ///
    /// Europe, the Middle East and Africa play in EU; Korea and Taiwan in KR;
    /// everyone else, including unknown countries, in US. The user can change
    /// it; this only picks the first guess.
    pub fn for_country(country: &str) -> Region {
        #[rustfmt::skip]
        const EU: &[&str] = &[
            // Europe
            "AD", "AL", "AM", "AT", "AX", "AZ", "BA", "BE", "BG", "BY", "CH", "CY", "CZ", "DE", "DK",
            "EE", "ES", "FI", "FO", "FR", "GB", "GE", "GG", "GI", "GL", "GR", "HR", "HU", "IE", "IM",
            "IS", "IT", "JE", "KZ", "LI", "LT", "LU", "LV", "MC", "MD", "ME", "MK", "MT", "NL", "NO",
            "PL", "PT", "RO", "RS", "RU", "SE", "SI", "SJ", "SK", "SM", "TR", "UA", "VA", "XK",
            // Middle East
            "AE", "BH", "EG", "IL", "IQ", "JO", "KW", "LB", "OM", "PS", "QA", "SA", "SY", "YE",
            // Africa
            "DZ", "GH", "KE", "MA", "NG", "TN", "ZA",
        ];
        let country = country.to_ascii_uppercase();
        match country.as_str() {
            "KR" | "TW" => Region::Kr,
            code if EU.contains(&code) => Region::Eu,
            _ => Region::Us,
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Region {
    type Err = ParseRegionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Region::ALL
            .into_iter()
            .find(|region| region.as_str().eq_ignore_ascii_case(value))
            .ok_or_else(|| ParseRegionError(value.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_its_code() {
        for region in Region::ALL {
            assert_eq!(region.as_str().parse::<Region>(), Ok(region));
        }
        assert_eq!("EU".parse::<Region>(), Ok(Region::Eu));
        assert!("cn".parse::<Region>().is_err());
    }

    #[test]
    fn guesses_a_region_from_the_country() {
        assert_eq!(Region::for_country("de"), Region::Eu);
        assert_eq!(Region::for_country("GB"), Region::Eu);
        assert_eq!(Region::for_country("KR"), Region::Kr);
        assert_eq!(Region::for_country("TW"), Region::Kr);
        assert_eq!(Region::for_country("US"), Region::Us);
        assert_eq!(Region::for_country("AU"), Region::Us);
        assert_eq!(Region::for_country(""), Region::Us);
    }

    #[test]
    fn writes_the_game_s_region_values() {
        assert_eq!(Region::Eu.launch_option_value(), "EU");
        assert_eq!(Region::Us.launch_option_value(), "US");
    }
}
